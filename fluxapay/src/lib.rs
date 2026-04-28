#![no_std]
#![allow(clippy::too_many_arguments)]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, vec, Address, BytesN, Env,
    MuxedAddress, String, Symbol, Vec,
};

pub const PAYMENT_TOLERANCE: i128 = 1;
const SHORT_LIVE_TTL: u32 = 120_960; // ~1 week at 5s/ledger
const LONG_LIVE_TTL: u32 = 18_921_600; // ~3 years at 5s/ledger
const TTL_BUMP_THRESHOLD_DIVISOR: u32 = 5;
const CREATE_PAYMENT_WINDOW_SECS: u64 = 60;
const CREATE_PAYMENT_MAX_PER_WINDOW: u32 = 30;
pub const DEFAULT_PAYMENT_DURATION_SECS: u64 = 3_600;
const REFUND_FEE_BPS: i128 = 100;
const DEFAULT_DEX_ROUTER: &[u8] = b"DEX_ROUTER_ADDRESS";

mod access_control;
pub mod fx_oracle;
mod dex_router;
use access_control::{
    role_admin, role_merchant, role_oracle, role_settlement_operator, AccessControl,
};
// Re-export for tests
#[allow(unused_imports)]
pub use access_control::AccessControlDataKey;
pub use fx_oracle::{FXOracle, FXOracleClient, FXOracleError};
pub use dex_router::{DexRouter, DexRouterClient};

#[contract]
pub struct PaymentProcessor;

