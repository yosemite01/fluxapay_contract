use crate::format_id;
use proptest::prelude::*;
use soroban_sdk::{
    testutils::{Address as _, BytesN as _, Ledger as _},
    Address, BytesN, Env, Symbol,
};

use crate::{
    access_control::{role_merchant, role_oracle},
    Error, PaymentProcessor, PaymentProcessorClient, PaymentStatus, PAYMENT_TOLERANCE,
};

fn setup_payment_processor(env: &Env) -> (Address, PaymentProcessorClient<'_>) {
    let contract_id = env.register(PaymentProcessor, ());
    let client = PaymentProcessorClient::new(env, &contract_id);
    let admin = Address::generate(env);
    client.initialize_payment_processor(&admin);
    (admin, client)
}

proptest! {
    #[test]
    fn test_format_id_starts_with_prefix(n in 0u64..u64::MAX) {
        let env = Env::default();
        let prefix = "refund_";
        let id = format_id(&env, prefix, n);

        let mut arr = [0u8; 64];
        let len = id.len() as usize;
        id.copy_into_slice(&mut arr[..len]);
        let id_str = core::str::from_utf8(&arr[..len]).unwrap();

        assert!(id_str.starts_with(prefix));
    }

    #[test]
    fn test_format_id_uniqueness(n1 in 0u64..u64::MAX, n2 in 0u64..u64::MAX) {
        prop_assume!(n1 != n2);
        let env = Env::default();
        let prefix = "id_";
        let id1 = format_id(&env, prefix, n1);
        let id2 = format_id(&env, prefix, n2);

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_format_id_round_trip(n in 1u64..u64::MAX) {
        let env = Env::default();
        let prefix = "dispute_";
        let id = format_id(&env, prefix, n);

        let mut arr = [0u8; 64];
        let len = id.len() as usize;
        id.copy_into_slice(&mut arr[..len]);
        let id_str = core::str::from_utf8(&arr[..len]).unwrap();

        // Extract the number part
        let num_part = &id_str[prefix.len()..];
        let parsed_n: u64 = num_part.parse().unwrap();

        assert_eq!(n, parsed_n);
    }

    #[test]
    fn test_verify_payment_fails_after_expiry(
        expires_in in 1u64..300u64,
        after_expiry in 1u64..300u64,
        amount in 1i128..1_000_000i128,
        nonce in 0u64..u64::MAX,
    ) {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, client) = setup_payment_processor(&env);

        let merchant = Address::generate(&env);
        let oracle = Address::generate(&env);
        client.grant_role(&admin, &role_merchant(&env), &merchant);
        client.grant_role(&admin, &role_oracle(&env), &oracle);

        let payment_id = format_id(&env, "exp_prop_", nonce);
        let expires_at = env.ledger().timestamp() + expires_in;

        let args = crate::CreatePaymentArgs {
            payment_id: payment_id.clone(),
            merchant_id: merchant.clone(),
            amount,
            currency: Symbol::new(&env, "USDC"),
            deposit_address: Address::generate(&env),
            expires_at: Some(expires_at),
            duration_secs: None,
            memo: None,
            memo_type: None,
            token_address: None,
            client_token: None,
            metadata_hash: None,
        };

        client.create_payment(&args);

        env.ledger().set_timestamp(expires_at + after_expiry);

        let result = client.try_verify_payment(
            &oracle,
            &payment_id,
            &BytesN::<32>::random(&env),
            &Address::generate(&env),
            &amount,
        );

        assert_eq!(result, Err(Ok(Error::PaymentExpired)));
    }

    #[test]
    fn test_verify_payment_amount_boundaries(
        amount in 5i128..1_000_000i128,
        delta in -200i128..200i128,
        nonce in 0u64..u64::MAX,
    ) {
        prop_assume!(amount + delta > 0);

        let env = Env::default();
        env.mock_all_auths();
        let (admin, client) = setup_payment_processor(&env);

        let merchant = Address::generate(&env);
        let oracle = Address::generate(&env);
        client.grant_role(&admin, &role_merchant(&env), &merchant);
        client.grant_role(&admin, &role_oracle(&env), &oracle);

        let payment_id = format_id(&env, "amt_prop_", nonce);
        let expires_at = env.ledger().timestamp() + 3600;

        let args = crate::CreatePaymentArgs {
            payment_id: payment_id.clone(),
            merchant_id: merchant.clone(),
            amount,
            currency: Symbol::new(&env, "USDC"),
            deposit_address: Address::generate(&env),
            expires_at: Some(expires_at),
            duration_secs: None,
            memo: None,
            memo_type: None,
            token_address: None,
            client_token: None,
            metadata_hash: None,
        };

        client.create_payment(&args);

        let status = client.verify_payment(
            &oracle,
            &payment_id,
            &BytesN::<32>::random(&env),
            &Address::generate(&env),
            &(amount + delta),
        );

        let expected = if delta > PAYMENT_TOLERANCE {
            PaymentStatus::Overpaid
        } else if delta < -PAYMENT_TOLERANCE {
            PaymentStatus::PartiallyPaid
        } else {
            PaymentStatus::Confirmed
        };

        assert_eq!(status, expected);
    }
}
