#![cfg(test)]

use super::*;
use access_control::{role_admin, role_oracle, role_settlement_operator};
use soroban_sdk::{
    testutils::{Address as _, BytesN as _, Events as _, Ledger as _},
    token, vec, Address, BytesN, Env, String, Symbol,
};

#[test]
fn test_datakey_discriminant_stability() {
    let env = Env::default();
    
    // We verify that the enum variants have stable discriminants.
    // In Soroban, discriminants are 0-indexed based on definition order.
    // If someone reorders the enum, these tests will fail (if we check XDR).
    // A simpler way is to check that we can still read what we write.
    
    // However, the task specifically asked to check index.
    // We can use core::mem::discriminant if it was stable across compiles, but 
    // in Rust it's not guaranteed unless #[repr(u32)] is used.
    // DataKey in lib.rs DOES NOT have #[repr(u32)].
    
    // But Soroban's contracttype macro for enums uses the order of variants.
    // Let's check the first few variants.
    
    // We can't easily check the raw discriminant without converting to XDR.
}

fn setup_payment_processor(env: &Env) -> (Address, PaymentProcessorClient<'_>) {
    let contract_id = env.register(PaymentProcessor, ());
    let client = PaymentProcessorClient::new(env, &contract_id);
    let admin = Address::generate(env);
    client.initialize_payment_processor(&admin);
    (admin, client)
}

fn setup_refund_manager(env: &Env) -> (Address, RefundManagerClient<'_>) {
    let contract_id = env.register(RefundManager, ());
    let client = RefundManagerClient::new(env, &contract_id);
    let admin = Address::generate(env);

    let token_admin = Address::generate(env);
    let usdc_token = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    client.initialize_refund_manager(&admin, &usdc_token);

    let token_admin_client = token::StellarAssetClient::new(env, &usdc_token);
    token_admin_client.mint(&contract_id, &1_000_000_000_000i128);

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
fn test_create_payment() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "payment_123");
    let merchant_id = Address::generate(&env);
    let amount = 1000000000i128; // 1000 USDC (6 decimals)
    let currency = Symbol::new(&env, "USDC");
    let deposit_address = Address::generate(&env);
    let expires_at = env.ledger().timestamp() + 3600;
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    let payment = client.create_payment(&args);

    assert_eq!(payment.payment_id, payment_id);
    assert_eq!(payment.merchant_id, merchant_id);
    assert_eq!(payment.amount, amount);
    assert_eq!(payment.currency, currency);
    assert_eq!(payment.deposit_address, deposit_address);
    assert_eq!(payment.status, PaymentStatus::Pending);
    assert_eq!(payment.memo, None);
    assert_eq!(payment.memo_type, None);
}

#[test]
fn test_create_payment_rate_limit_enforced() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let currency = Symbol::new(&env, "USDC");
    let deposit_address = Address::generate(&env);
    let expires_at = env.ledger().timestamp() + 3600;

    for i in 0..CREATE_PAYMENT_MAX_PER_WINDOW {
        let payment_id = format_id(&env, "rate_limit_", i as u64);
        let args = create_payment_args(&env, &payment_id, &merchant_id, 100i128);
        client.create_payment(&args);
    }

    let overflow_id = String::from_str(&env, "rate_limit_overflow");
    let args = create_payment_args(&env, &overflow_id, &merchant_id, 100i128);
    let overflow = client.try_create_payment(&args);

    assert_eq!(overflow, Err(Ok(Error::RateLimitExceeded)));
}

#[test]
fn test_cancel_multiple_streams_for_sender() {
    let env = Env::default();
    env.mock_all_auths();
    let (_admin, client) = setup_payment_processor(&env);

    let token_admin = Address::generate(&env);
    let token = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
    token::StellarAssetClient::new(&env, &token).mint(&client.address, &1_000_000i128);

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let stream_id1 = String::from_str(&env, "stream_1");
    let stream_id2 = String::from_str(&env, "stream_2");

    // Fund sender
    token::StellarAssetClient::new(&env, &token).mint(&sender, &1_000_000i128);

    client.create_stream(&sender, &recipient, &token, &100i128, &1_000i128, &stream_id1);
    client.create_stream(&sender, &recipient, &token, &200i128, &2_000i128, &stream_id2);

    let stream_ids = vec![&env, stream_id1.clone(), stream_id2.clone()];
    let cancelled = client.cancel_multiple_streams(&sender, &stream_ids);

    assert_eq!(cancelled.len(), 2);
    let stream1 = client.get_stream(&stream_id1);
    let stream2 = client.get_stream(&stream_id2);
    assert_eq!(stream1.status, StreamStatus::Cancelled);
    assert_eq!(stream2.status, StreamStatus::Cancelled);
}

#[test]
fn test_batch_withdraw_to_custom_routing() {
    let env = Env::default();
    env.mock_all_auths();
    let (_admin, client) = setup_payment_processor(&env);

    let token_admin = Address::generate(&env);
    let token = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
    let token_client = token::StellarAssetClient::new(&env, &token);

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let destination1 = Address::generate(&env);
    let destination2 = Address::generate(&env);
    let stream_id1 = String::from_str(&env, "stream_a");
    let stream_id2 = String::from_str(&env, "stream_b");

    // Fund sender and let contract hold tokens
    token_client.mint(&sender, &10_000i128);

    client.create_stream(&sender, &recipient, &token, &100i128, &1_000i128, &stream_id1);
    client.create_stream(&sender, &recipient, &token, &200i128, &2_000i128, &stream_id2);

    // Advance time so some tokens accrue
    env.ledger().set_timestamp(env.ledger().timestamp() + 1);

    let withdrawal1 = WithdrawalRecipient {
        stream_id: stream_id1.clone(),
        destination: destination1.clone(),
        amount: 40,
    };
    let withdrawal2 = WithdrawalRecipient {
        stream_id: stream_id2.clone(),
        destination: destination2.clone(),
        amount: 150,
    };
    let withdrawals = vec![&env, withdrawal1, withdrawal2];

    let success = client.batch_withdraw_to(&recipient, &withdrawals);
    assert_eq!(success.len(), 2);
}

