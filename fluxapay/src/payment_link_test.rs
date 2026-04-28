use crate::{PaymentLinkManager, PaymentLinkManagerClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token, Address, Env, String, Symbol,
};

fn setup_payment_link(env: &Env) -> (Address, PaymentLinkManagerClient<'_>) {
    let contract_id = env.register(PaymentLinkManager, ());
    let client = PaymentLinkManagerClient::new(env, &contract_id);
    let admin = Address::generate(env);
    (admin, client)
}

#[test]
fn test_create_link() {
    let env = Env::default();
    env.mock_all_auths();
    let (merchant, client) = setup_payment_link(&env);

    let link_id = String::from_str(&env, "link_123");
    let amount = Some(1000i128);
    let currency = Symbol::new(&env, "USDC");
    let description = String::from_str(&env, "Test Link");

    let id = client.create_link(
        &merchant,
        &link_id,
        &amount,
        &currency,
        &description,
        &None,
        &None,
        &false, &None,
    );

    assert_eq!(id, link_id);
    let link = client.get_link(&link_id);
    assert_eq!(link.merchant_id, merchant);
    assert_eq!(link.amount, amount);
    assert!(link.active);
    assert!(!link.direct_transfer);
}

#[test]
fn test_use_link_fixed_amount() {
    let env = Env::default();
    env.mock_all_auths();
    let (merchant, client) = setup_payment_link(&env);
    let payer = Address::generate(&env);

    let link_id = String::from_str(&env, "fixed_link");
    let amount = 1000i128;
    client.create_link(
        &merchant,
        &link_id,
        &Some(amount),
        &Symbol::new(&env, "USDC"),
        &String::from_str(&env, "Fixed"),
        &None,
        &None,
        &false, &None,
    );

    let payment_id = client.use_link(&payer, &link_id, &amount, &None);
    assert!(!payment_id.is_empty());

    let link = client.get_link(&link_id);
    assert_eq!(link.use_count, 1);
}

#[test]
#[should_panic(expected = "Error(Contract, #406)")]
fn test_use_link_wrong_amount() {
    let env = Env::default();
    env.mock_all_auths();
    let (merchant, client) = setup_payment_link(&env);
    let payer = Address::generate(&env);

    let link_id = String::from_str(&env, "fixed_link_wrong");
    client.create_link(
        &merchant,
        &link_id,
        &Some(1000i128),
        &Symbol::new(&env, "USDC"),
        &String::from_str(&env, "Fixed"),
        &None,
        &None,
        &false, &None,
    );

    client.use_link(&payer, &link_id, &500i128, &None);
}

#[test]
fn test_use_link_open_amount() {
    let env = Env::default();
    env.mock_all_auths();
    let (merchant, client) = setup_payment_link(&env);
    let payer = Address::generate(&env);

    let link_id = String::from_str(&env, "open_link");
    client.create_link(
        &merchant,
        &link_id,
        &None,
        &Symbol::new(&env, "USDC"),
        &String::from_str(&env, "Open"),
        &None,
        &None,
        &false, &None,
    );

    client.use_link(&payer, &link_id, &1500i128, &None);
    let link = client.get_link(&link_id);
    assert_eq!(link.use_count, 1);
}

#[test]
fn test_deactivate_link() {
    let env = Env::default();
    env.mock_all_auths();
    let (merchant, client) = setup_payment_link(&env);

    let link_id = String::from_str(&env, "deactivate_me");
    client.create_link(
        &merchant,
        &link_id,
        &None,
        &Symbol::new(&env, "USDC"),
        &String::from_str(&env, "Bye"),
        &None,
        &None,
        &false, &None,
    );

    client.deactivate_link(&merchant, &link_id);
    let link = client.get_link(&link_id);
    assert!(!link.active);
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_link_expired() {
    let env = Env::default();
    env.mock_all_auths();
    let (merchant, client) = setup_payment_link(&env);
    let payer = Address::generate(&env);

    let link_id = String::from_str(&env, "expired_link");
    let expiry = 1000u64;
    client.create_link(
        &merchant,
        &link_id,
        &None,
        &Symbol::new(&env, "USDC"),
        &String::from_str(&env, "Old"),
        &Some(expiry),
        &None,
        &false, &None,
    );

    env.ledger().set_timestamp(expiry + 1);
    client.use_link(&payer, &link_id, &100i128, &None);
}

#[test]
#[should_panic(expected = "Error(Contract, #14)")]
fn test_max_uses() {
    let env = Env::default();
    env.mock_all_auths();
    let (merchant, client) = setup_payment_link(&env);
    let payer = Address::generate(&env);

    let link_id = String::from_str(&env, "limited_link");
    client.create_link(
        &merchant,
        &link_id,
        &None,
        &Symbol::new(&env, "USDC"),
        &String::from_str(&env, "Limit"),
        &None,
        &Some(1),
        &false, &None,
    );

    client.use_link(&payer, &link_id, &100i128, &None);
    // Should fail on second use
    client.use_link(&payer, &link_id, &100i128, &None);
}

// ── Issue #111: Direct-to-Merchant Payment Flow ──────────────────────────────

#[test]
fn test_direct_transfer_link_transfers_to_merchant() {
    let env = Env::default();
    env.mock_all_auths();

    let token_admin = Address::generate(&env);
    let usdc_token = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let token_admin_client = token::StellarAssetClient::new(&env, &usdc_token);

    let (merchant, client) = setup_payment_link(&env);
    let payer = Address::generate(&env);

    // Fund payer
    token_admin_client.mint(&payer, &5000i128);

    let link_id = String::from_str(&env, "direct_link");
    let amount = 1000i128;
    client.create_link(
        &merchant,
        &link_id,
        &Some(amount),
        &Symbol::new(&env, "USDC"),
        &String::from_str(&env, "Direct"),
        &None,
        &None,
        &true, &None, // direct_transfer = true
    );

    let link = client.get_link(&link_id);
    assert!(link.direct_transfer);

    let token_client = token::TokenClient::new(&env, &usdc_token);
    let merchant_balance_before = token_client.balance(&merchant);

    client.use_link(&payer, &link_id, &amount, &Some(usdc_token.clone()));

    let merchant_balance_after = token_client.balance(&merchant);
    assert_eq!(merchant_balance_after - merchant_balance_before, amount);
}

#[test]
#[should_panic(expected = "Error(Contract, #1)")]
fn test_direct_transfer_without_token_address_fails() {
    let env = Env::default();
    env.mock_all_auths();

    let (merchant, client) = setup_payment_link(&env);
    let payer = Address::generate(&env);

    let link_id = String::from_str(&env, "direct_no_token");
    client.create_link(
        &merchant,
        &link_id,
        &Some(500i128),
        &Symbol::new(&env, "USDC"),
        &String::from_str(&env, "Direct no token"),
        &None,
        &None,
        &true, &None,
    );

    // Should fail because usdc_token is None but direct_transfer is true
    client.use_link(&payer, &link_id, &500i128, &None);
}
