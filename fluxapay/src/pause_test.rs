#![cfg(test)]

use crate::{
    CreatePaymentArgs, PaymentProcessorClient, PauseInfo, PauseState,
};
use soroban_sdk::{testutils::Address as _, Address, Env, String, Symbol};

#[test]
fn test_pause_initial_state() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let processor_id = env.register(crate::PaymentProcessor, ());
    let client = PaymentProcessorClient::new(&env, &processor_id);

    client.initialize_payment_processor(&admin);

    let info = client.get_pause_info();
    assert!(!info.global.paused);
    assert!(!info.creation.paused);
    assert_eq!(info.global.reason, String::from_str(&env, ""));
    assert_eq!(info.creation.reason, String::from_str(&env, ""));
}

#[test]
fn test_global_pause_blocks_creation() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    let processor_id = env.register(crate::PaymentProcessor, ());
    let client = PaymentProcessorClient::new(&env, &processor_id);

    client.initialize_payment_processor(&admin);
    
    // Grant merchant role
    client.grant_role(&admin, &Symbol::new(&env, "MERCHANT"), &merchant);

    // Set global pause
    let reason = String::from_str(&env, "Global maintenance");
    client.set_global_pause(&admin, &true, &reason);

    let info = client.get_pause_info();
    assert!(info.global.paused);
    assert_eq!(info.global.reason, reason);
    assert_eq!(info.global.admin, Some(admin.clone()));

    // Try to create payment
    let res = client.try_create_payment(
        &CreatePaymentArgs {
            payment_id: String::from_str(&env, "p1"),
            merchant_id: merchant.clone(),
            amount: 100,
            currency: Symbol::new(&env, "USDC"),
            deposit_address: Address::generate(&env),
            expires_at: None,
            duration_secs: None,
            memo: None,
            memo_type: None,
            token_address: None,
            client_token: None,
        }
    );

    assert!(res.is_err());
}

#[test]
fn test_creation_pause_blocks_only_creation() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    let oracle = Address::generate(&env);
    let processor_id = env.register(crate::PaymentProcessor, ());
    let client = PaymentProcessorClient::new(&env, &processor_id);

    client.initialize_payment_processor(&admin);
    
    client.grant_role(&admin, &Symbol::new(&env, "MERCHANT"), &merchant);
    client.grant_role(&admin, &Symbol::new(&env, "ORACLE"), &oracle);

    // Set creation pause
    let reason = String::from_str(&env, "High load");
    client.set_creation_pause(&admin, &true, &reason);

    let info = client.get_pause_info();
    assert!(!info.global.paused);
    assert!(info.creation.paused);
    assert_eq!(info.creation.reason, reason);

    // create_payment should fail
    let res = client.try_create_payment(
        &CreatePaymentArgs {
            payment_id: String::from_str(&env, "p1"),
            merchant_id: merchant.clone(),
            amount: 100,
            currency: Symbol::new(&env, "USDC"),
            deposit_address: Address::generate(&env),
            expires_at: None,
            duration_secs: None,
            memo: None,
            memo_type: None,
            token_address: None,
            client_token: None,
        }
    );
    assert!(res.is_err());

    // verify_payment should still work (won't actually succeed because payment doesn't exist, but won't fail with ContractPaused)
    let res_verify = client.try_verify_payment(
        &oracle,
        &String::from_str(&env, "p1"),
        &soroban_sdk::BytesN::from_array(&env, &[0u8; 32]),
        &Address::generate(&env),
        &100,
    );
    
    // It should fail with PaymentNotFound (404), not ContractPaused (17)
    // We check the error by seeing if it's NOT the pause error
    if let Err(res) = res_verify {
        match res {
            Ok(crate::Error::ContractPaused) => panic!("Should not be blocked by pause"),
            _ => {}
        }
    }
}