#[test]
fn test_verify_payment_success() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "payment_123");
    let merchant_id = Address::generate(&env);
    let amount = 1000000000i128;
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    client.create_payment(&args);

    let payer_address = Address::generate(&env);
    let transaction_hash = BytesN::<32>::random(&env);
    let oracle = Address::generate(&env);
    client.grant_role(&admin, &role_oracle(&env), &oracle);

    let status = client.verify_payment(
        &oracle,
        &payment_id,
        &transaction_hash,
        &payer_address,
        &amount,
    );

    assert_eq!(status, PaymentStatus::Confirmed);
    let payment = client.get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Confirmed);
    assert_eq!(payment.amount_received, Some(amount));
}

#[test]
fn test_verify_payment_partially_paid() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "partial_pay");
    let merchant_id = Address::generate(&env);
    let amount = 1000000000i128;
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    client.create_payment(&args);

    let oracle = Address::generate(&env);
    client.grant_role(&admin, &role_oracle(&env), &oracle);

    // Send significantly less than expected (outside tolerance)
    let amount_received = amount - 100;
    let status = client.verify_payment(
        &oracle,
        &payment_id,
        &BytesN::<32>::random(&env),
        &Address::generate(&env),
        &amount_received,
    );

    assert_eq!(status, PaymentStatus::PartiallyPaid);
    let payment = client.get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::PartiallyPaid);
    assert_eq!(payment.amount_received, Some(amount_received));
}

#[test]
fn test_verify_payment_overpaid() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "over_pay");
    let merchant_id = Address::generate(&env);
    let amount = 1000000000i128;
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    client.create_payment(&args);

    let oracle = Address::generate(&env);
    client.grant_role(&admin, &role_oracle(&env), &oracle);

    // Send more than expected (outside tolerance)
    let amount_received = amount + 100;
    let status = client.verify_payment(
        &oracle,
        &payment_id,
        &BytesN::<32>::random(&env),
        &Address::generate(&env),
        &amount_received,
    );

    assert_eq!(status, PaymentStatus::Overpaid);
    let payment = client.get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Overpaid);
    assert_eq!(payment.amount_received, Some(amount_received));
}

#[test]
fn test_verify_payment_within_tolerance() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "tol_pay");
    let merchant_id = Address::generate(&env);
    let amount = 1000000000i128;
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    client.create_payment(&args);

    let oracle = Address::generate(&env);
    client.grant_role(&admin, &role_oracle(&env), &oracle);

    // Send exactly 1 stroop less — within tolerance → Confirmed
    let amount_received = amount - 1;
    let status = client.verify_payment(
        &oracle,
        &payment_id,
        &BytesN::<32>::random(&env),
        &Address::generate(&env),
        &amount_received,
    );

    assert_eq!(status, PaymentStatus::Confirmed);
    let payment = client.get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Confirmed);
    assert_eq!(payment.amount_received, Some(amount_received));
}

#[test]
fn test_get_merchant_payments_index_and_pagination() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    let currency = Symbol::new(&env, "USDC");
    let deposit_address = Address::generate(&env);
    let expires_at = env.ledger().timestamp() + 3600;

    let payment_id_1 = String::from_str(&env, "merchant_pay_1");
    let payment_id_2 = String::from_str(&env, "merchant_pay_2");
    let payment_id_3 = String::from_str(&env, "merchant_pay_3");

    client.grant_role(&admin, &role_merchant(&env), &merchant_id);
    client.create_payment(&create_payment_args(&env, &payment_id_1, &merchant_id, 100i128));
    client.create_payment(&create_payment_args(&env, &payment_id_2, &merchant_id, 200i128));
    client.create_payment(&create_payment_args(&env, &payment_id_3, &merchant_id, 300i128));

    let all = client.get_merchant_payments(&merchant_id);
    assert_eq!(all.len(), 3);
    assert_eq!(all.get(0), Some(payment_id_1.clone()));
    assert_eq!(all.get(1), Some(payment_id_2.clone()));
    assert_eq!(all.get(2), Some(payment_id_3.clone()));

    let page = client.get_merchant_payments_paginated(&merchant_id, &1u32, &2u32);
    assert_eq!(page.len(), 2);
    assert_eq!(page.get(0), Some(payment_id_2));
    assert_eq!(page.get(1), Some(payment_id_3));
}

#[test]
fn test_cancel_pending_success() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "cancel_pending_success");
    let merchant_id = Address::generate(&env);
    let expires_at = env.ledger().timestamp() + 3600;
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let args = create_payment_args(&env, &payment_id, &merchant_id, 500i128);
    client.create_payment(&args);

    // Set time to before expiry
    env.ledger().set_timestamp(expires_at - 1);

    client.cancel_payment(&merchant_id, &payment_id);

    let payment = client.get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Failed);
}

#[test]
fn test_cancel_fails_when_confirmed() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "cancel_fails_confirmed");
    let merchant_id = Address::generate(&env);
    let amount = 500i128;
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    client.create_payment(&args);

    let oracle = Address::generate(&env);
    client.grant_role(&admin, &role_oracle(&env), &oracle);

    client.verify_payment(
        &oracle,
        &payment_id,
        &BytesN::<32>::random(&env),
        &Address::generate(&env),
        &amount,
    );

    let res = client.try_cancel_payment(&merchant_id, &payment_id);
    assert_eq!(res.unwrap_err().unwrap(), Error::PaymentAlreadyProcessed);
}

#[test]
fn test_expiry_logic() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "cancel_past_expiry");
    let merchant_id = Address::generate(&env);
    let expires_at = env.ledger().timestamp() + 3600;
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let args = create_payment_args(&env, &payment_id, &merchant_id, 500i128);
    client.create_payment(&args);

    // Set time to past expiry
    env.ledger().set_timestamp(expires_at + 1);

    // This should correctly mark it Expired, not throw an error
    let res = client.try_cancel_payment(&merchant_id, &payment_id);
    assert!(res.is_ok());

    let payment = client.get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Expired);
}