#[contract]
pub struct RefundManager;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentCharge {
    pub payment_id: String,
    pub merchant_id: Address,
    pub amount: i128,
    pub currency: Symbol,
    pub deposit_address: Address,
    pub status: PaymentStatus,
    pub payer_address: Option<Address>,
    pub transaction_hash: Option<BytesN<32>>,
    pub created_at: u64,
    pub confirmed_at: Option<u64>,
    pub expires_at: u64,
    /// Actual amount received on-chain; set by verify_payment for reconciliation.
    pub amount_received: Option<i128>,
    /// Optional memo for Stellar payment routing.
    pub memo: Option<String>,
    /// Optional memo type: Text, Id, Hash, or Return.
    pub memo_type: Option<String>,
    /// Token contract address used for this payment (None defaults to the configured USDC token).
    pub token_address: Option<Address>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaymentStatus {
    Pending,
    Confirmed,
    Settled,
    Expired,
    Failed,
    /// Customer sent less than the required amount (within tolerance but below threshold).
    PartiallyPaid,
    /// Customer sent more than the required amount (e.g. tip or rounding).
    Overpaid,
}


#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refund {
    pub refund_id: String,
    pub payment_id: String,
    pub amount: i128,
    pub reason: String,
    pub status: RefundStatus,
    pub requester: Address,
    pub created_at: u64,
    pub processed_at: Option<u64>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefundStatus {
    Pending,
    Completed,
    Rejected,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DisputeStatus {
    Open,
    UnderReview,
    Resolved,
    Rejected,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dispute {
    pub dispute_id: String,
    pub payment_id: String,
    pub refund_id: Option<String>,
    pub amount: i128,
    pub reason: String,
    pub evidence: String,
    pub status: DisputeStatus,
    pub disputer: Address,
    pub created_at: u64,
    pub resolved_at: Option<u64>,
    pub resolution_notes: Option<String>,
    /// Operator-set deadline (Unix timestamp) by which the dispute must be resolved.
    pub review_deadline: Option<u64>,
    /// True when the dispute has been flagged for escalation (e.g. deadline exceeded).
    pub escalated: bool,
}

#[contracterror]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    PaymentNotFound = 404,
    RefundNotFound = 405,
    InvalidAmount = 406,
    Unauthorized = 1,
    PaymentAlreadyExists = 2,
    PaymentExpired = 3,
    InvalidPaymentId = 4,
    RefundAlreadyProcessed = 8,
    DisputeNotFound = 9,
    DisputeAlreadyResolved = 12,
    PaymentAlreadyProcessed = 14,
    AccessControlError = 15,
    RefundExceedsPayment = 16,
    ContractPaused = 17,
    RateLimitExceeded = 18,
    RefundCancelled = 19,
    UnsupportedToken = 20,
    AmountBelowMin = 21,
    AmountAboveMax = 22,
    InvalidExpiry = 23,
    InvalidSettlement = 24,
    DuplicateIdempotencyKey = 25,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatePaymentArgs {
    pub payment_id: String,
    pub merchant_id: Address,
    pub amount: i128,
    pub currency: Symbol,
    pub deposit_address: Address,
    pub expires_at: Option<u64>,
    pub duration_secs: Option<u64>,
    pub memo: Option<String>,
    pub memo_type: Option<String>,
    pub token_address: Option<Address>,
    pub client_token: Option<String>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwapAndPayArgs {
    pub payer: Address,
    pub payment_id: String,
    pub merchant_id: Address,
    pub amount: i128,
    pub currency: Symbol,
    pub deposit_address: Address,
    pub token_in: Address,
    pub amount_in: i128,
    pub amount_out_min: i128,
    pub path: Vec<Address>,
    pub expires_at: Option<u64>,
    pub dex_router: Address,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseState {
    pub paused: bool,
    pub reason: String,
    pub admin: Option<Address>,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseInfo {
    pub global: PauseState,
    pub creation: PauseState,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MerchantCreateRateLimit {
    pub last_payment_at: u64,
    pub count: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmountLimits {
    pub min: Option<i128>,
    pub max: Option<i128>,
}

/// A single recipient in a multi-account settlement split.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettlementSplit {
    pub recipient: Address,
    pub amount: i128,
}

/// Configuration for creating a payment.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentConfig {
    /// Optional memo for Stellar payment routing.
    pub memo: Option<String>,
    /// Optional memo type: Text, Id, Hash, or Return.
    pub memo_type: Option<String>,
    /// Token contract address used for this payment (None defaults to the configured USDC token).
    pub token_address: Option<Address>,
    /// Optional idempotency key. If provided, retrying with the same key and payment_id
    /// returns the existing payment. Using the same key with a different payment_id
    /// returns `DuplicateIdempotencyKey`.
    pub client_token: Option<String>,
}



#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionStatus {
    Active,
    Paused,
    Cancelled,
    Expired,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subscription {
    pub subscription_id: String,
    pub merchant_id: Address,
    pub payer_address: Address,
    pub plan_id: String,
    pub amount: i128,
    pub currency: Symbol,
    pub interval_secs: u64,
    pub next_payment_at: u64,
    pub status: SubscriptionStatus,
    pub created_at: u64,
    pub last_payment_at: Option<u64>,
    pub total_payments: u32,
    pub max_payments: Option<u32>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionPlan {
    pub plan_id: String,
    pub merchant_id: Address,
    pub name: String,
    pub description: String,
    pub amount: i128,
    pub currency: Symbol,
    pub interval_secs: u64,
    pub active: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WithdrawalRecipient {
    pub stream_id: String,
    pub destination: Address,
    pub amount: i128,
}

#[contracttype]
pub enum DataKey {
    Payment(String),
    MerchantPayments(Address),
    MerchantRateLimit(Address),
    Refund(String),
    PaymentRefunds(String),
    RefundCounter,
    Dispute(String),
    PaymentDisputes(String),
    DisputeCounter,
    Stream(String),
    UsdcToken,
    Paused,
    CreationPaused,
    MerchantRegistryAddress,
    AllowedToken(Address),
    MerchantAmountLimits(Address),
    GlobalAmountLimits,
    IdempotencyKey(String),
    SubscriptionPlan(String),
    Subscription(String),
    PayerSubscriptions(Address),
    SubscriptionCounter,
    StreamCounter,
}


#[contractimpl]
#[allow(deprecated)] // events::publish — migrate to #[contractevent] in a follow-up
impl RefundManager {
    pub fn version() -> u32 {
        1
    }

    pub fn initialize_refund_manager(env: Env, admin: Address, usdc_token_address: Address) {
        AccessControl::initialize(&env, admin);
        env.storage()
            .persistent()
            .set(&DataKey::UsdcToken, &usdc_token_address);
    }

    pub fn grant_role(
        env: Env,
        admin: Address,
        role: Symbol,
        account: Address,
    ) -> Result<(), Error> {
        AccessControl::grant_role(&env, admin, role, account).map_err(|_| Error::AccessControlError)
    }

    pub fn revoke_role(
        env: Env,
        admin: Address,
        role: Symbol,
        account: Address,
    ) -> Result<(), Error> {
        AccessControl::revoke_role(&env, admin, role, account)
            .map_err(|_| Error::AccessControlError)
    }

    pub fn has_role(env: Env, role: Symbol, account: Address) -> bool {
        AccessControl::has_role(&env, &role, &account)
    }

    pub fn renounce_role(env: Env, account: Address, role: Symbol) -> Result<(), Error> {
        AccessControl::renounce_role(&env, account, role).map_err(|_| Error::AccessControlError)
    }

    pub fn transfer_admin(
        env: Env,
        current_admin: Address,
        new_admin: Address,
    ) -> Result<(), Error> {
        AccessControl::transfer_admin(&env, current_admin, new_admin)
            .map_err(|_| Error::AccessControlError)
    }

    pub fn get_admin(env: Env) -> Option<Address> {
        AccessControl::get_admin(&env)
    }

    /// Returns all addresses currently holding the given role (issue #37).
    pub fn get_role_members(env: Env, role: Symbol) -> Vec<Address> {
        AccessControl::get_role_members(&env, &role)
    }

    /// Register a payment with the refund manager so refund amounts can be validated.
    pub fn register_payment(
        env: Env,
        payment_id: String,
        merchant_id: Address,
        amount: i128,
        currency: Symbol,
    ) {
        if !env
            .storage()
            .persistent()
            .has(&DataKey::Payment(payment_id.clone()))
        {
            let payment = PaymentCharge {
                payment_id: payment_id.clone(),
                merchant_id,
                amount,
                currency,
                deposit_address: env.current_contract_address(),
                status: PaymentStatus::Confirmed,
                payer_address: None,
                transaction_hash: None,
                created_at: env.ledger().timestamp(),
                confirmed_at: None,
                expires_at: 0,
                amount_received: None,
                memo: None,
                memo_type: None,
                token_address: None,
            };
            env.storage()
                .persistent()
                .set(&DataKey::Payment(payment_id.clone()), &payment);
            Self::bump_payment_ttl(&env, &payment_id, &payment.status);
        }
    }

    pub fn create_refund(
        env: Env,
        payment_id: String,
        refund_amount: i128,
        reason: String,
        requester: Address,
    ) -> Result<String, Error> {
        requester.require_auth();
        Self::create_refund_internal(&env, payment_id, refund_amount, reason, requester)
    }

    fn create_refund_internal(
        env: &Env,
        payment_id: String,
        refund_amount: i128,
        reason: String,
        requester: Address,
    ) -> Result<String, Error> {
        if refund_amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        // Validate refund amount does not exceed original payment amount
        // First try to get payment from local storage
        let payment: PaymentCharge = if let Some(local_payment) =
            env.storage()
                .persistent()
                .get::<DataKey, PaymentCharge>(&DataKey::Payment(payment_id.clone()))
        {
            local_payment
        } else {
            return Err(Error::PaymentNotFound);
        };

        // Issue #76: Reject refunds unless payment.status == Confirmed
        if payment.status != PaymentStatus::Confirmed {
            return Err(Error::PaymentAlreadyProcessed);
        }

        // Sum existing refund amounts for this payment
        let existing_refunds = Self::get_payment_refunds_internal(env, &payment_id);
        let mut total_refunded: i128 = 0;
        for id in existing_refunds.iter() {
            if let Ok(r) = Self::get_refund_internal(env, &id) {
                if r.status != RefundStatus::Rejected {
                    total_refunded += r.amount;
                }
            }
        }

        if total_refunded + refund_amount > payment.amount {
            return Err(Error::RefundExceedsPayment);
        }

        let counter = Self::get_next_refund_id(env);

        // Build refund ID: "refund_" + counter
        // For simplicity and to avoid complex string manipulation in no_std,
        // we use a match statement for common cases
        let refund_id = format_id(env, "refund_", counter);

        let refund = Refund {
            refund_id: refund_id.clone(),
            payment_id: payment_id.clone(),
            amount: refund_amount,
            reason,
            status: RefundStatus::Pending,
            requester,
            created_at: env.ledger().timestamp(),
            processed_at: None,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Refund(refund_id.clone()), &refund);

        let mut payment_refunds = Self::get_payment_refunds_internal(env, &payment_id);
        payment_refunds.push_back(refund_id.clone());
        env.storage().persistent().set(
            &DataKey::PaymentRefunds(payment_id.clone()),
            &payment_refunds,
        );
        Self::bump_ttl(
            env,
            &DataKey::PaymentRefunds(payment_id.clone()),
            LONG_LIVE_TTL,
        );

        Self::bump_refund_ttl(env, &refund_id, &refund.status);

        // Issue #27: emit REFUND/CREATED event
        env.events().publish(
            (Symbol::new(env, "REFUND"), Symbol::new(env, "CREATED")),
            (payment_id, refund_id.clone(), refund_amount),
        );

        Ok(refund_id)
    }

    pub fn process_refund(env: Env, operator: Address, refund_id: String) -> Result<(), Error> {
        operator.require_auth();
        let has_settlement =
            AccessControl::has_role(&env, &role_settlement_operator(&env), &operator);
        let has_oracle = AccessControl::has_role(&env, &role_oracle(&env), &operator);

        if !has_settlement && !has_oracle {
            return Err(Error::Unauthorized);
        }

        Self::process_refund_internal(&env, &operator, refund_id)
    }

    fn process_refund_internal(
        env: &Env,
        _operator: &Address,
        refund_id: String,
    ) -> Result<(), Error> {
        let mut refund = Self::get_refund_internal(env, &refund_id)?;

        if refund.status != RefundStatus::Pending {
            return Err(Error::RefundAlreadyProcessed);
        }

        let usdc_token_address: Address = env
            .storage()
            .persistent()
            .get(&DataKey::UsdcToken)
            .ok_or(Error::Unauthorized)?;
        let token_client = token::TokenClient::new(env, &usdc_token_address);

        let fee = refund.amount * REFUND_FEE_BPS / 10_000;
        let net_amount = refund.amount - fee;

        let from = env.current_contract_address();
        let to: MuxedAddress = (&refund.requester).into();

        refund.status = RefundStatus::Completed;
        refund.processed_at = Some(env.ledger().timestamp());

        // Persist state before interaction (reentrancy protection)
        env.storage()
            .persistent()
            .set(&DataKey::Refund(refund_id.clone()), &refund);
        Self::bump_refund_ttl(env, &refund_id, &refund.status);

        // Interaction: Transfer net amount to requester
        if token_client.try_transfer(&from, &to, &net_amount).is_err() {
            // If transfer fails, we currently return Ok(()) but state is already updated.
            // In a more robust system we might want to revert or handle failures differently.
            return Ok(());
        }

        // Interaction: Transfer fee to admin
        if fee > 0 {
            if let Some(admin) = AccessControl::get_admin(env) {
                let admin_muxed: MuxedAddress = (&admin).into();
                let _ = token_client.try_transfer(&from, &admin_muxed, &fee);
            }
        }
        env.events().publish(
            (Symbol::new(env, "REFUND"), Symbol::new(env, "COMPLETED")),
            (refund.payment_id, refund_id, refund.amount),
        );

        Ok(())
    }

    /// Reject a pending refund (operator only). Emits REFUND/REJECTED (issue #27).
    pub fn reject_refund(env: Env, operator: Address, refund_id: String) -> Result<(), Error> {
        operator.require_auth();
        let has_settlement =
            AccessControl::has_role(&env, &role_settlement_operator(&env), &operator);
        let has_oracle = AccessControl::has_role(&env, &role_oracle(&env), &operator);

        if !has_settlement && !has_oracle {
            return Err(Error::Unauthorized);
        }

        let mut refund = Self::get_refund_internal(&env, &refund_id)?;

        if refund.status != RefundStatus::Pending {
            return Err(Error::RefundAlreadyProcessed);
        }

        refund.status = RefundStatus::Rejected;
        refund.processed_at = Some(env.ledger().timestamp());

        env.storage()
            .persistent()
            .set(&DataKey::Refund(refund_id.clone()), &refund);
        Self::bump_refund_ttl(&env, &refund_id, &refund.status);

        // Issue #27: emit REFUND/REJECTED event
        env.events().publish(
            (Symbol::new(&env, "REFUND"), Symbol::new(&env, "REJECTED")),
            (refund.payment_id, refund_id, refund.amount),
        );

        Ok(())
    }

    /// Cancel a pending refund. Caller must be the refund requester (merchant) or contract admin.
    /// Removes the refund from the payment's pending list and emits REFUND/CANCELLED.
    pub fn cancel_refund(env: Env, caller: Address, refund_id: String) -> Result<(), Error> {
        caller.require_auth();

        let refund = Self::get_refund_internal(&env, &refund_id)?;

        if refund.status != RefundStatus::Pending {
            return Err(Error::RefundAlreadyProcessed);
        }

        let is_requester = caller == refund.requester;
        let is_admin = AccessControl::has_role(&env, &role_admin(&env), &caller);
        if !is_requester && !is_admin {
            return Err(Error::Unauthorized);
        }

        // Remove from payment's refund list
        let existing = Self::get_payment_refunds_internal(&env, &refund.payment_id);
        let mut updated = vec![&env];
        for id in existing.iter() {
            if id != refund_id {
                updated.push_back(id);
            }
        }
        env.storage().persistent().set(
            &DataKey::PaymentRefunds(refund.payment_id.clone()),
            &updated,
        );

        // Remove the refund record
        env.storage()
            .persistent()
            .remove(&DataKey::Refund(refund_id.clone()));

        env.events().publish(
            (Symbol::new(&env, "REFUND"), Symbol::new(&env, "CANCELLED")),
            (refund.payment_id, refund_id, refund.amount),
        );

        Ok(())
    }

    pub fn get_refund(env: Env, refund_id: String) -> Result<Refund, Error> {
        Self::get_refund_internal(&env, &refund_id)
    }

    pub fn get_payment_refunds(env: Env, payment_id: String) -> Result<Vec<Refund>, Error> {
        let refund_ids = Self::get_payment_refunds_internal(&env, &payment_id);
        let mut refunds = vec![&env];
        for id in refund_ids.iter() {
            if let Ok(refund) = Self::get_refund_internal(&env, &id) {
                refunds.push_back(refund);
            }
        }
        Ok(refunds)
    }

    fn get_next_refund_id(env: &Env) -> u64 {
        let mut counter: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::RefundCounter)
            .unwrap_or(0);
        counter += 1;
        env.storage()
            .persistent()
            .set(&DataKey::RefundCounter, &counter);
        counter
    }

    fn get_refund_internal(env: &Env, refund_id: &String) -> Result<Refund, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Refund(refund_id.clone()))
            .ok_or(Error::RefundNotFound)
    }

    fn get_payment_refunds_internal(env: &Env, payment_id: &String) -> Vec<String> {
        env.storage()
            .persistent()
            .get(&DataKey::PaymentRefunds(payment_id.clone()))
            .unwrap_or_else(|| vec![env])
    }

    // Dispute handling functions
    pub fn create_dispute(
        env: Env,
        payment_id: String,
        amount: i128,
        reason: String,
        evidence: String,
        disputer: Address,
    ) -> Result<String, Error> {
        disputer.require_auth();

        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        // Issue #77: Load payment and cap dispute amount to confirmed payment amount
        let payment: PaymentCharge = env
            .storage()
            .persistent()
            .get(&DataKey::Payment(payment_id.clone()))
            .ok_or(Error::PaymentNotFound)?;

        // Ensure payment is confirmed
        if payment.status != PaymentStatus::Confirmed {
            return Err(Error::PaymentAlreadyProcessed);
        }

        // Cap dispute amount to payment amount
        if amount > payment.amount {
            return Err(Error::InvalidAmount);
        }

        // Sum open disputes + prior refunds for the same payment_id
        let existing_disputes = Self::get_payment_disputes_internal(&env, &payment_id);
        let mut total_disputed: i128 = 0;
        for id in existing_disputes.iter() {
            if let Ok(d) = Self::get_dispute_internal(&env, &id) {
                if d.status != DisputeStatus::Rejected {
                    total_disputed += d.amount;
                }
            }
        }

        let existing_refunds = Self::get_payment_refunds_internal(&env, &payment_id);
        let mut total_refunded: i128 = 0;
        for id in existing_refunds.iter() {
            if let Ok(r) = Self::get_refund_internal(&env, &id) {
                if r.status != RefundStatus::Rejected {
                    total_refunded += r.amount;
                }
            }
        }

        // Ensure totals stay within payment.amount
        if total_disputed + total_refunded + amount > payment.amount {
            return Err(Error::RefundExceedsPayment);
        }

        let counter = Self::get_next_dispute_id(&env);
        let dispute_id = Self::build_dispute_id(&env, counter);

        let dispute = Dispute {
            dispute_id: dispute_id.clone(),
            payment_id: payment_id.clone(),
            refund_id: None,
            amount,
            reason,
            evidence,
            status: DisputeStatus::Open,
            disputer,
            created_at: env.ledger().timestamp(),
            resolved_at: None,
            resolution_notes: None,
            review_deadline: None,
            escalated: false,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Dispute(dispute_id.clone()), &dispute);

        let mut payment_disputes = Self::get_payment_disputes_internal(&env, &payment_id);
        payment_disputes.push_back(dispute_id.clone());
        env.storage().persistent().set(
            &DataKey::PaymentDisputes(payment_id.clone()),
            &payment_disputes,
        );
        Self::bump_ttl(
            &env,
            &DataKey::PaymentDisputes(payment_id.clone()),
            LONG_LIVE_TTL,
        );

        Self::bump_dispute_ttl(&env, &dispute_id, &dispute.status);

        // Issue #27: emit DISPUTE_CREATED event
        env.events().publish(
            (Symbol::new(&env, "DISPUTE"), Symbol::new(&env, "CREATED")),
            (dispute_id.clone(), payment_id),
        );

        Ok(dispute_id)
    }

    pub fn review_dispute(env: Env, operator: Address, dispute_id: String) -> Result<(), Error> {
        operator.require_auth();

        let has_settlement =
            AccessControl::has_role(&env, &role_settlement_operator(&env), &operator);
        let has_oracle = AccessControl::has_role(&env, &role_oracle(&env), &operator);

        if !has_settlement && !has_oracle {
            return Err(Error::Unauthorized);
        }

        let mut dispute = Self::get_dispute_internal(&env, &dispute_id)?;

        if dispute.status != DisputeStatus::Open {
            return Err(Error::DisputeAlreadyResolved);
        }

        dispute.status = DisputeStatus::UnderReview;

        env.storage()
            .persistent()
            .set(&DataKey::Dispute(dispute_id.clone()), &dispute);
        Self::bump_dispute_ttl(&env, &dispute_id, &dispute.status);

        // Issue #27: emit DISPUTE_REVIEWED event
        env.events().publish(
            (
                Symbol::new(&env, "DISPUTE"),
                Symbol::new(&env, "REVIEWED"),
            ),
            (dispute_id, dispute.payment_id),
        );

        Ok(())
    }

    /// Operator-only: set or update the review deadline for an open or under-review dispute.
    /// Emits DISPUTE/DEADLINE_SET. If the current ledger time already exceeds the deadline,
    /// the dispute is also flagged as escalated and DISPUTE/ESCALATED is emitted.
    pub fn set_dispute_deadline(
        env: Env,
        operator: Address,
        dispute_id: String,
        deadline: u64,
    ) -> Result<(), Error> {
        operator.require_auth();

        let has_settlement =
            AccessControl::has_role(&env, &role_settlement_operator(&env), &operator);
        let has_oracle = AccessControl::has_role(&env, &role_oracle(&env), &operator);

        if !has_settlement && !has_oracle {
            return Err(Error::Unauthorized);
        }

        let mut dispute = Self::get_dispute_internal(&env, &dispute_id)?;

        if dispute.status == DisputeStatus::Resolved || dispute.status == DisputeStatus::Rejected {
            return Err(Error::DisputeAlreadyResolved);
        }

        dispute.review_deadline = Some(deadline);

        let now = env.ledger().timestamp();
        if now > deadline && !dispute.escalated {
            dispute.escalated = true;
            env.storage()
                .persistent()
                .set(&DataKey::Dispute(dispute_id.clone()), &dispute);
            Self::bump_dispute_ttl(&env, &dispute_id, &dispute.status);
            env.events().publish(
                (Symbol::new(&env, "DISPUTE"), Symbol::new(&env, "ESCALATED")),
                (
                    dispute.payment_id.clone(),
                    dispute_id.clone(),
                    dispute.amount,
                ),
            );
        } else {
            env.storage()
                .persistent()
                .set(&DataKey::Dispute(dispute_id.clone()), &dispute);
            Self::bump_dispute_ttl(&env, &dispute_id, &dispute.status);
        }

        env.events().publish(
            (
                Symbol::new(&env, "DISPUTE"),
                Symbol::new(&env, "DEADLINE_SET"),
            ),
            (dispute.payment_id, dispute_id, deadline),
        );

        Ok(())
    }

    pub fn resolve_dispute_with_refund(
        env: Env,
        operator: Address,
        dispute_id: String,
        resolution_notes: String,
    ) -> Result<String, Error> {
        operator.require_auth();

        let has_settlement =
            AccessControl::has_role(&env, &role_settlement_operator(&env), &operator);
        let has_oracle = AccessControl::has_role(&env, &role_oracle(&env), &operator);

        if !has_settlement && !has_oracle {
            return Err(Error::Unauthorized);
        }

        let mut dispute = Self::get_dispute_internal(&env, &dispute_id)?;

        if dispute.status == DisputeStatus::Resolved || dispute.status == DisputeStatus::Rejected {
            return Err(Error::DisputeAlreadyResolved);
        }

        // Create refund for the disputed amount
        let refund_reason = String::from_str(&env, "Refund issued due to dispute resolution");

        let refund_id = Self::create_refund_internal(
            &env,
            dispute.payment_id.clone(),
            dispute.amount,
            refund_reason,
            dispute.disputer.clone(),
        )?;

        // Process the refund immediately
        Self::process_refund_internal(&env, &operator, refund_id.clone())?;

        // Update dispute status
        dispute.status = DisputeStatus::Resolved;
        dispute.refund_id = Some(refund_id.clone());
        dispute.resolved_at = Some(env.ledger().timestamp());
        dispute.resolution_notes = Some(resolution_notes);

        env.storage()
            .persistent()
            .set(&DataKey::Dispute(dispute_id.clone()), &dispute);
        Self::bump_dispute_ttl(&env, &dispute_id, &dispute.status);

        // Issue #27: emit DISPUTE_RESOLVED event
        env.events().publish(
            (Symbol::new(&env, "DISPUTE"), Symbol::new(&env, "RESOLVED")),
            (dispute_id, dispute.payment_id),
        );

        Ok(refund_id)
    }

    pub fn reject_dispute(
        env: Env,
        operator: Address,
        dispute_id: String,
        resolution_notes: String,
    ) -> Result<(), Error> {
        operator.require_auth();

        let has_settlement =
            AccessControl::has_role(&env, &role_settlement_operator(&env), &operator);
        let has_oracle = AccessControl::has_role(&env, &role_oracle(&env), &operator);

        if !has_settlement && !has_oracle {
            return Err(Error::Unauthorized);
        }

        let mut dispute = Self::get_dispute_internal(&env, &dispute_id)?;

        if dispute.status == DisputeStatus::Resolved || dispute.status == DisputeStatus::Rejected {
            return Err(Error::DisputeAlreadyResolved);
        }

        dispute.status = DisputeStatus::Rejected;
        dispute.resolved_at = Some(env.ledger().timestamp());
        dispute.resolution_notes = Some(resolution_notes);

        env.storage()
            .persistent()
            .set(&DataKey::Dispute(dispute_id.clone()), &dispute);
        Self::bump_dispute_ttl(&env, &dispute_id, &dispute.status);

        // Issue #27: emit DISPUTE_REJECTED event
        env.events().publish(
            (Symbol::new(&env, "DISPUTE"), Symbol::new(&env, "REJECTED")),
            (dispute_id, dispute.payment_id),
        );

        Ok(())
    }

    pub fn get_dispute(env: Env, dispute_id: String) -> Result<Dispute, Error> {
        Self::get_dispute_internal(&env, &dispute_id)
    }

    pub fn get_payment_disputes(env: Env, payment_id: String) -> Result<Vec<Dispute>, Error> {
        let dispute_ids = Self::get_payment_disputes_internal(&env, &payment_id);
        let mut disputes = vec![&env];
        for id in dispute_ids.iter() {
            if let Ok(dispute) = Self::get_dispute_internal(&env, &id) {
                disputes.push_back(dispute);
            }
        }
        Ok(disputes)
    }

    fn get_next_dispute_id(env: &Env) -> u64 {
        let mut counter: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::DisputeCounter)
            .unwrap_or(0);
        counter += 1;
        env.storage()
            .persistent()
            .set(&DataKey::DisputeCounter, &counter);
        counter
    }

    fn build_dispute_id(env: &Env, counter: u64) -> String {
        format_id(env, "dispute_", counter)
    }

    fn get_dispute_internal(env: &Env, dispute_id: &String) -> Result<Dispute, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Dispute(dispute_id.clone()))
            .ok_or(Error::DisputeNotFound)
    }

    fn get_payment_disputes_internal(env: &Env, payment_id: &String) -> Vec<String> {
        env.storage()
            .persistent()
            .get(&DataKey::PaymentDisputes(payment_id.clone()))
            .unwrap_or_else(|| vec![env])
    }

    // Subscription management functions
    pub fn create_subscription_plan(
        env: Env,
        merchant: Address,
        plan_id: String,
        name: String,
        description: String,
        amount: i128,
        currency: Symbol,
        interval_secs: u64,
    ) -> Result<(), Error> {
        merchant.require_auth();

        if !AccessControl::has_role(&env, &role_merchant(&env), &merchant) {
            return Err(Error::Unauthorized);
        }

        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        if interval_secs == 0 {
            return Err(Error::InvalidAmount);
        }

        let plan = SubscriptionPlan {
            plan_id: plan_id.clone(),
            merchant_id: merchant,
            name,
            description,
            amount,
            currency,
            interval_secs,
            active: true,
        };

        env.storage()
            .persistent()
            .set(&DataKey::SubscriptionPlan(plan_id), &plan);

        Ok(())
    }

    pub fn get_subscription_plan(env: Env, plan_id: String) -> Result<SubscriptionPlan, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::SubscriptionPlan(plan_id))
            .ok_or(Error::PaymentNotFound)
    }

