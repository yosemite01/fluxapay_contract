use crate::{
    access_control::{role_merchant, role_oracle},
    PaymentProcessor, PaymentProcessorClient, PaymentStatus,
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
fn test_pause_and_unpause() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    // Initially not paused
    assert!(!client.is_paused());

    // Pause the contract
    client.set_paused(&admin, &true);
    assert!(client.is_paused());

    // Unpause the contract
    client.set_paused(&admin, &false);
    assert!(!client.is_paused());
}

#[test]
#[should_panic(expected = "Error(Contract, #17)")]
fn test_create_payment_when_paused() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    // Pause the contract
    client.set_paused(&admin, &true);

    // Try to create payment - should fail
    let payment_id = String::from_str(&env, "payment_paused");
    client.create_payment(
        &payment_id,
        &merchant_id,
        &1000i128,
        &Symbol::new(&env, "USDC"),
        &Address::generate(&env),
        &(env.ledger().timestamp() + 3600),
        &None::<String>,
        &None::<String>,
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #17)")]
fn test_verify_payment_when_paused() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    let payment_id = String::from_str(&env, "payment_verify_paused");
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    // Create payment while not paused
    client.create_payment(
        &payment_id,
        &merchant_id,
        &1000i128,
        &Symbol::new(&env, "USDC"),
        &Address::generate(&env),
        &(env.ledger().timestamp() + 3600),
        &None::<String>,
        &None::<String>,
    );

    // Pause the contract
    client.set_paused(&admin, &true);

    // Try to verify payment - should fail
    let oracle = Address::generate(&env);
    client.grant_role(&admin, &role_oracle(&env), &oracle);
    client.verify_payment(
        &oracle,
        &payment_id,
        &BytesN::<32>::random(&env),
        &Address::generate(&env),
        &1000i128,
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #17)")]
fn test_cancel_payment_when_paused() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    let payment_id = String::from_str(&env, "payment_cancel_paused");
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    // Create payment while not paused
    client.create_payment(
        &payment_id,
        &merchant_id,
        &1000i128,
        &Symbol::new(&env, "USDC"),
        &Address::generate(&env),
        &(env.ledger().timestamp() + 3600),
        &None::<String>,
        &None::<String>,
    );

    // Pause the contract
    client.set_paused(&admin, &true);

    // Try to cancel payment - should fail
    client.cancel_payment(&merchant_id, &payment_id);
}

#[test]
fn test_create_payment_after_unpause() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    // Pause and then unpause
    client.set_paused(&admin, &true);
    client.set_paused(&admin, &false);

    // Create payment should succeed
    let payment_id = String::from_str(&env, "payment_after_unpause");
    let payment = client.create_payment(
        &payment_id,
        &merchant_id,
        &1000i128,
        &Symbol::new(&env, "USDC"),
        &Address::generate(&env),
        &(env.ledger().timestamp() + 3600),
        &None::<String>,
        &None::<String>,
    );

    assert_eq!(payment.status, PaymentStatus::Pending);
}

#[test]
#[should_panic(expected = "Error(Contract, #1)")]
fn test_set_paused_unauthorized() {
    let env = Env::default();
    env.mock_all_auths();
    let (_admin, client) = setup_payment_processor(&env);

    let non_admin = Address::generate(&env);

    // Try to pause as non-admin - should fail
    client.set_paused(&non_admin, &true);
}