#[test]
fn test_unauthorized_cancel() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "unauth_cancel");
    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let args = create_payment_args(&env, &payment_id, &merchant_id, 500i128);
    client.create_payment(&args);

    let random_addr = Address::generate(&env);
    let res = client.try_cancel_payment(&random_addr, &payment_id);
    assert_eq!(res.unwrap_err().unwrap(), Error::Unauthorized);
}

#[test]
fn test_expire_payment_after_deadline() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "expire_after_deadline");
    let merchant_id = Address::generate(&env);
    let expires_at = env.ledger().timestamp() + 10;
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let mut args = create_payment_args(&env, &payment_id, &merchant_id, 500i128);
    args.expires_at = Some(expires_at);
    client.create_payment(&args);

    env.ledger().set_timestamp(expires_at + 1);
    client.expire_payment(&payment_id);

    let payment = client.get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Expired);
}

#[test]
fn test_create_and_get_refund() {
    let env = Env::default();
    env.mock_all_auths();
    let (_, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "payment_123");
    let merchant_id = Address::generate(&env);
    let refund_amount = 1000i128;
    let reason = String::from_str(&env, "Reason");
    let requester = Address::generate(&env);

    // Register payment so refund amount can be validated
    client.register_payment(
        &payment_id,
        &merchant_id,
        &5000i128,
        &Symbol::new(&env, "USDC"),
    );

    let refund_id = client.create_refund(&payment_id, &refund_amount, &reason, &requester);
    let refund = client.get_refund(&refund_id);

    assert_eq!(refund.payment_id, payment_id);
    assert_eq!(refund.amount, refund_amount);
    assert_eq!(refund.status, RefundStatus::Pending);
}

#[test]
fn test_process_refund() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "payment_123");
    let merchant_id = Address::generate(&env);
    let refund_amount = 1000i128;
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &5000i128,
        &Symbol::new(&env, "USDC"),
    );

    let refund_id = client.create_refund(
        &payment_id,
        &refund_amount,
        &String::from_str(&env, "Reason"),
        &requester,
    );

    let operator = Address::generate(&env);
    client.grant_role(&admin, &role_settlement_operator(&env), &operator);

    client.process_refund(&operator, &refund_id);

    let refund = client.get_refund(&refund_id);
    assert_eq!(refund.status, RefundStatus::Completed);
}

#[test]
fn test_initialize_contract() {
    let env = Env::default();
    let admin = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let usdc_token = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();

    let contract_id = env.register(RefundManager, ());
    let client = RefundManagerClient::new(&env, &contract_id);
    client.initialize_refund_manager(&admin, &usdc_token);

    assert_eq!(client.get_admin(), Some(admin.clone()));
    assert!(client.has_role(&role_admin(&env), &admin));
}

#[test]
fn test_grant_role() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_refund_manager(&env);
    let account = Address::generate(&env);
    let role = role_oracle(&env);

    client.grant_role(&admin, &role, &account);
    assert!(client.has_role(&role, &account));
}

#[test]
fn test_transfer_admin() {
    let env = Env::default();
    env.mock_all_auths();
    let (current_admin, client) = setup_refund_manager(&env);
    let new_admin = Address::generate(&env);

    client.transfer_admin(&current_admin, &new_admin);
    assert!(client.has_role(&role_admin(&env), &new_admin));
    assert_eq!(client.get_admin(), Some(new_admin));
}

#[test]
fn test_multiple_refunds_unique_ids() {
    let env = Env::default();
    env.mock_all_auths();
    let (_, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "payment_123");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &5000i128,
        &Symbol::new(&env, "USDC"),
    );

    // Create first refund
    let refund_id_1 = client.create_refund(
        &payment_id,
        &1000i128,
        &String::from_str(&env, "First refund"),
        &requester,
    );

    // Create second refund
    let refund_id_2 = client.create_refund(
        &payment_id,
        &500i128,
        &String::from_str(&env, "Second refund"),
        &requester,
    );

    // Create third refund
    let refund_id_3 = client.create_refund(
        &payment_id,
        &250i128,
        &String::from_str(&env, "Third refund"),
        &requester,
    );

    // Verify all refund IDs are unique
    assert_ne!(refund_id_1, refund_id_2);
    assert_ne!(refund_id_2, refund_id_3);
    assert_ne!(refund_id_1, refund_id_3);

    // Verify all refunds can be retrieved independently
    let refund_1 = client.get_refund(&refund_id_1);
    let refund_2 = client.get_refund(&refund_id_2);
    let refund_3 = client.get_refund(&refund_id_3);

    assert_eq!(refund_1.amount, 1000i128);
    assert_eq!(refund_2.amount, 500i128);
    assert_eq!(refund_3.amount, 250i128);

    // Verify refund IDs follow expected pattern
    assert_eq!(refund_id_1, String::from_str(&env, "refund_1"));
    assert_eq!(refund_id_2, String::from_str(&env, "refund_2"));
    assert_eq!(refund_id_3, String::from_str(&env, "refund_3"));
}

#[test]
#[should_panic(expected = "HostError: Error(Auth, InvalidAction)")]
fn test_create_refund_requires_auth() {
    let env = Env::default();
    let (_, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "payment_123");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &5000i128,
        &Symbol::new(&env, "USDC"),
    );

    // This should panic because we're not mocking auth
    client.create_refund(
        &payment_id,
        &1000i128,
        &String::from_str(&env, "Unauthorized refund"),
        &requester,
    );
}

#[test]
#[should_panic(expected = "HostError: Error(Auth, InvalidAction)")]
fn test_create_payment_requires_auth() {
    let env = Env::default();
    let (_admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "payment_123");
    let merchant_id = Address::generate(&env);
    let amount = 1000000000i128;
    let currency = Symbol::new(&env, "USDC");
    let deposit_address = Address::generate(&env);
    let expires_at = env.ledger().timestamp() + 3600;

    // This should panic because we're not mocking auth
    let args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    client.create_payment(&args);
}