    pub fn deactivate_subscription_plan(
        env: Env,
        merchant: Address,
        plan_id: String,
    ) -> Result<(), Error> {
        merchant.require_auth();

        let mut plan: SubscriptionPlan = env
            .storage()
            .persistent()
            .get(&DataKey::SubscriptionPlan(plan_id.clone()))
            .ok_or(Error::PaymentNotFound)?;

        if plan.merchant_id != merchant {
            return Err(Error::Unauthorized);
        }

        plan.active = false;
        env.storage()
            .persistent()
            .set(&DataKey::SubscriptionPlan(plan_id), &plan);

        Ok(())
    }

    pub fn subscribe(
        env: Env,
        payer: Address,
        plan_id: String,
        max_payments: Option<u32>,
    ) -> Result<String, Error> {
        payer.require_auth();

        let plan: SubscriptionPlan = env
            .storage()
            .persistent()
            .get(&DataKey::SubscriptionPlan(plan_id.clone()))
            .ok_or(Error::PaymentNotFound)?;

        if !plan.active {
            return Err(Error::PaymentAlreadyProcessed);
        }

        let counter = Self::get_next_subscription_id(&env);
        let subscription_id = format_id(&env, "sub_", counter);

        let now = env.ledger().timestamp();
        let subscription = Subscription {
            subscription_id: subscription_id.clone(),
            merchant_id: plan.merchant_id.clone(),
            payer_address: payer.clone(),
            plan_id: plan_id.clone(),
            amount: plan.amount,
            currency: plan.currency,
            interval_secs: plan.interval_secs,
            next_payment_at: now.saturating_add(plan.interval_secs),
            status: SubscriptionStatus::Active,
            created_at: now,
            last_payment_at: None,
            total_payments: 0,
            max_payments,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Subscription(subscription_id.clone()), &subscription);

        let mut payer_subscriptions =
            Self::get_payer_subscriptions_internal(&env, &payer);
        payer_subscriptions.push_back(subscription_id.clone());
        env.storage().persistent().set(
            &DataKey::PayerSubscriptions(payer.clone()),
            &payer_subscriptions,
        );

        env.events().publish(
            (Symbol::new(&env, "SUBSCRIPTION"), Symbol::new(&env, "CREATED")),
            (subscription_id.clone(), payer, plan_id),
        );

        Ok(subscription_id)
    }

