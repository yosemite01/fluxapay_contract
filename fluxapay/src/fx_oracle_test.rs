use crate::{FXOracle, FXOracleClient, FXOracleError};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    Address, Env, Symbol,
};

fn setup_oracle(env: &Env) -> (Address, FXOracleClient<'_>) {
    let contract_id = env.register(FXOracle, ());
    let client = FXOracleClient::new(env, &contract_id);
    let admin = Address::generate(env);
    client.oracle_initialize(&admin, &86400); // 24 hour threshold
    (admin, client)
}

#[test]
fn test_set_and_get_rate() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_oracle(&env);

    let oracle = Address::generate(&env);
    client.oracle_grant_role(&admin, &Symbol::new(&env, "ORACLE"), &oracle);

    let pair = Symbol::new(&env, "USDC_NGN");
    let rate = 1500_0000000i128; // 1500 NGN/USDC
    let decimals = 7;

    client.set_rate(&oracle, &pair, &rate, &decimals);

    let rate_data = client.get_rate(&pair);
    assert_eq!(rate_data.rate, rate);
    assert_eq!(rate_data.decimals, decimals);
    assert_eq!(rate_data.pair, pair);
    assert_eq!(rate_data.updated_at, env.ledger().timestamp());
}

#[test]
fn test_unauthorized_set_rate() {
    let env = Env::default();
    env.mock_all_auths();
    let (_admin, client) = setup_oracle(&env);

    let unauthorized_user = Address::generate(&env);
    let pair = Symbol::new(&env, "USDC_NGN");

    let result = client.try_set_rate(&unauthorized_user, &pair, &1000i128, &2);
    assert_eq!(result, Err(Ok(FXOracleError::Unauthorized)));
}

#[test]
fn test_staleness_check() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_oracle(&env);

    let oracle = Address::generate(&env);
    client.oracle_grant_role(&admin, &Symbol::new(&env, "ORACLE"), &oracle);

    let pair = Symbol::new(&env, "USDC_NGN");
    client.set_rate(&oracle, &pair, &1500i128, &0);

    // Jump forward 25 hours (threshold is 24)
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 25 * 3600);

    let result = client.try_get_rate(&pair);
    assert_eq!(result, Err(Ok(FXOracleError::RateStale)));
}

#[test]
fn test_settlement_amount_calculation() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_oracle(&env);

    let oracle = Address::generate(&env);
    client.oracle_grant_role(&admin, &Symbol::new(&env, "ORACLE"), &oracle);

    // 1 USDC = 1500.50 NGN (2 decimals: 150050)
    let pair = Symbol::new(&env, "NGN");
    client.set_rate(&oracle, &pair, &150050i128, &2);

    // 100 USDC -> 150050 NGN
    let usdc_amount = 100i128;
    let expected_fiat = 150050i128; // (100 * 150050) / 100

    let amount = client.get_settlement_amount(&usdc_amount, &pair);
    assert_eq!(amount, expected_fiat);
}

#[test]
fn test_update_staleness_threshold() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_oracle(&env);

    client.set_staleness_threshold(&admin, &3600);
    assert_eq!(client.get_staleness_threshold(), 3600);
}