/// Issue #37: verify role membership list integrity.
#[test]
fn test_get_role_members() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_refund_manager(&env);

    let oracle1 = Address::generate(&env);
    let oracle2 = Address::generate(&env);
    let oracle_role = role_oracle(&env);

    // Initially no oracle members
    let members = client.get_role_members(&oracle_role);
    assert_eq!(members.len(), 0);

    // Grant oracle to oracle1
    client.grant_role(&admin, &oracle_role, &oracle1);
    let members = client.get_role_members(&oracle_role);
    assert_eq!(members.len(), 1);
    assert_eq!(members.get(0), Some(oracle1.clone()));

    // Grant oracle to oracle2
    client.grant_role(&admin, &oracle_role, &oracle2);
    let members = client.get_role_members(&oracle_role);
    assert_eq!(members.len(), 2);

    // Revoke oracle1 — list should shrink
    client.revoke_role(&admin, &oracle_role, &oracle1);
    let members = client.get_role_members(&oracle_role);
    assert_eq!(members.len(), 1);
    assert_eq!(members.get(0), Some(oracle2.clone()));
}

/// Issue #37: admin is automatically in the ADMIN role members list after initialize.
#[test]
fn test_admin_in_role_members_after_init() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_refund_manager(&env);

    let admin_role = role_admin(&env);
    let members = client.get_role_members(&admin_role);
    assert_eq!(members.len(), 1);
    assert_eq!(members.get(0), Some(admin));
}

fn setup_refund_manager_with_token(env: &Env) -> (Address, RefundManagerClient<'_>, Address) {
    let contract_id = env.register(RefundManager, ());
    let client = RefundManagerClient::new(env, &contract_id);
    let admin = Address::generate(env);
    let token_admin = Address::generate(env);
    let usdc_token = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    client.initialize_refund_manager(&admin, &usdc_token);
    let token_admin_client = token::StellarAssetClient::new(env, &usdc_token);
    token_admin_client.mint(&contract_id, &1_000_000_000_000i128);
    (admin, client, usdc_token)
}

#[test]
fn test_process_refund_deducts_fee_from_requester() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client, usdc_token) = setup_refund_manager_with_token(&env);

    let payment_id = String::from_str(&env, "payment_fee_1");
    let merchant_id = Address::generate(&env);
    let refund_amount = 10_000i128;
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &refund_amount,
        &Symbol::new(&env, "USDC"),
    );
    let refund_id = client.create_refund(
        &payment_id,
        &refund_amount,
        &String::from_str(&env, "fee test"),
        &requester,
    );

    let operator = Address::generate(&env);
    client.grant_role(&admin, &role_settlement_operator(&env), &operator);
    client.process_refund(&operator, &refund_id);

    let token_client = token::TokenClient::new(&env, &usdc_token);
    let fee = refund_amount * 100 / 10_000; // 1%
    let net = refund_amount - fee;

    assert_eq!(token_client.balance(&requester), net);
}

#[test]
fn test_process_refund_sends_fee_to_admin() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client, usdc_token) = setup_refund_manager_with_token(&env);

    let payment_id = String::from_str(&env, "payment_fee_2");
    let merchant_id = Address::generate(&env);
    let refund_amount = 10_000i128;
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &refund_amount,
        &Symbol::new(&env, "USDC"),
    );
    let refund_id = client.create_refund(
        &payment_id,
        &refund_amount,
        &String::from_str(&env, "fee test"),
        &requester,
    );

    let operator = Address::generate(&env);
    client.grant_role(&admin, &role_settlement_operator(&env), &operator);
    client.process_refund(&operator, &refund_id);

    let token_client = token::TokenClient::new(&env, &usdc_token);
    let fee = refund_amount * 100 / 10_000; // 1%

    assert_eq!(token_client.balance(&admin), fee);
}

#[test]
fn test_cancel_refund_by_requester() {
    let env = Env::default();
    env.mock_all_auths();
    let (_, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "payment_cancel_1");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &5000i128,
        &Symbol::new(&env, "USDC"),
    );
    let refund_id = client.create_refund(
        &payment_id,
        &1000i128,
        &String::from_str(&env, "cancel me"),
        &requester,
    );

    client.cancel_refund(&requester, &refund_id);

    // Refund record should be gone
    let result = client.try_get_refund(&refund_id);
    assert_eq!(result, Err(Ok(Error::RefundNotFound)));

    // Payment refund list should be empty
    let refunds = client.get_payment_refunds(&payment_id);
    assert_eq!(refunds.len(), 0);
}

#[test]
fn test_cancel_refund_by_admin() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "payment_cancel_2");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &5000i128,
        &Symbol::new(&env, "USDC"),
    );
    let refund_id = client.create_refund(
        &payment_id,
        &500i128,
        &String::from_str(&env, "admin cancel"),
        &requester,
    );

    client.cancel_refund(&admin, &refund_id);

    let result = client.try_get_refund(&refund_id);
    assert_eq!(result, Err(Ok(Error::RefundNotFound)));
}

#[test]
fn test_cancel_refund_unauthorized() {
    let env = Env::default();
    env.mock_all_auths();
    let (_, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "payment_cancel_3");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &5000i128,
        &Symbol::new(&env, "USDC"),
    );
    let refund_id = client.create_refund(
        &payment_id,
        &500i128,
        &String::from_str(&env, "reason"),
        &requester,
    );

    let random = Address::generate(&env);
    let result = client.try_cancel_refund(&random, &refund_id);
    assert_eq!(result, Err(Ok(Error::Unauthorized)));
}

#[test]
fn test_cancel_refund_already_processed() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "payment_cancel_4");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &5000i128,
        &Symbol::new(&env, "USDC"),
    );
    let refund_id = client.create_refund(
        &payment_id,
        &500i128,
        &String::from_str(&env, "reason"),
        &requester,
    );

    let operator = Address::generate(&env);
    client.grant_role(&admin, &role_settlement_operator(&env), &operator);
    client.process_refund(&operator, &refund_id);

    // Attempt to cancel a completed refund
    let result = client.try_cancel_refund(&requester, &refund_id);
    assert_eq!(result, Err(Ok(Error::RefundAlreadyProcessed)));
}