    pub fn get_subscription(
        env: Env,
        subscription_id: String,
    ) -> Result<Subscription, Error> {
        Self::get_subscription_internal(&env, &subscription_id)
    }

    pub fn get_payer_subscriptions(env: Env, payer: Address) -> Vec<Subscription> {
        let subscription_ids = Self::get_payer_subscriptions_internal(&env, &payer);
        let mut subscriptions = vec![&env];
        for id in subscription_ids.iter() {
            if let Ok(sub) = Self::get_subscription_internal(&env, &id) {
                subscriptions.push_back(sub);
            }
        }
        subscriptions
    }

    pub fn pause_subscription(
        env: Env,
        payer: Address,
        subscription_id: String,
    ) -> Result<(), Error> {
        payer.require_auth();

        let mut subscription =
            Self::get_subscription_internal(&env, &subscription_id)?;

        if subscription.payer_address != payer {
            return Err(Error::Unauthorized);
        }

        if subscription.status != SubscriptionStatus::Active {
            return Err(Error::PaymentAlreadyProcessed);
        }

        subscription.status = SubscriptionStatus::Paused;
        env.storage()
            .persistent()
            .set(&DataKey::Subscription(subscription_id), &subscription);

        Ok(())
    }

    pub fn resume_subscription(
        env: Env,
        payer: Address,
        subscription_id: String,
    ) -> Result<(), Error> {
        payer.require_auth();

        let mut subscription =
            Self::get_subscription_internal(&env, &subscription_id)?;

        if subscription.payer_address != payer {
            return Err(Error::Unauthorized);
        }

        if subscription.status != SubscriptionStatus::Paused {
            return Err(Error::PaymentAlreadyProcessed);
        }

        subscription.status = SubscriptionStatus::Active;
        subscription.next_payment_at = env.ledger().timestamp().saturating_add(subscription.interval_secs);
        env.storage()
            .persistent()
            .set(&DataKey::Subscription(subscription_id), &subscription);

        Ok(())
    }

