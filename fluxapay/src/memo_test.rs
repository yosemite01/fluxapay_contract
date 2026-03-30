use crate::{
    access_control::role_merchant, PaymentProcessor, PaymentProcessorClient, PaymentStatus,
};
use soroban_sdk::{
    testutils::Address as _, testutils::BytesN as _, Address, BytesN, Env, String, Symbol,
};

fn setup_payment_processor(env: &Env) -> (Address, PaymentProcessorClient<'_>) {
    let contract_id = env.register(PaymentProcessor, ());
    let client = PaymentProcessorClient::new(env, &contract_id);
    let admin = Address::generate(env);
    client.initialize_payment_processor(&admin);
    (admin, client)
}

#[test]
fn test_create_payment_with_memo() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "payment_with_memo");
    let merchant_id = Address::generate(&env);
    let amount = 1000i128;
    let memo = Some(String::from_str(&env, "ORDER-12345"));
    let memo_type = Some(String::from_str(&env, "Text"));
    let expires_at = env.ledger().timestamp() + 3600;

    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let payment = client.create_payment(
        &payment_id,
        &merchant_id,
        &amount,
        &Symbol::new(&env, "USDC"),
        &Address::generate(&env),
        &expires_at,
        &memo,
        &memo_type,
    );

    assert_eq!(payment.payment_id, payment_id);
    assert_eq!(payment.memo, Some(String::from_str(&env, "ORDER-12345")));
    assert_eq!(payment.memo_type, Some(String::from_str(&env, "Text")));
}

#[test]
fn test_create_payment_without_memo() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "payment_no_memo");
    let merchant_id = Address::generate(&env);
    let amount = 1000i128;
    let expires_at = env.ledger().timestamp() + 3600;

    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let payment = client.create_payment(
        &payment_id,
        &merchant_id,
        &amount,
        &Symbol::new(&env, "USDC"),
        &Address::generate(&env),
        &expires_at,
        &None::<String>,
        &None::<String>,
    );

    assert_eq!(payment.payment_id, payment_id);
    assert_eq!(payment.memo, None);
    assert_eq!(payment.memo_type, None);
}

#[test]
fn test_create_payment_with_id_memo() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "payment_id_memo");
    let merchant_id = Address::generate(&env);
    let amount = 2000i128;
    let memo = Some(String::from_str(&env, "123456789"));
    let memo_type = Some(String::from_str(&env, "Id"));
    let expires_at = env.ledger().timestamp() + 3600;

    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let payment = client.create_payment(
        &payment_id,
        &merchant_id,
        &amount,
        &Symbol::new(&env, "USDC"),
        &Address::generate(&env),
        &expires_at,
        &memo,
        &memo_type,
    );

    assert_eq!(payment.memo, Some(String::from_str(&env, "123456789")));
    assert_eq!(payment.memo_type, Some(String::from_str(&env, "Id")));
}

#[test]
fn test_create_payment_with_hash_memo() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "payment_hash_memo");
    let merchant_id = Address::generate(&env);
    let amount = 3000i128;
    let memo = Some(String::from_str(&env, "abcdef1234567890"));
    let memo_type = Some(String::from_str(&env, "Hash"));
    let expires_at = env.ledger().timestamp() + 3600;

    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let payment = client.create_payment(
        &payment_id,
        &merchant_id,
        &amount,
        &Symbol::new(&env, "USDC"),
        &Address::generate(&env),
        &expires_at,
        &memo,
        &memo_type,
    );

    assert_eq!(
        payment.memo,
        Some(String::from_str(&env, "abcdef1234567890"))
    );
    assert_eq!(payment.memo_type, Some(String::from_str(&env, "Hash")));
}

#[test]
fn test_memo_persists_after_verification() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "payment_memo_persist");
    let merchant_id = Address::generate(&env);
    let amount = 1500i128;
    let memo = Some(String::from_str(&env, "INVOICE-999"));
    let memo_type = Some(String::from_str(&env, "Text"));
    let expires_at = env.ledger().timestamp() + 3600;

    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    client.create_payment(
        &payment_id,
        &merchant_id,
        &amount,
        &Symbol::new(&env, "USDC"),
        &Address::generate(&env),
        &expires_at,
        &memo,
        &memo_type,
    );

    // Verify payment
    let oracle = Address::generate(&env);
    client.grant_role(&admin, &crate::access_control::role_oracle(&env), &oracle);
    client.verify_payment(
        &oracle,
        &payment_id,
        &BytesN::<32>::random(&env),
        &Address::generate(&env),
        &amount,
    );

    // Check memo persists after verification
    let payment = client.get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Confirmed);
    assert_eq!(payment.memo, Some(String::from_str(&env, "INVOICE-999")));
    assert_eq!(payment.memo_type, Some(String::from_str(&env, "Text")));
}