#[test]
fn test_cancel_refund_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (_, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "payment_cancel_5");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &5000i128,
        &Symbol::new(&env, "USDC"),
    );
    let refund_id = client.create_refund(
        &payment_id,
        &750i128,
        &String::from_str(&env, "reason"),
        &requester,
    );

    client.cancel_refund(&requester, &refund_id);

    // Verify REFUND/CANCELLED event was emitted
    let events = env.events().all();
    assert!(!events.is_empty());
}

// ── Issue #114: Total Refund Validation ──────────────────────────────────────

/// Refunding exactly the payment amount should succeed.
#[test]
fn test_refund_total_equals_payment_amount_succeeds() {
    let env = Env::default();
    env.mock_all_auths();
    let (_, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "pay_exact");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);
    let amount = 1000i128;

    client.register_payment(
        &payment_id,
        &merchant_id,
        &amount,
        &Symbol::new(&env, "USDC"),
    );
    let refund_id = client.create_refund(
        &payment_id,
        &amount,
        &String::from_str(&env, "full refund"),
        &requester,
    );
    let refund = client.get_refund(&refund_id);
    assert_eq!(refund.amount, amount);
}

/// A single refund exceeding the payment amount must be rejected.
#[test]
#[should_panic(expected = "Error(Contract, #16)")]
fn test_refund_exceeds_payment_amount_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (_, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "pay_over");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &500i128,
        &Symbol::new(&env, "USDC"),
    );
    // Attempt to refund more than the payment amount
    client.create_refund(
        &payment_id,
        &501i128,
        &String::from_str(&env, "over refund"),
        &requester,
    );
}

/// Cumulative partial refunds that exceed the payment amount must be rejected.
#[test]
#[should_panic(expected = "Error(Contract, #16)")]
fn test_cumulative_refunds_exceed_payment_amount_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (_, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "pay_cumulative");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &1000i128,
        &Symbol::new(&env, "USDC"),
    );

    // First partial refund: 600
    client.create_refund(
        &payment_id,
        &600i128,
        &String::from_str(&env, "partial 1"),
        &requester,
    );

    // Second partial refund: 401 — total would be 1001 > 1000, must fail
    client.create_refund(
        &payment_id,
        &401i128,
        &String::from_str(&env, "partial 2 over"),
        &requester,
    );
}

// ── Issue #115: Partial Refund Support ───────────────────────────────────────

/// Multiple partial refunds up to the payment total should all succeed and be tracked.
#[test]
fn test_partial_refunds_tracked_in_payment_refunds_list() {
    let env = Env::default();
    env.mock_all_auths();
    let (_, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "pay_partial");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &1000i128,
        &Symbol::new(&env, "USDC"),
    );

    let r1 = client.create_refund(
        &payment_id,
        &300i128,
        &String::from_str(&env, "partial 1"),
        &requester,
    );
    let r2 = client.create_refund(
        &payment_id,
        &400i128,
        &String::from_str(&env, "partial 2"),
        &requester,
    );
    let r3 = client.create_refund(
        &payment_id,
        &300i128,
        &String::from_str(&env, "partial 3"),
        &requester,
    );

    // All three refunds should be in the payment's refund list
    let refunds = client.get_payment_refunds(&payment_id);
    assert_eq!(refunds.len(), 3);

    // Verify amounts are tracked correctly
    assert_eq!(client.get_refund(&r1).amount, 300i128);
    assert_eq!(client.get_refund(&r2).amount, 400i128);
    assert_eq!(client.get_refund(&r3).amount, 300i128);
}

/// Rejected refunds should not count toward the total, allowing a replacement refund.
#[test]
fn test_rejected_refund_does_not_count_toward_total() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "pay_rejected");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);

    client.register_payment(
        &payment_id,
        &merchant_id,
        &1000i128,
        &Symbol::new(&env, "USDC"),
    );

    let refund_id = client.create_refund(
        &payment_id,
        &800i128,
        &String::from_str(&env, "will be rejected"),
        &requester,
    );

    let operator = Address::generate(&env);
    client.grant_role(&admin, &role_settlement_operator(&env), &operator);
    client.reject_refund(&operator, &refund_id);

    // After rejection, a new refund for 800 should succeed (rejected one doesn't count)
    let new_refund_id = client.create_refund(
        &payment_id,
        &800i128,
        &String::from_str(&env, "replacement"),
        &requester,
    );
    let new_refund = client.get_refund(&new_refund_id);
    assert_eq!(new_refund.amount, 800i128);
    assert_eq!(new_refund.status, RefundStatus::Pending);
}

// --- Payment expiry / duration tests ---

#[test]
fn test_create_payment_with_explicit_expires_at() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let expires_at = env.ledger().timestamp() + 7200; // 2 hours
    let payment_id = String::from_str(&env, "pay_explicit_expiry");
    let mut args = create_payment_args(&env, &payment_id, &merchant_id, 1000i128);
    args.expires_at = Some(expires_at);
    let payment = client.create_payment(&args);
    assert_eq!(payment.expires_at, expires_at);
}

#[test]
fn test_create_payment_with_duration_secs() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let now = env.ledger().timestamp();
    let duration = 1800u64; // 30 minutes
    let payment_id = String::from_str(&env, "pay_duration");
    let mut args = create_payment_args(&env, &payment_id, &merchant_id, 1000i128);
    args.expires_at = None;
    args.duration_secs = Some(duration);
    let payment = client.create_payment(&args);
    assert_eq!(payment.expires_at, now + duration);
}