    pub fn cancel_subscription(
        env: Env,
        payer: Address,
        subscription_id: String,
    ) -> Result<(), Error> {
        payer.require_auth();

        let mut subscription =
            Self::get_subscription_internal(&env, &subscription_id)?;

        if subscription.payer_address != payer {
            return Err(Error::Unauthorized);
        }

        subscription.status = SubscriptionStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::Subscription(subscription_id), &subscription);

        Ok(())
    }

    /// Process due subscriptions - called by an operator or oracle
    pub fn process_due_subscriptions(env: Env, operator: Address) -> Result<u32, Error> {
        operator.require_auth();

        if !AccessControl::has_role(&env, &role_oracle(&env), &operator)
            && !AccessControl::has_role(&env, &role_settlement_operator(&env), &operator)
        {
            return Err(Error::Unauthorized);
        }

        let now = env.ledger().timestamp();
        let mut processed_count = 0u32;

        // Note: In a real implementation, you'd want to iterate through subscriptions
        // more efficiently. This is a simplified version.
        // The actual implementation would need a way to track which subscriptions to check.

        Ok(processed_count)
    }

    fn get_next_subscription_id(env: &Env) -> u64 {
        let mut counter: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::SubscriptionCounter)
            .unwrap_or(0);
        counter += 1;
        env.storage()
            .persistent()
            .set(&DataKey::SubscriptionCounter, &counter);
        counter
    }

    fn get_subscription_internal(
        env: &Env,
        subscription_id: &String,
    ) -> Result<Subscription, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Subscription(subscription_id.clone()))
            .ok_or(Error::PaymentNotFound)
    }

    fn get_payer_subscriptions_internal(env: &Env, payer: &Address) -> Vec<String> {
        env.storage()
            .persistent()
            .get(&DataKey::PayerSubscriptions(payer.clone()))
            .unwrap_or_else(|| vec![env])
    }

    fn refund_ttl(status: &RefundStatus) -> u32 {
        match status {
            RefundStatus::Pending => SHORT_LIVE_TTL,
            RefundStatus::Completed | RefundStatus::Rejected => LONG_LIVE_TTL,
        }
    }

    fn bump_refund_ttl(env: &Env, refund_id: &String, status: &RefundStatus) {
        let key = DataKey::Refund(refund_id.clone());
        Self::bump_ttl(env, &key, Self::refund_ttl(status));
    }

    fn dispute_ttl(status: &DisputeStatus) -> u32 {
        match status {
            DisputeStatus::Open | DisputeStatus::UnderReview => SHORT_LIVE_TTL,
            DisputeStatus::Resolved | DisputeStatus::Rejected => LONG_LIVE_TTL,
        }
    }

    fn bump_dispute_ttl(env: &Env, dispute_id: &String, status: &DisputeStatus) {
        let key = DataKey::Dispute(dispute_id.clone());
        Self::bump_ttl(env, &key, Self::dispute_ttl(status));
    }

    fn payment_ttl(status: &PaymentStatus) -> u32 {
        match status {
            PaymentStatus::Pending => SHORT_LIVE_TTL,
            PaymentStatus::Confirmed
            | PaymentStatus::Settled
            | PaymentStatus::Expired
            | PaymentStatus::Failed
            | PaymentStatus::PartiallyPaid
            | PaymentStatus::Overpaid => LONG_LIVE_TTL,
        }
    }

    fn bump_payment_ttl(env: &Env, payment_id: &String, status: &PaymentStatus) {
        let key = DataKey::Payment(payment_id.clone());
        Self::bump_ttl(env, &key, Self::payment_ttl(status));
    }

    fn bump_ttl(env: &Env, key: &DataKey, ttl: u32) {
        let threshold = core::cmp::max(1, ttl / TTL_BUMP_THRESHOLD_DIVISOR);
        env.storage().persistent().extend_ttl(key, threshold, ttl);
    }
}

#[contractimpl]
#[allow(deprecated)] // events::publish — migrate to #[contractevent] in a follow-up
impl PaymentProcessor {
    pub fn version() -> u32 {
        1
    }

    pub fn initialize_payment_processor(env: Env, admin: Address) {
        AccessControl::initialize(&env, admin);
        
        let empty_reason = String::from_str(&env, "");
        let initial_state = PauseState {
            paused: false,
            reason: empty_reason,
            admin: None,
            timestamp: env.ledger().timestamp(),
        };
        
        env.storage().persistent().set(&DataKey::Paused, &initial_state);
        env.storage().persistent().set(&DataKey::CreationPaused, &initial_state);
    }

    pub fn set_merchant_registry_address(
        env: Env,
        admin: Address,
        registry_address: Address,
    ) -> Result<(), Error> {
        admin.require_auth();

        if !AccessControl::has_role(&env, &role_admin(&env), &admin) {
            return Err(Error::Unauthorized);
        }

        env.storage()
            .persistent()
            .set(&DataKey::MerchantRegistryAddress, &registry_address);

        Ok(())
    }

    pub fn grant_role(
        env: Env,
        admin: Address,
        role: Symbol,
        account: Address,
    ) -> Result<(), Error> {
        AccessControl::grant_role(&env, admin, role, account).map_err(|_| Error::AccessControlError)
    }

    /// Set the global paused state (admin only). When paused, create_payment, verify_payment, and cancel_payment are blocked.
    pub fn set_global_pause(env: Env, admin: Address, paused: bool, reason: String) -> Result<(), Error> {
        admin.require_auth();

        if !AccessControl::has_role(&env, &role_admin(&env), &admin) {
            return Err(Error::Unauthorized);
        }

        let state = PauseState {
            paused,
            reason: reason.clone(),
            admin: Some(admin.clone()),
            timestamp: env.ledger().timestamp(),
        };

        env.storage().persistent().set(&DataKey::Paused, &state);

        let event_name = if paused {
            Symbol::new(&env, "GLOBAL_PAUSED")
        } else {
            Symbol::new(&env, "GLOBAL_UNPAUSED")
        };

        env.events()
            .publish((Symbol::new(&env, "CONTRACT"), event_name), (admin, reason));

        Ok(())
    }

    /// Set the creation paused state (admin only). When paused, only create_payment is blocked.
    pub fn set_creation_pause(env: Env, admin: Address, paused: bool, reason: String) -> Result<(), Error> {
        admin.require_auth();

        if !AccessControl::has_role(&env, &role_admin(&env), &admin) {
            return Err(Error::Unauthorized);
        }

        let state = PauseState {
            paused,
            reason: reason.clone(),
            admin: Some(admin.clone()),
            timestamp: env.ledger().timestamp(),
        };

        env.storage().persistent().set(&DataKey::CreationPaused, &state);

        let event_name = if paused {
            Symbol::new(&env, "CREATION_PAUSED")
        } else {
            Symbol::new(&env, "CREATION_UNPAUSED")
        };

        env.events()
            .publish((Symbol::new(&env, "CONTRACT"), event_name), (admin, reason));

        Ok(())
    }

    /// Legacy wrapper for set_global_pause
    pub fn set_paused(env: Env, admin: Address, paused: bool) -> Result<(), Error> {
        let reason = if paused {
            String::from_str(&env, "Legacy pause")
        } else {
            String::from_str(&env, "Legacy unpause")
        };
        Self::set_global_pause(env, admin, paused, reason)
    }

