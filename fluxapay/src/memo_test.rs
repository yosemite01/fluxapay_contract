use crate::{
    access_control::role_merchant, CreatePaymentArgs, PaymentProcessor, PaymentProcessorClient, PaymentStatus,
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

fn create_payment_args(
    env: &Env,
    payment_id: &String,
    merchant_id: &Address,
    amount: i128,
) -> CreatePaymentArgs {
    CreatePaymentArgs {
        payment_id: payment_id.clone(),
        merchant_id: merchant_id.clone(),
        amount,
        currency: Symbol::new(env, "USDC"),
        deposit_address: Address::generate(env),
        expires_at: Some(env.ledger().timestamp() + 3600),
        duration_secs: None,
        memo: None,
        memo_type: None,
        token_address: None,
        client_token: None,
    }
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

    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let mut args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    args.memo = memo;
    args.memo_type = memo_type;

    let payment = client.create_payment(&args);

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

    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    let payment = client.create_payment(&args);

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

    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let mut args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    args.memo = memo;
    args.memo_type = memo_type;

    let payment = client.create_payment(&args);

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

    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let mut args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    args.memo = memo;
    args.memo_type = memo_type;

    let payment = client.create_payment(&args);

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

    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let mut args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    args.memo = memo;
    args.memo_type = memo_type;

    client.create_payment(&args);

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