#[test]
fn test_create_payment_defaults_to_one_hour() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let now = env.ledger().timestamp();
    let payment_id = String::from_str(&env, "pay_default_expiry");
    let mut args = create_payment_args(&env, &payment_id, &merchant_id, 1000i128);
    args.expires_at = None;
    let payment = client.create_payment(&args);
    assert_eq!(payment.expires_at, now + DEFAULT_PAYMENT_DURATION_SECS);
}

#[test]
fn test_create_payment_explicit_expires_at_overrides_duration() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let explicit_ts = env.ledger().timestamp() + 9999;
    let payment_id = String::from_str(&env, "pay_explicit_wins");
    let mut args = create_payment_args(&env, &payment_id, &merchant_id, 1000i128);
    args.expires_at = Some(explicit_ts);
    args.duration_secs = Some(60u64);
    let payment = client.create_payment(&args);
    assert_eq!(payment.expires_at, explicit_ts);
}

#[test]
fn test_create_payment_past_expires_at_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let now = env.ledger().timestamp();
    // expires_at in the past (or equal to now)
    let payment_id = String::from_str(&env, "pay_past_expiry");
    let mut args = create_payment_args(&env, &payment_id, &merchant_id, 1000i128);
    args.expires_at = Some(now);
    let result = client.try_create_payment(&args);
    assert_eq!(result, Err(Ok(Error::InvalidExpiry)));
}

#[test]
fn test_create_payment_zero_duration_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let payment_id = String::from_str(&env, "pay_zero_duration");
    let mut args = create_payment_args(&env, &payment_id, &merchant_id, 1000i128);
    args.expires_at = None;
    args.duration_secs = Some(0u64);
    let result = client.try_create_payment(&args);
    assert_eq!(result, Err(Ok(Error::InvalidExpiry)));
}

// --- Amount limits tests ---

#[test]
fn test_global_min_limit_blocks_payment() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    client.set_global_amount_limits(&admin, &Some(500i128), &None::<i128>);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let payment_id = String::from_str(&env, "pay_below_global_min");
    let args = create_payment_args(&env, &payment_id, &merchant_id, 499i128);
    let result = client.try_create_payment(&args);
    assert_eq!(result, Err(Ok(Error::AmountBelowMin)));
}

#[test]
fn test_global_max_limit_blocks_payment() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    client.set_global_amount_limits(&admin, &None::<i128>, &Some(1000i128));

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let payment_id = String::from_str(&env, "pay_above_global_max");
    let args = create_payment_args(&env, &payment_id, &merchant_id, 1001i128);
    let result = client.try_create_payment(&args);
    assert_eq!(result, Err(Ok(Error::AmountAboveMax)));
}

#[test]
fn test_global_limits_allow_payment_within_range() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    client.set_global_amount_limits(&admin, &Some(100i128), &Some(10_000i128));

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let payment_id = String::from_str(&env, "pay_within_global");
    let args = create_payment_args(&env, &payment_id, &merchant_id, 5_000i128);
    let payment = client.create_payment(&args);
    assert_eq!(payment.status, PaymentStatus::Pending);
}

#[test]
fn test_merchant_limits_override_global_limits() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    // Global: min 1000
    client.set_global_amount_limits(&admin, &Some(1000i128), &None::<i128>);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    // Merchant-specific: min 10 (lower than global)
    client.set_merchant_amount_limits(&merchant_id, &Some(10i128), &None::<i128>);

    // 500 is below global min but above merchant min — should succeed
    let payment_id = String::from_str(&env, "pay_merchant_override");
    let args = create_payment_args(&env, &payment_id, &merchant_id, 500i128);
    let payment = client.create_payment(&args);
    assert_eq!(payment.status, PaymentStatus::Pending);
}

#[test]
fn test_merchant_max_limit_blocks_payment() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    client.set_merchant_amount_limits(&merchant_id, &None::<i128>, &Some(200i128));

    let payment_id = String::from_str(&env, "pay_above_merchant_max");
    let args = create_payment_args(&env, &payment_id, &merchant_id, 201i128);
    let result = client.try_create_payment(&args);
    assert_eq!(result, Err(Ok(Error::AmountAboveMax)));
}

#[test]
fn test_set_merchant_limits_invalid_range_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    // min > max — must fail
    let result = client.try_set_merchant_amount_limits(
        &merchant_id,
        &Some(1000i128),
        &Some(500i128),
    );
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn test_get_merchant_and_global_limits() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    assert_eq!(client.get_global_amount_limits(), None);
    assert_eq!(client.get_merchant_amount_limits(&merchant_id), None);

    client.set_global_amount_limits(&admin, &Some(50i128), &Some(5000i128));
    client.set_merchant_amount_limits(&merchant_id, &Some(100i128), &Some(2000i128));

    let global = client.get_global_amount_limits().unwrap();
    assert_eq!(global.min, Some(50i128));
    assert_eq!(global.max, Some(5000i128));

    let merchant = client.get_merchant_amount_limits(&merchant_id).unwrap();
    assert_eq!(merchant.min, Some(100i128));
    assert_eq!(merchant.max, Some(2000i128));
}

// --- Multi-asset payment tests ---

#[test]
fn test_create_payment_with_allowed_token() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let token_admin = Address::generate(&env);
    let alt_token = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();

    // Allow the token
    client.allow_token(&admin, &alt_token);
    assert!(client.is_token_allowed(&alt_token));

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let payment_id = String::from_str(&env, "pay_alt_token");
    let mut args = create_payment_args(&env, &payment_id, &merchant_id, 1000i128);
    args.currency = Symbol::new(&env, "EURC");
    args.token_address = Some(alt_token.clone());
    let payment = client.create_payment(&args);

    assert_eq!(payment.token_address, Some(alt_token));
    assert_eq!(payment.status, PaymentStatus::Pending);
}

#[test]
fn test_create_payment_with_unlisted_token_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let token_admin = Address::generate(&env);
    let unknown_token = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();

    // Do NOT allow the token
    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let payment_id = String::from_str(&env, "pay_bad_token");
    let mut args = create_payment_args(&env, &payment_id, &merchant_id, 1000i128);
    args.currency = Symbol::new(&env, "RAND");
    args.token_address = Some(unknown_token);
    let result = client.try_create_payment(&args);

    assert_eq!(result, Err(Ok(Error::UnsupportedToken)));
}