    /// Get the current consolidated pause info.
    pub fn get_pause_info(env: Env) -> PauseInfo {
        let empty_reason = String::from_str(&env, "");
        let default_state = PauseState {
            paused: false,
            reason: empty_reason,
            admin: None,
            timestamp: 0,
        };

        let global = env.storage()
            .persistent()
            .get::<DataKey, PauseState>(&DataKey::Paused)
            .unwrap_or_else(|| default_state.clone());
            
        let creation = env.storage()
            .persistent()
            .get::<DataKey, PauseState>(&DataKey::CreationPaused)
            .unwrap_or(default_state);

        PauseInfo { global, creation }
    }

    /// Get the current global paused state.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .persistent()
            .get::<DataKey, PauseState>(&DataKey::Paused)
            .map(|s| s.paused)
            .unwrap_or(false)
    }

    /// Check if contract is globally paused and return error if so.
    fn require_not_paused(env: &Env) -> Result<(), Error> {
        if Self::is_paused(env.clone()) {
            return Err(Error::ContractPaused);
        }
        Ok(())
    }

    /// Check if payment creation is paused (either globally or specifically for creation).
    fn require_creation_not_paused(env: &Env) -> Result<(), Error> {
        Self::require_not_paused(env)?;
        
        let creation_paused: bool = env
            .storage()
            .persistent()
            .get::<DataKey, PauseState>(&DataKey::CreationPaused)
            .map(|s| s.paused)
            .unwrap_or(false);

        if creation_paused {
            return Err(Error::ContractPaused);
        }
        Ok(())
    }

    fn enforce_create_payment_rate_limit(env: &Env, merchant_id: &Address) -> Result<(), Error> {
        let now = env.ledger().timestamp();
        let key = DataKey::MerchantRateLimit(merchant_id.clone());

        let mut state: MerchantCreateRateLimit =
            env.storage()
                .persistent()
                .get(&key)
                .unwrap_or(MerchantCreateRateLimit {
                    last_payment_at: now,
                    count: 0,
                });

        if now.saturating_sub(state.last_payment_at) >= CREATE_PAYMENT_WINDOW_SECS {
            state.count = 0;
        }

        if state.count >= CREATE_PAYMENT_MAX_PER_WINDOW {
            return Err(Error::RateLimitExceeded);
        }

        state.count = state.count.saturating_add(1);
        state.last_payment_at = now;

        env.storage().persistent().set(&key, &state);
        Self::bump_ttl(env, &key, SHORT_LIVE_TTL);

        Ok(())
    }

    /// Set per-merchant min/max payment amount limits (merchant self-service).
    /// Pass None to clear a bound. Requires the caller to hold the MERCHANT role.
    pub fn set_merchant_amount_limits(
        env: Env,
        merchant_id: Address,
        min: Option<i128>,
        max: Option<i128>,
    ) -> Result<(), Error> {
        merchant_id.require_auth();
        if !AccessControl::has_role(&env, &role_merchant(&env), &merchant_id) {
            return Err(Error::Unauthorized);
        }
        if let (Some(lo), Some(hi)) = (min, max) {
            if lo > hi {
                return Err(Error::InvalidAmount);
            }
        }
        let limits = AmountLimits { min, max };
        env.storage()
            .persistent()
            .set(&DataKey::MerchantAmountLimits(merchant_id), &limits);
        Ok(())
    }

    /// Read per-merchant amount limits.
    pub fn get_merchant_amount_limits(env: Env, merchant_id: Address) -> Option<AmountLimits> {
        env.storage()
            .persistent()
            .get(&DataKey::MerchantAmountLimits(merchant_id))
    }

    /// Set global min/max payment amount limits (admin only).
    /// Pass None to clear a bound.
    pub fn set_global_amount_limits(
        env: Env,
        admin: Address,
        min: Option<i128>,
        max: Option<i128>,
    ) -> Result<(), Error> {
        admin.require_auth();
        if !AccessControl::has_role(&env, &role_admin(&env), &admin) {
            return Err(Error::Unauthorized);
        }
        if let (Some(lo), Some(hi)) = (min, max) {
            if lo > hi {
                return Err(Error::InvalidAmount);
            }
        }
        let limits = AmountLimits { min, max };
        env.storage()
            .persistent()
            .set(&DataKey::GlobalAmountLimits, &limits);
        Ok(())
    }

    /// Read global amount limits.
    pub fn get_global_amount_limits(env: Env) -> Option<AmountLimits> {
        env.storage()
            .persistent()
            .get(&DataKey::GlobalAmountLimits)
    }

    /// Enforce amount limits: merchant-specific limits take precedence over global limits.
    fn enforce_amount_limits(env: &Env, merchant_id: &Address, amount: i128) -> Result<(), Error> {
        let limits: Option<AmountLimits> = env
            .storage()
            .persistent()
            .get(&DataKey::MerchantAmountLimits(merchant_id.clone()))
            .or_else(|| {
                env.storage()
                    .persistent()
                    .get(&DataKey::GlobalAmountLimits)
            });

        if let Some(l) = limits {
            if let Some(min) = l.min {
                if amount < min {
                    return Err(Error::AmountBelowMin);
                }
            }
            if let Some(max) = l.max {
                if amount > max {
                    return Err(Error::AmountAboveMax);
                }
            }
        }
        Ok(())
    }

    /// Allow or disallow a token address for use in payments (admin only).
    pub fn allow_token(env: Env, admin: Address, token_address: Address) -> Result<(), Error> {
        admin.require_auth();
        if !AccessControl::has_role(&env, &role_admin(&env), &admin) {
            return Err(Error::Unauthorized);
        }
        env.storage()
            .persistent()
            .set(&DataKey::AllowedToken(token_address), &true);
        Ok(())
    }

    /// Returns true if the given token address is on the allowlist.
    pub fn is_token_allowed(env: Env, token_address: Address) -> bool {
        env.storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::AllowedToken(token_address))
            .unwrap_or(false)
    }

    #[allow(deprecated)]
    pub fn create_payment(
        env: Env,
        args: CreatePaymentArgs,
    ) -> Result<PaymentCharge, Error> {
        Self::require_creation_not_paused(&env)?;
        args.merchant_id.require_auth();

        // Idempotency check: if client_token was already used, return the existing payment
        // (or error if it maps to a different payment_id).
        if let Some(ref token) = args.client_token {
            let key = DataKey::IdempotencyKey(token.clone());
            if let Some(existing_id) = env
                .storage()
                .persistent()
                .get::<DataKey, String>(&key)
            {
                if existing_id == args.payment_id {
                    return Self::get_payment_internal(&env, &args.payment_id);
                } else {
                    return Err(Error::DuplicateIdempotencyKey);
                }
            }
        }

        // Verify that the merchant has the MERCHANT role (granted on verification)
        if !AccessControl::has_role(&env, &role_merchant(&env), &args.merchant_id) {
            return Err(Error::Unauthorized);
        }

        // Validate token: if provided it must be on the allowlist; if absent the default USDC token is used.
        if let Some(ref token_addr) = args.token_address {
            let allowed: bool = env
                .storage()
                .persistent()
                .get::<DataKey, bool>(&DataKey::AllowedToken(token_addr.clone()))
                .unwrap_or(false);
            if !allowed {
                return Err(Error::UnsupportedToken);
            }
        }

        // Issue #79: Cross-contract validate merchant is verified and active
        if let Some(registry_address) = env
            .storage()
            .persistent()
            .get::<DataKey, Address>(&DataKey::MerchantRegistryAddress)
        {
            let registry_client =
                crate::merchant_registry::MerchantRegistryClient::new(&env, &registry_address);
            match registry_client.try_get_merchant(&args.merchant_id) {
                Ok(Ok(merchant)) => {
                    // Require merchant to be verified (not Unverified), active, and not suspended
                    if merchant.kyc_tier == crate::merchant_registry::KycTier::Unverified
                        || !merchant.active
                        || merchant.suspension_reason.is_some()
                    {
                        return Err(Error::Unauthorized);
                    }
                }
                _ => {
                    // If registry lookup fails, reject the payment
                    return Err(Error::Unauthorized);
                }
            }
        }

        if args.amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        Self::enforce_amount_limits(&env, &args.merchant_id, args.amount)?;

        if env
            .storage()
            .persistent()
            .has(&DataKey::Payment(args.payment_id.clone()))
        {
            return Err(Error::PaymentAlreadyExists);
        }

        if args.payment_id.is_empty() {
            return Err(Error::InvalidPaymentId);
        }

        Self::enforce_create_payment_rate_limit(&env, &args.merchant_id)?;

        let now = env.ledger().timestamp();
        let resolved_expires_at = match args.expires_at {
            Some(ts) => ts,
            None => now.saturating_add(
                args.duration_secs.unwrap_or(DEFAULT_PAYMENT_DURATION_SECS),
            ),
        };
        if resolved_expires_at <= now {
            return Err(Error::InvalidExpiry);
        }

        let payment = PaymentCharge {
            payment_id: args.payment_id.clone(),
            merchant_id: args.merchant_id.clone(),
            amount: args.amount,
            currency: args.currency,
            deposit_address: args.deposit_address,
            status: PaymentStatus::Pending,
            payer_address: None,
            transaction_hash: None,
            created_at: now,
            confirmed_at: None,
            expires_at: resolved_expires_at,
            amount_received: None,
            memo: args.memo.clone(),
            memo_type: args.memo_type.clone(),
            token_address: args.token_address.clone(),
        };

        env.storage()
            .persistent()
            .set(&DataKey::Payment(args.payment_id.clone()), &payment);
        Self::bump_payment_ttl(&env, &args.payment_id, &payment.status);

        let mut merchant_payments = Self::get_merchant_payments_internal(&env, &args.merchant_id);
        merchant_payments.push_back(args.payment_id.clone());
        let merchant_payments_key = DataKey::MerchantPayments(args.merchant_id.clone());
        env.storage()
            .persistent()
            .set(&merchant_payments_key, &merchant_payments);
        Self::bump_ttl(&env, &merchant_payments_key, LONG_LIVE_TTL);

        env.events().publish(
            (
                Symbol::new(&env, "PAYMENT"),
                Symbol::new(&env, "CREATED"),
                args.payment_id.clone(),
            ),
            (args.merchant_id, args.amount),
        );

        // Persist idempotency key → payment_id mapping so retries are safe.
        if let Some(token) = args.client_token {
            let key = DataKey::IdempotencyKey(token);
            env.storage().persistent().set(&key, &args.payment_id);
            Self::bump_ttl(&env, &key, LONG_LIVE_TTL);
        }

        Ok(payment)
    }

    #[allow(deprecated)]
    pub fn verify_payment(
        env: Env,
        oracle: Address,
        payment_id: String,
        transaction_hash: BytesN<32>,
        payer_address: Address,
        amount_received: i128,
    ) -> Result<PaymentStatus, Error> {
        Self::require_not_paused(&env)?;
        oracle.require_auth();

        if !AccessControl::has_role(&env, &role_oracle(&env), &oracle) {
            return Err(Error::Unauthorized);
        }

        let mut payment = Self::get_payment_internal(&env, &payment_id)?;

        // Issue #75: Enforce idempotent verify_payment - reject double verification
        // If payment is already Confirmed, return current status without error
        if payment.status == PaymentStatus::Confirmed {
            return Ok(payment.status);
        }

        // Reject if payment is in any other terminal state
        if payment.status != PaymentStatus::Pending {
            return Err(Error::PaymentAlreadyProcessed);
        }

        if env.ledger().timestamp() > payment.expires_at {
            return Err(Error::PaymentExpired);
        }

        // Record the actual amount received for reconciliation
        payment.amount_received = Some(amount_received);
        payment.payer_address = Some(payer_address);
        payment.transaction_hash = Some(transaction_hash);
        payment.confirmed_at = Some(env.ledger().timestamp());

        // Scale tolerance by token decimals: 1 unit in the smallest denomination per decimal place.
        // USDC has 7 decimals on Stellar (stroops); other tokens may differ.
        // tolerance = 10^(decimals - 6) clamped to at least 1, so a 6-decimal token gets tolerance=1,
        // a 7-decimal token gets tolerance=10, a 2-decimal token gets tolerance=1 (clamped).
        let tolerance = if let Some(ref token_addr) = payment.token_address {
            let decimals = token::TokenClient::new(&env, token_addr).decimals();
            if decimals >= 6 {
                let exp = (decimals - 6) as u32;
                let mut t: i128 = 1;
                let mut i = 0u32;
                while i < exp {
                    t *= 10;
                    i += 1;
                }
                t
            } else {
                1i128
            }
        } else {
            PAYMENT_TOLERANCE
        };

        let diff = amount_received - payment.amount;

        let new_status = if (0..=tolerance).contains(&diff) {
            // Exact match or tiny overpay within tolerance → Confirmed
            PaymentStatus::Confirmed
        } else if diff > tolerance {
            // Meaningfully more than expected → Overpaid
            PaymentStatus::Overpaid
        } else if diff >= -tolerance {
            // Tiny underpay within tolerance → Confirmed
            PaymentStatus::Confirmed
        } else {
            // Meaningfully less than expected → PartiallyPaid
            PaymentStatus::PartiallyPaid
        };

        payment.status = new_status.clone();

        env.storage()
            .persistent()
            .set(&DataKey::Payment(payment_id.clone()), &payment);
        Self::bump_payment_ttl(&env, &payment_id, &payment.status);

        let event_name = match &new_status {
            PaymentStatus::Confirmed => Symbol::new(&env, "VERIFIED"),
            PaymentStatus::Overpaid => Symbol::new(&env, "OVERPAID"),
            PaymentStatus::PartiallyPaid => Symbol::new(&env, "PARTIALLY_PAID"),
            _ => Symbol::new(&env, "FAILED"),
        };

        env.events().publish(
            (Symbol::new(&env, "PAYMENT"), event_name, payment_id.clone()),
            (payment.merchant_id, payment.amount, amount_received),
        );

        Ok(new_status)
    }

    pub fn get_payment(env: Env, payment_id: String) -> Result<PaymentCharge, Error> {
        Self::get_payment_internal(&env, &payment_id)
    }

    pub fn get_merchant_payments(env: Env, merchant_id: Address) -> Vec<String> {
        Self::get_merchant_payments_internal(&env, &merchant_id)
    }

    pub fn get_merchant_payments_paginated(
        env: Env,
        merchant_id: Address,
        offset: u32,
        limit: u32,
    ) -> Vec<String> {
        let all = Self::get_merchant_payments_internal(&env, &merchant_id);
        if limit == 0 {
            return vec![&env];
        }

        let mut page = vec![&env];
        let start = offset;
        let end = core::cmp::min(all.len(), start.saturating_add(limit));

        let mut i = start;
        while i < end {
            if let Some(id) = all.get(i) {
                page.push_back(id);
            }
            i += 1;
        }

        page
    }

    #[allow(deprecated)]
    pub fn cancel_payment(env: Env, authority: Address, payment_id: String) -> Result<(), Error> {
        Self::require_not_paused(&env)?;

        let mut payment = Self::get_payment_internal(&env, &payment_id)?;

        if payment.status != PaymentStatus::Pending {
            return Err(Error::PaymentAlreadyProcessed);
        }

        // Ensure the current time is less than the expiry time; if not, mark as expired and return.
        if env.ledger().timestamp() >= payment.expires_at {
            payment.status = PaymentStatus::Expired;

            env.storage()
                .persistent()
                .set(&DataKey::Payment(payment_id.clone()), &payment);
            Self::bump_payment_ttl(&env, &payment_id, &payment.status);

            env.events().publish(
                (
                    Symbol::new(&env, "PAYMENT"),
                    Symbol::new(&env, "EXPIRED"),
                    payment_id.clone(),
                ),
                (payment.merchant_id, payment.amount),
            );

            return Ok(());
        }

        authority.require_auth();
        let is_merchant = authority == payment.merchant_id;
        let is_oracle = AccessControl::has_role(&env, &role_oracle(&env), &authority);
        if !is_merchant && !is_oracle {
            return Err(Error::Unauthorized);
        }

        payment.status = PaymentStatus::Failed;

        env.storage()
            .persistent()
            .set(&DataKey::Payment(payment_id.clone()), &payment);
        Self::bump_payment_ttl(&env, &payment_id, &payment.status);

        env.events().publish(
            (
                Symbol::new(&env, "PAYMENT"),
                Symbol::new(&env, "CANCELLED"),
                payment_id.clone(),
            ),
            (payment.merchant_id, payment.amount),
        );

        Ok(())
    }

    #[allow(deprecated)]
    pub fn expire_payment(env: Env, payment_id: String) -> Result<(), Error> {
        let mut payment = Self::get_payment_internal(&env, &payment_id)?;

        if payment.status != PaymentStatus::Pending {
            return Err(Error::PaymentAlreadyProcessed);
        }

        if env.ledger().timestamp() <= payment.expires_at {
            return Err(Error::Unauthorized);
        }

        payment.status = PaymentStatus::Expired;

        env.storage()
            .persistent()
            .set(&DataKey::Payment(payment_id.clone()), &payment);
        Self::bump_payment_ttl(&env, &payment_id, &payment.status);

        env.events().publish(
            (
                Symbol::new(&env, "PAYMENT"),
                Symbol::new(&env, "EXPIRED"),
                payment_id.clone(),
            ),
            (payment.merchant_id, payment.amount),
        );

        Ok(())
    }

    pub fn settle_payment(
        env: Env,
        operator: Address,
        payment_id: String,
        splits: Vec<SettlementSplit>,
    ) -> Result<(), Error> {
        operator.require_auth();

        if !AccessControl::has_role(&env, &role_settlement_operator(&env), &operator) {
            return Err(Error::Unauthorized);
        }

        let mut payment = Self::get_payment_internal(&env, &payment_id)?;

        if payment.status != PaymentStatus::Confirmed {
            return Err(Error::PaymentAlreadyProcessed);
        }

        if splits.is_empty() {
            return Err(Error::InvalidSettlement);
        }

        // Verify split amounts are positive and total matches payment amount
        let mut total: i128 = 0;
        for split in splits.iter() {
            if split.amount <= 0 {
                return Err(Error::InvalidSettlement);
            }
            total = total.saturating_add(split.amount);
        }
        if total != payment.amount {
            return Err(Error::InvalidSettlement);
        }

        payment.status = PaymentStatus::Settled;

        env.storage()
            .persistent()
            .set(&DataKey::Payment(payment_id.clone()), &payment);
        Self::bump_payment_ttl(&env, &payment_id, &payment.status);

        env.events().publish(
            (
                Symbol::new(&env, "PAYMENT"),
                Symbol::new(&env, "SETTLED"),
                payment_id.clone(),
            ),
            (payment.merchant_id, payment.amount),
        );

        Ok(())
    }

    /// Atomic swap and pay: swap sender's token to merchant's required token and create payment.
    /// Integrates with DEX (e.g., Soroswap) for atomic asset conversion.
    /// 
    /// # Arguments
    /// * `payer` - The address making the payment
    /// * `payment_id` - Unique payment identifier
    /// * `merchant_id` - Merchant's address
    /// * `amount` - Amount in the merchant's settlement currency (after swap)
    /// * `currency` - Settlement currency symbol
    /// * `deposit_address` - Where the payment should be deposited
    /// * `token_in` - Address of the token the payer is sending
    /// * `amount_in` - Amount of token_in to swap
    /// * `amount_out_min` - Minimum amount of settlement token required
    /// * `path` - DEX swap path [token_in, ..., settlement_token]
    /// * `expires_at` - Payment expiry timestamp
    /// * `dex_router` - Address of the DEX router contract
    /// 
    /// # Returns
    /// The created PaymentCharge on success
    #[allow(clippy::too_many_arguments)]
    pub fn swap_and_pay(
        env: Env,
        args: SwapAndPayArgs,
    ) -> Result<PaymentCharge, Error> {
        args.payer.require_auth();

        // Validate inputs
        if args.amount <= 0 || args.amount_in <= 0 {
            return Err(Error::InvalidAmount);
        }

        // Execute atomic swap via DEX router
        let deadline = env.ledger().timestamp().saturating_add(3_600); // 1 hour deadline
        
        let dex_client = DexRouterClient::new(&env, &args.dex_router);
        
        // Perform the swap - this transfers tokens from payer and sends output to deposit_address
        let _swap_result = dex_client.swap_exact_tokens_for_tokens(
            &args.amount_in,
            &args.amount_out_min,
            &args.path,
            &args.deposit_address,
            &deadline,
        );

        // Now create the payment with the swapped amount
        let create_args = CreatePaymentArgs {
            payment_id: args.payment_id.clone(),
            merchant_id: args.merchant_id,
            amount: args.amount,
            currency: args.currency,
            deposit_address: args.deposit_address.clone(),
            expires_at: args.expires_at,
            duration_secs: None,
            memo: None,
            memo_type: None,
            token_address: Some(args.deposit_address),
            client_token: None,
        };

        let payment = Self::create_payment(env.clone(), create_args)?;

        // Emit SWAP/AND/PAY event
        env.events().publish(
            (
                Symbol::new(&env, "SWAP"),
                Symbol::new(&env, "AND"),
                Symbol::new(&env, "PAY"),
            ),
            (args.payment_id, args.payer, args.amount_in, args.token_in, args.amount),
        );
        Ok(payment)
    }

    fn get_next_stream_id(env: &Env) -> u64 {
        let mut counter: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::StreamCounter)
            .unwrap_or(0);
        counter += 1;
        env.storage()
            .persistent()
            .set(&DataKey::StreamCounter, &counter);
        counter
    }

    fn get_payment_internal(env: &Env, payment_id: &String) -> Result<PaymentCharge, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Payment(payment_id.clone()))
            .ok_or(Error::PaymentNotFound)
    }

    fn get_merchant_payments_internal(env: &Env, merchant_id: &Address) -> Vec<String> {
        env.storage()
            .persistent()
            .get(&DataKey::MerchantPayments(merchant_id.clone()))
            .unwrap_or_else(|| vec![env])
    }

    fn payment_ttl(status: &PaymentStatus) -> u32 {
        match status {
            PaymentStatus::Pending => SHORT_LIVE_TTL,
            PaymentStatus::Confirmed
            | PaymentStatus::Settled
            | PaymentStatus::Expired
            | PaymentStatus::Failed
            | PaymentStatus::PartiallyPaid
            | PaymentStatus::Overpaid => LONG_LIVE_TTL,
        }
    }

    fn bump_payment_ttl(env: &Env, payment_id: &String, status: &PaymentStatus) {
        let key = DataKey::Payment(payment_id.clone());
        Self::bump_ttl(env, &key, Self::payment_ttl(status));
    }

    fn bump_ttl(env: &Env, key: &DataKey, ttl: u32) {
        let threshold = core::cmp::max(1, ttl / TTL_BUMP_THRESHOLD_DIVISOR);
        env.storage().persistent().extend_ttl(key, threshold, ttl);
    }

    pub fn cancel_stream(env: Env, sender: Address, stream_id: String) -> Result<(), StreamError> { PaymentStreaming::cancel_stream(env, sender, stream_id) }

    pub fn cancel_multiple_streams(env: Env, sender: Address, stream_ids: Vec<String>) -> Result<Vec<String>, StreamError> { PaymentStreaming::cancel_multiple_streams(env, sender, stream_ids) }

    pub fn batch_withdraw_to(env: Env, recipient: Address, withdrawals: Vec<WithdrawalRecipient>) -> Result<Vec<String>, StreamError> { PaymentStreaming::batch_withdraw_to(env, recipient, withdrawals) }

    pub fn get_stream(env: Env, stream_id: String) -> Result<PaymentStream, StreamError> { PaymentStreaming::get_stream(env, stream_id) }

    /// Create a new payment stream. Tokens are pulled from `sender` into the contract.
    pub fn create_stream(env: Env, sender: Address, receiver: Address, token: Address, rate_per_second: i128, deposit: i128, stream_id: String) -> Result<PaymentStream, StreamError> { PaymentStreaming::create_stream(env, sender, receiver, token, rate_per_second, deposit, stream_id) }

    pub fn top_up_multiple_streams(env: Env, sender: Address, top_ups: Vec<(String, i128)>) -> Result<(), StreamError> { PaymentStreaming::top_up_multiple_streams(env, sender, top_ups) }

}

#[cfg(test)]
mod auth_test;
#[cfg(test)]
mod dispute_test;
#[cfg(test)]
mod fx_oracle_test;
#[cfg(test)]
mod integration_test;
pub mod merchant_registry;
#[cfg(test)]
mod merchant_registry_test;
mod payment_link;
#[cfg(test)]
mod proptests;
pub use payment_link::{PaymentLink, PaymentLinkManager, PaymentLinkManagerClient};
#[cfg(test)]
mod memo_test;
#[cfg(test)]
mod pause_test;
#[cfg(test)]
mod payment_link_test;
mod test;

// Payment streaming module (Issue #127)
pub mod stream;
pub use stream::{PaymentStream, PaymentStreaming, PaymentStreamingClient, StreamError, StreamStatus};
#[cfg(test)]
mod stream_test;

pub mod utils;
pub use utils::format_id;
