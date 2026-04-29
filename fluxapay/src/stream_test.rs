#![cfg(test)]

use super::stream::{PaymentStreaming, PaymentStreamingClient, StreamError, StreamStatus};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token, Address, Env, String,
};

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn setup(env: &Env) -> (PaymentStreamingClient<'_>, Address, Address, Address) {
    let contract_id = env.register(PaymentStreaming, ());
    let client = PaymentStreamingClient::new(env, &contract_id);

    let token_admin = Address::generate(env);
    let token_id = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    let sender = Address::generate(env);
    let receiver = Address::generate(env);

    // Fund sender with 1 000 000 tokens.
    let token_admin_client = token::StellarAssetClient::new(env, &token_id);
    token_admin_client.mint(&sender, &1_000_000i128);

    // Allow contract to pull tokens from sender (pre-approve).
    // mock_all_auths covers both sender.require_auth() and token transfer.
    (client, sender, receiver, token_id)
}

// ─── create_stream ─────────────────────────────────────────────────────────────

#[test]
fn test_create_stream_success() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, receiver, token) = setup(&env);

    let stream_id = String::from_str(&env, "stream_01");
    let rate = 10i128; // 10 tokens/s
    let deposit = 1_000i128;

    let stream = client.create_stream(&sender, &receiver, &token, &rate, &deposit, &stream_id);

    assert_eq!(stream.stream_id, stream_id);
    assert_eq!(stream.sender, sender);
    assert_eq!(stream.receiver, receiver);
    assert_eq!(stream.rate_per_second, rate);
    assert_eq!(stream.remaining_deposit, deposit);
    assert_eq!(stream.accrued_at_checkpoint, 0);
    assert_eq!(stream.status, StreamStatus::Active);
}

#[test]
fn test_create_stream_invalid_rate() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, receiver, token) = setup(&env);
    let stream_id = String::from_str(&env, "stream_rate_err");

    let err = client.try_create_stream(&sender, &receiver, &token, &0i128, &500i128, &stream_id);
    assert_eq!(err, Err(Ok(StreamError::InvalidRate)));
}

#[test]
fn test_create_stream_invalid_deposit() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, receiver, token) = setup(&env);
    let stream_id = String::from_str(&env, "stream_dep_err");

    let err = client.try_create_stream(&sender, &receiver, &token, &5i128, &0i128, &stream_id);
    assert_eq!(err, Err(Ok(StreamError::InvalidDeposit)));
}

#[test]
fn test_create_stream_duplicate_id() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, receiver, token) = setup(&env);
    let stream_id = String::from_str(&env, "stream_dup");

    client.create_stream(&sender, &receiver, &token, &10i128, &500i128, &stream_id);

    let err =
        client.try_create_stream(&sender, &receiver, &token, &10i128, &500i128, &stream_id);
    assert_eq!(err, Err(Ok(StreamError::StreamAlreadyExists)));
}

// ─── decrease_rate_per_second ─────────────────────────────────────────────────

/// After 50 seconds at 10 tok/s → accrued = 500.
/// New rate = 5. Old unlocked = 1000 − 500 = 500; old_secs_left = 500/10 = 50.
/// new_needed = 50 * 5 = 250 → surplus = 500 − 250 = 250.
#[test]
fn test_decrease_rate_checkpoints_and_refunds_surplus() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, receiver, token) = setup(&env);

    let stream_id = String::from_str(&env, "stream_dec");
    let deposit = 1_000i128;
    let old_rate = 10i128;

    client.create_stream(&sender, &receiver, &token, &old_rate, &deposit, &stream_id);

    // Advance time by 50 seconds.
    env.ledger().set_timestamp(env.ledger().timestamp() + 50);

    client.decrease_rate_per_second(&sender, &stream_id, &5i128);

    let stream = client.get_stream(&stream_id);

    // Checkpoint should reflect 50s * 10 tok/s = 500 accrued.
    assert_eq!(stream.accrued_at_checkpoint, 500);
    // New rate applied.
    assert_eq!(stream.rate_per_second, 5);
    // Deposit reduced by surplus (250).
    assert_eq!(stream.remaining_deposit, deposit - 250); // 750
}

/// Verify that the accrued view helper is correct before and after a rate decrease.
#[test]
fn test_get_accrued_amount_reflects_elapsed_time() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, receiver, token) = setup(&env);

    let stream_id = String::from_str(&env, "stream_accrued");
    client.create_stream(&sender, &receiver, &token, &10i128, &500i128, &stream_id);

    // 30 seconds in → 300 accrued (lazily).
    env.ledger().set_timestamp(env.ledger().timestamp() + 30);
    assert_eq!(client.get_accrued_amount(&stream_id), 300);
}

/// Decreasing rate requires the new rate < current rate.
#[test]
fn test_decrease_rate_rejects_equal_rate() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, receiver, token) = setup(&env);

    let stream_id = String::from_str(&env, "stream_eq");
    client.create_stream(&sender, &receiver, &token, &10i128, &500i128, &stream_id);

    let err = client.try_decrease_rate_per_second(&sender, &stream_id, &10i128);
    assert_eq!(err, Err(Ok(StreamError::RateNotDecreased)));
}

/// Decreasing rate to higher value is also rejected.
#[test]
fn test_decrease_rate_rejects_higher_rate() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, receiver, token) = setup(&env);

    let stream_id = String::from_str(&env, "stream_hi");
    client.create_stream(&sender, &receiver, &token, &10i128, &500i128, &stream_id);

    let err = client.try_decrease_rate_per_second(&sender, &stream_id, &20i128);
    assert_eq!(err, Err(Ok(StreamError::RateNotDecreased)));
}