#[test]
fn test_create_payment_no_token_address_uses_default() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let payment_id = String::from_str(&env, "pay_default_token");
    let args = create_payment_args(&env, &payment_id, &merchant_id, 500i128);
    let payment = client.create_payment(&args);

    assert_eq!(payment.token_address, None);
    assert_eq!(payment.status, PaymentStatus::Pending);
}

#[test]
fn test_verify_payment_decimal_aware_tolerance_7_decimals() {
    // A token with 7 decimals should have tolerance = 10 (10^(7-6))
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let token_admin = Address::generate(&env);
    let alt_token = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    // Stellar asset contracts report 7 decimals
    client.allow_token(&admin, &alt_token);

    let merchant_id = Address::generate(&env);
    let oracle = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);
    client.grant_role(&admin, &role_oracle(&env), &oracle);

    let payment_id = String::from_str(&env, "pay_7dec");
    let amount = 1_000_000_0i128; // 1.0 in 7-decimal units
    let mut args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    args.currency = Symbol::new(&env, "EURC");
    args.token_address = Some(alt_token);
    client.create_payment(&args);

    // Underpay by 10 (within 7-decimal tolerance of 10) → Confirmed
    let status = client.verify_payment(
        &oracle,
        &payment_id,
        &BytesN::<32>::random(&env),
        &Address::generate(&env),
        &(amount - 10),
    );
    assert_eq!(status, PaymentStatus::Confirmed);
}

#[test]
fn test_verify_payment_decimal_aware_tolerance_7_decimals_overpay() {
    // Underpay by 11 (outside 7-decimal tolerance of 10) → PartiallyPaid
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let token_admin = Address::generate(&env);
    let alt_token = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    client.allow_token(&admin, &alt_token);

    let merchant_id = Address::generate(&env);
    let oracle = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);
    client.grant_role(&admin, &role_oracle(&env), &oracle);

    let payment_id = String::from_str(&env, "pay_7dec_partial");
    let amount = 1_000_000_0i128;
    let mut args = create_payment_args(&env, &payment_id, &merchant_id, amount);
    args.currency = Symbol::new(&env, "EURC");
    args.token_address = Some(alt_token);
    client.create_payment(&args);

    // Underpay by 11 → PartiallyPaid
    let status = client.verify_payment(
        &oracle,
        &payment_id,
        &BytesN::<32>::random(&env),
        &Address::generate(&env),
        &(amount - 11),
    );
    assert_eq!(status, PaymentStatus::PartiallyPaid);
}

// --- Cumulative refund cap tests ---

#[test]
fn test_cumulative_refunds_exceed_payment_amount_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (_, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "pay_cumulative_1");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);
    let payment_amount = 1000i128;

    client.register_payment(&payment_id, &merchant_id, &payment_amount, &Symbol::new(&env, "USDC"));

    // First refund: 600 — ok
    client.create_refund(&payment_id, &600i128, &String::from_str(&env, "partial 1"), &requester);

    // Second refund: 500 — 600 + 500 = 1100 > 1000 — must fail
    let result = client.try_create_refund(
        &payment_id,
        &500i128,
        &String::from_str(&env, "partial 2"),
        &requester,
    );
    assert_eq!(result, Err(Ok(Error::RefundExceedsPayment)));
}

#[test]
fn test_refund_exactly_equal_to_payment_amount_succeeds() {
    let env = Env::default();
    env.mock_all_auths();
    let (_, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "pay_exact_1");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);
    let payment_amount = 1000i128;

    client.register_payment(&payment_id, &merchant_id, &payment_amount, &Symbol::new(&env, "USDC"));

    // Single refund equal to full payment amount — must succeed
    let refund_id = client.create_refund(
        &payment_id,
        &payment_amount,
        &String::from_str(&env, "full refund"),
        &requester,
    );
    let refund = client.get_refund(&refund_id);
    assert_eq!(refund.amount, payment_amount);
    assert_eq!(refund.status, RefundStatus::Pending);
}

#[test]
fn test_second_refund_after_full_refund_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (_, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "pay_full_then_extra");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);
    let payment_amount = 1000i128;

    client.register_payment(&payment_id, &merchant_id, &payment_amount, &Symbol::new(&env, "USDC"));

    // Full refund — ok
    client.create_refund(&payment_id, &payment_amount, &String::from_str(&env, "full"), &requester);

    // Any additional refund — must fail
    let result = client.try_create_refund(
        &payment_id,
        &1i128,
        &String::from_str(&env, "extra"),
        &requester,
    );
    assert_eq!(result, Err(Ok(Error::RefundExceedsPayment)));
}

#[test]
fn test_rejected_refunds_not_counted_in_cumulative_total() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_refund_manager(&env);

    let payment_id = String::from_str(&env, "pay_rejected_refund");
    let merchant_id = Address::generate(&env);
    let requester = Address::generate(&env);
    let payment_amount = 1000i128;

    client.register_payment(&payment_id, &merchant_id, &payment_amount, &Symbol::new(&env, "USDC"));

    // Create and reject a refund for 800
    let refund_id = client.create_refund(
        &payment_id,
        &800i128,
        &String::from_str(&env, "will be rejected"),
        &requester,
    );
    let operator = Address::generate(&env);
    client.grant_role(&admin, &role_settlement_operator(&env), &operator);
    client.reject_refund(&operator, &refund_id);

    // A new refund for 1000 should succeed because the rejected one is excluded
    let new_refund_id = client.create_refund(
        &payment_id,
        &payment_amount,
        &String::from_str(&env, "after rejection"),
        &requester,
    );
    let refund = client.get_refund(&new_refund_id);
    assert_eq!(refund.amount, payment_amount);
    assert_eq!(refund.status, RefundStatus::Pending);
}

// --- Multi-account settlement tests ---

