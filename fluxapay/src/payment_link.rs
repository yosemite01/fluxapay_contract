use soroban_sdk::{
    contract, contractimpl, contracttype, token, vec, Address, Env, Map, MuxedAddress, String,
    Symbol, Vec,
};

use crate::PAYMENT_TOLERANCE;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentLink {
    pub link_id: String,
    pub merchant_id: Address,
    pub amount: Option<i128>,
    pub currency: Symbol,
    pub description: String,
    pub expires_at: Option<u64>,
    pub max_uses: Option<u32>,
    pub use_count: u32,
    pub active: bool,
    /// If true, funds are transferred directly to the merchant wallet on use_link,
    /// bypassing the escrow/platform wallet (issue #111).
    pub direct_transfer: bool,
    /// Optional metadata attached to this payment link.
    pub metadata: Option<Map<String, String>>,
}

#[contracttype]
pub enum LinkDataKey {
    Link(String),
}

#[contract]
pub struct PaymentLinkManager;

#[contractimpl]
#[allow(deprecated)] // events::publish — migrate to #[contractevent] in a follow-up
impl PaymentLinkManager {
    pub fn version() -> u32 {
        1
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_link(
        env: Env,
        merchant: Address,
        link_id: String,
        amount: Option<i128>,
        currency: Symbol,
        description: String,
        expires_at: Option<u64>,
        max_uses: Option<u32>,
        direct_transfer: bool,
        metadata: Option<Map<String, String>>,
    ) -> String {
        merchant.require_auth();

        let link = PaymentLink {
            link_id: link_id.clone(),
            merchant_id: merchant.clone(),
            amount,
            currency,
            description,
            expires_at,
            max_uses,
            use_count: 0,
            active: true,
            direct_transfer,
            metadata,
        };

        env.storage()
            .persistent()
            .set(&LinkDataKey::Link(link_id.clone()), &link);

        // Emit LINK/CREATED event
        env.events().publish(
            (Symbol::new(&env, "LINK"), Symbol::new(&env, "CREATED")),
            (link_id.clone(), merchant),
        );

        link_id
    }

    pub fn use_link(
        env: Env,
        payer: Address,
        link_id: String,
        amount: i128,
        usdc_token: Option<Address>,
    ) -> Result<String, crate::Error> {
        payer.require_auth();

        let mut link = Self::get_link_internal(&env, &link_id)?;

        if !link.active {
            return Err(crate::Error::Unauthorized);
        }

        if let Some(expires_at) = link.expires_at {
            if env.ledger().timestamp() > expires_at {
                return Err(crate::Error::PaymentExpired);
            }
        }

        if let Some(max_uses) = link.max_uses {
            if link.use_count >= max_uses {
                return Err(crate::Error::PaymentAlreadyProcessed);
            }
        }

        if let Some(fixed_amount) = link.amount {
            if amount != fixed_amount {
                return Err(crate::Error::InvalidAmount);
            }
        } else if amount <= 0 {
            return Err(crate::Error::InvalidAmount);
        }

        link.use_count += 1;
        env.storage()
            .persistent()
            .set(&LinkDataKey::Link(link_id.clone()), &link);

        // Issue #111: If direct_transfer is true, transfer funds directly to the merchant,
        // bypassing the escrow/platform wallet.
        if link.direct_transfer {
            let token_address = usdc_token.ok_or(crate::Error::Unauthorized)?;
            let token_client = token::TokenClient::new(&env, &token_address);
            let merchant_muxed: MuxedAddress = (&link.merchant_id).into();
            token_client.transfer(&payer, &merchant_muxed, &amount);
        }

        // Generate a virtual payment ID for tracking
        let payment_id = crate::format_id(&env, "lnk_pay_", env.ledger().timestamp());

        // Emit LINK/USED event
        env.events().publish(
            (Symbol::new(&env, "LINK"), Symbol::new(&env, "USED")),
            (link_id, payer, amount, payment_id.clone()),
        );

        Ok(payment_id)
    }

    pub fn deactivate_link(
        env: Env,
        merchant: Address,
        link_id: String,
    ) -> Result<(), crate::Error> {
        merchant.require_auth();

        let mut link = Self::get_link_internal(&env, &link_id)?;

        if link.merchant_id != merchant {
            return Err(crate::Error::Unauthorized);
        }

        link.active = false;
        env.storage()
            .persistent()
            .set(&LinkDataKey::Link(link_id.clone()), &link);

        // Emit LINK/DEACTIVATED event
        env.events().publish(
            (Symbol::new(&env, "LINK"), Symbol::new(&env, "DEACTIVATED")),
            link_id,
        );

        Ok(())
    }

    pub fn get_link(env: Env, link_id: String) -> Result<PaymentLink, crate::Error> {
        Self::get_link_internal(&env, &link_id)
    }

    fn get_link_internal(env: &Env, link_id: &String) -> Result<PaymentLink, crate::Error> {
        env.storage()
            .persistent()
            .get(&LinkDataKey::Link(link_id.clone()))
            .ok_or(crate::Error::PaymentNotFound)
    }

    /// Verify the status of multiple payment links in a single call.
    /// Returns a vector of (link_id, is_active, use_count, max_uses) tuples.
    pub fn verify_batch(env: Env, link_ids: Vec<String>) -> Vec<(String, bool, u32, Option<u32>)> {
        let mut results = vec![&env];
        for link_id in link_ids.iter() {
            match Self::get_link_internal(&env, &link_id) {
                Ok(link) => {
                    results.push_back((link_id.clone(), link.active, link.use_count, link.max_uses));
                }
                Err(_) => {
                    // Link not found - return inactive status
                    results.push_back((link_id.clone(), false, 0, None));
                }
            }
        }
        results
    }
}