/// Only the original sender may decrease the rate.
#[test]
fn test_decrease_rate_unauthorized_caller() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, receiver, token) = setup(&env);

    let stream_id = String::from_str(&env, "stream_auth");
    client.create_stream(&sender, &receiver, &token, &10i128, &500i128, &stream_id);

    let impostor = Address::generate(&env);
    let err = client.try_decrease_rate_per_second(&impostor, &stream_id, &5i128);
    assert_eq!(err, Err(Ok(StreamError::Unauthorized)));
}

/// Decreasing rate on a non-existent stream returns StreamNotFound.
#[test]
fn test_decrease_rate_stream_not_found() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, _receiver, _token) = setup(&env);

    let bad_id = String::from_str(&env, "no_such_stream");
    let err = client.try_decrease_rate_per_second(&sender, &bad_id, &5i128);
    assert_eq!(err, Err(Ok(StreamError::StreamNotFound)));
}

/// Multiple sequential rate decreases each checkpoint correctly.
#[test]
fn test_multiple_sequential_rate_decreases() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, receiver, token) = setup(&env);

    // Deposit 10 000 tok, rate 100 tok/s → would last 100s.
    let stream_id = String::from_str(&env, "stream_multi");
    client.create_stream(
        &sender,
        &receiver,
        &token,
        &100i128,
        &10_000i128,
        &stream_id,
    );

    // After 20s → accrued 2000; reduce to 50 tok/s.
    // Unlocked after checkpoint: 10000−2000=8000; old_secs_left=8000/100=80
    // new_needed=80*50=4000; surplus=4000; deposit becomes 6000.
    env.ledger().set_timestamp(env.ledger().timestamp() + 20);
    client.decrease_rate_per_second(&sender, &stream_id, &50i128);

    let s = client.get_stream(&stream_id);
    assert_eq!(s.accrued_at_checkpoint, 2_000);
    assert_eq!(s.rate_per_second, 50);
    assert_eq!(s.remaining_deposit, 6_000);

    // After another 10s → accrued_since_checkpoint = 10*50 = 500; reduce to 10.
    // Unlocked: 6000−(2000+500)=3500; old_secs_left=3500/50=70
    // new_needed=70*10=700; surplus=2800; deposit becomes 6000−2800=3200.
    env.ledger().set_timestamp(env.ledger().timestamp() + 10);
    client.decrease_rate_per_second(&sender, &stream_id, &10i128);

    let s2 = client.get_stream(&stream_id);
    assert_eq!(s2.accrued_at_checkpoint, 2_500); // 2000+500
    assert_eq!(s2.rate_per_second, 10);
    assert_eq!(s2.remaining_deposit, 3_200);
}

#[test]
fn test_set_stream_destination_and_trigger_withdrawal() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, receiver, token) = setup(&env);
    let token_client = token::StellarAssetClient::new(&env, &token);

    let stream_id = String::from_str(&env, "stream_dest");
    let destination = Address::generate(&env);

    client.create_stream(&sender, &receiver, &token, &10i128, &500i128, &stream_id);
    client.set_stream_destination(&receiver, &stream_id, &destination).unwrap();

    env.ledger().set_timestamp(env.ledger().timestamp() + 10);
    let processed = client.trigger_withdrawal(&stream_id).unwrap();

    assert_eq!(processed, stream_id);
    assert_eq!(token_client.balance(&destination), 100i128);

    let stream = client.get_stream(&stream_id);
    assert_eq!(stream.accrued_at_checkpoint, 0);
    assert_eq!(stream.remaining_deposit, 400);
}

#[test]
fn test_withdraw_all_for_recipient_limits_execution() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, receiver, token) = setup(&env);
    let token_client = token::StellarAssetClient::new(&env, &token);

    let stream_id1 = String::from_str(&env, "stream_all_1");
    let stream_id2 = String::from_str(&env, "stream_all_2");
    let stream_id3 = String::from_str(&env, "stream_all_3");

    client.create_stream(&sender, &receiver, &token, &10i128, &500i128, &stream_id1);
    client.create_stream(&sender, &receiver, &token, &20i128, &500i128, &stream_id2);
    client.create_stream(&sender, &receiver, &token, &30i128, &500i128, &stream_id3);

    env.ledger().set_timestamp(env.ledger().timestamp() + 10);

    let processed = client.withdraw_all_for_recipient(&receiver, &2u32).unwrap();
    assert_eq!(processed.len(), 2);
    assert_eq!(token_client.balance(&receiver), 100 + 200);

    let next = client.withdraw_all_for_recipient(&receiver, &2u32).unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(token_client.balance(&receiver), 100 + 200 + 300);
}

#[test]
fn test_get_sender_streams_pagination() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, sender, receiver, token) = setup(&env);

    for i in 0..5 {
        let stream_id = String::from_str(&env, &format!("sender_page_{}", i));
        client.create_stream(&sender, &receiver, &token, &10i128, &500i128, &stream_id);
    }

    let page1 = client.get_sender_streams(&sender, &0u32, &2u32);
    assert_eq!(page1.len(), 2);
    let page2 = client.get_sender_streams(&sender, &1u32, &2u32);
    assert_eq!(page2.len(), 2);
    let page3 = client.get_sender_streams(&sender, &2u32, &2u32);
    assert_eq!(page3.len(), 1);
}