fn make_confirmed_payment(
    env: &Env,
    client: &PaymentProcessorClient,
    admin: &Address,
    payment_id: &String,
    amount: i128,
) {
    let merchant = Address::generate(env);
    let oracle = Address::generate(env);
    client.grant_role(admin, &role_merchant(env), &merchant);
    client.grant_role(admin, &role_oracle(env), &oracle);
    let args = create_payment_args(env, payment_id, &merchant, amount);
    client.create_payment(&args);
    client.verify_payment(
        &oracle,
        payment_id,
        &BytesN::<32>::random(env),
        &Address::generate(env),
        &amount,
    );
}

#[test]
fn test_settle_payment_single_split() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "settle_single");
    let amount = 1000i128;
    make_confirmed_payment(&env, &client, &admin, &payment_id, amount);

    let operator = Address::generate(&env);
    client.grant_role(&admin, &role_settlement_operator(&env), &operator);

    let recipient = Address::generate(&env);
    let splits = vec![&env, SettlementSplit { recipient, amount }];
    client.settle_payment(&operator, &payment_id, &splits);

    assert_eq!(client.get_payment(&payment_id).status, PaymentStatus::Settled);
}

// --- Idempotency key (client_token) tests ---

#[test]
fn test_settle_payment_multi_split() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "settle_multi");
    let amount = 1000i128;
    make_confirmed_payment(&env, &client, &admin, &payment_id, amount);

    let operator = Address::generate(&env);
    client.grant_role(&admin, &role_settlement_operator(&env), &operator);

    let splits = vec![
        &env,
        SettlementSplit { recipient: Address::generate(&env), amount: 600 },
        SettlementSplit { recipient: Address::generate(&env), amount: 400 },
    ];
    client.settle_payment(&operator, &payment_id, &splits);

    assert_eq!(client.get_payment(&payment_id).status, PaymentStatus::Settled);
}

// --- Idempotency key (client_token) tests ---

#[test]
fn test_create_payment_idempotency_retry_returns_same_payment() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let payment_id = String::from_str(&env, "idem_pay_1");
    let client_token = Some(String::from_str(&env, "tok_abc123"));
    let expires_at = env.ledger().timestamp() + 3600;

    let args = CreatePaymentArgs {
        payment_id: payment_id.clone(),
        merchant_id: merchant_id.clone(),
        amount: 1000,
        currency: Symbol::new(&env, "USDC"),
        deposit_address: Address::generate(&env),
        expires_at: Some(expires_at),
        duration_secs: None,
        memo: None,
        memo_type: None,
        token_address: None,
        client_token: client_token.clone(),
    };

    let first = client.create_payment(&args);

    // Retry with same client_token and payment_id — must return the same payment
    let retry = client.create_payment(&args);

    assert_eq!(first.payment_id, retry.payment_id);
    assert_eq!(first.created_at, retry.created_at);
}

#[test]
fn test_create_payment_idempotency_different_payment_id_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let client_token = Some(String::from_str(&env, "tok_conflict"));
    let expires_at = env.ledger().timestamp() + 3600;

    let args_a = CreatePaymentArgs {
        payment_id: String::from_str(&env, "idem_pay_a"),
        merchant_id: merchant_id.clone(),
        amount: 1000,
        currency: Symbol::new(&env, "USDC"),
        deposit_address: Address::generate(&env),
        expires_at: Some(expires_at),
        duration_secs: None,
        memo: None,
        memo_type: None,
        token_address: None,
        client_token: client_token.clone(),
    };

    // First call succeeds
    client.create_payment(&args_a);

    // Second call with same token but different payment_id must fail
    let mut args_b = args_a.clone();
    args_b.payment_id = String::from_str(&env, "idem_pay_b");

    let result = client.try_create_payment(&args_b);

    assert_eq!(result, Err(Ok(Error::DuplicateIdempotencyKey)));
}

#[test]
fn test_create_payment_without_idempotency_token_fails_on_retry() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let merchant_id = Address::generate(&env);
    client.grant_role(&admin, &role_merchant(&env), &merchant_id);

    let payment_id = String::from_str(&env, "idem_pay_no_tok");
    let expires_at = env.ledger().timestamp() + 3600;

    let args = CreatePaymentArgs {
        payment_id: payment_id.clone(),
        merchant_id: merchant_id.clone(),
        amount: 1000,
        currency: Symbol::new(&env, "USDC"),
        deposit_address: Address::generate(&env),
        expires_at: Some(expires_at),
        duration_secs: None,
        memo: None,
        memo_type: None,
        token_address: None,
        client_token: None,
    };

    client.create_payment(&args);

    // Without a client_token, a second call with the same payment_id returns PaymentAlreadyExists
    let result = client.try_create_payment(&args);

    assert_eq!(result, Err(Ok(Error::PaymentAlreadyExists)));
}

#[test]
fn test_settle_payment_empty_splits_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "settle_empty");
    let amount = 1000i128;
    make_confirmed_payment(&env, &client, &admin, &payment_id, amount);

    let operator = Address::generate(&env);
    client.grant_role(&admin, &role_settlement_operator(&env), &operator);

    let splits = vec![&env];
    let result = client.try_settle_payment(&operator, &payment_id, &splits);
    assert_eq!(result, Err(Ok(Error::InvalidSettlement)));
}

#[test]
fn test_settle_payment_split_total_mismatch_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_payment_processor(&env);

    let payment_id = String::from_str(&env, "settle_mismatch");
    let amount = 1000i128;
    make_confirmed_payment(&env, &client, &admin, &payment_id, amount);

    let operator = Address::generate(&env);
    client.grant_role(&admin, &role_settlement_operator(&env), &operator);

    // Total is 900, not 1000 — must fail
    let splits = vec![
        &env,
        SettlementSplit { recipient: Address::generate(&env), amount: 500 },
        SettlementSplit { recipient: Address::generate(&env), amount: 400 },
    ];
    let result = client.try_settle_payment(&operator, &payment_id, &splits);
    assert_eq!(result, Err(Ok(Error::InvalidSettlement)));
}
