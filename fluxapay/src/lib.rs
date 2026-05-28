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

// Issue #167: Tiered refund fees based on merchant KYC tier
const REFUND_FEE_BPS_BASIC: i128 = 100;     // 1.0% for Basic tier
const REFUND_FEE_BPS_FULL: i128 = 80;       // 0.8% for Full tier
const REFUND_FEE_BPS_BUSINESS: i128 = 50;   // 0.5% for Business tier

/// Maximum number of payment retries before a subscription is cancelled.
pub const SUBSCRIPTION_MAX_RETRIES: u32 = 3;
/// Spacing between retry attempts in seconds (2 days).
pub const SUBSCRIPTION_RETRY_INTERVAL_SECS: u64 = 2 * 24 * 60 * 60;
pub(crate) const ZERO_CONTRACT_STRKEY: &str =
    "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";

mod access_control;
mod dex_router;
pub mod fx_oracle;
pub mod merchant_auth;
use access_control::{
    role_admin, role_merchant, role_oracle, role_settlement_operator, AccessControl,
};
// Re-export for tests
#[allow(unused_imports)]
pub use access_control::AccessControlDataKey;
pub use dex_router::{DexRouter, DexRouterClient};
pub use fx_oracle::{FXOracle, FXOracleClient, FXOracleError};
pub use merchant_auth::{
    MerchantAuthError, MerchantAuthorization, MerchantPreAuth,
};

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
    /// Optional 32-byte hash merchants can use to tie a payment to an order ID or customer ID.
    pub metadata_hash: Option<BytesN<32>>,
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
    InvalidAddress = 26,
    /// Swap path contains a circular route indicative of arbitrage exploitation.
    ArbitrageDetected = 27,
    /// DEX path or quoted returns failed validation.
    SwapPathInvalid = 28,
    /// DEX quoted swap output deviates from the oracle reference price.
    OraclePriceDeviation = 29,
    /// Subscription is in a grace period; payment will be retried.
    SubscriptionInGracePeriod = 30,
    /// Subscription has exhausted all retries and is now cancelled.
    SubscriptionRetryExhausted = 31,
    /// The provided resume timestamp is in the past or invalid.
    InvalidResumeTimestamp = 32,
    /// Merchant authorization error (see MerchantAuthError for details).
    MerchantAuthError = 33,
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
    pub metadata_hash: Option<BytesN<32>>,
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
    /// Optional FX oracle used to sanitize DEX swap quotes.
    pub fx_oracle: Option<Address>,
    /// Oracle rate pair symbol (required when `fx_oracle` is set).
    pub oracle_pair: Option<Symbol>,
    /// Maximum allowed deviation from oracle price in basis points (100 = 1%).
    pub max_deviation_bps: u32,
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

/// Vote choice for stake-weighted dispute voting.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoteChoice {
    /// Vote in favour of the disputer (refund should be issued).
    Favour,
    /// Vote against the disputer (dispute should be rejected).
    Against,
}

/// Accumulated vote tally for a dispute.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoteTally {
    /// Total stake weight voting in favour.
    pub favour_weight: i128,
    /// Total stake weight voting against.
    pub against_weight: i128,
    /// Number of arbitrators who have voted.
    pub vote_count: u32,
}

/// Operator note persisted on-chain for dispute transparency.
///
/// Stored under `DataKey::DisputeOperatorNote(dispute_id)` and emitted
/// in full via the `DISPUTE / OPERATOR_NOTE` event so that off-chain
/// indexers can reconstruct the complete audit trail.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisputeOperatorNote {
    /// The dispute this note belongs to.
    pub dispute_id: String,
    /// Operator address that authored the note.
    pub operator: Address,
    /// Full resolution notes text.
    pub resolution_notes: String,
    /// Operator-provided signature (e.g. base64-encoded Ed25519 sig over the note hash).
    pub operator_signature: String,
    /// Ledger timestamp when the note was recorded.
    pub recorded_at: u64,
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
    /// Number of consecutive failed payment attempts in the current grace period.
    pub retry_count: u32,
    /// Timestamp of the next retry attempt (set when a payment fails and grace period begins).
    pub next_retry_at: Option<u64>,
    /// When set, the subscription will automatically resume at this timestamp.
    /// Only meaningful when `status == Paused`.
    pub resume_at: Option<u64>,
    /// Optional affiliate address to receive a percentage of each payment.
    pub affiliate: Option<Address>,
    /// Affiliate fee in basis points (bps). If set and `affiliate` is Some,
    /// `affiliate_fee_bps / 10000` of each payment will be routed to the affiliate.
    pub affiliate_fee_bps: Option<u32>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BillingInterval {
    Daily,
    Weekly,
    Monthly,
    Annually,
}

impl BillingInterval {
    /// Returns the approximate duration in seconds for each interval.
    pub fn to_secs(&self) -> u64 {
        match self {
            BillingInterval::Daily => 86_400,
            BillingInterval::Weekly => 604_800,
            BillingInterval::Monthly => 2_592_000,   // 30 days
            BillingInterval::Annually => 31_536_000, // 365 days
        }
    }
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
    pub billing_interval: BillingInterval,
    pub active: bool,
    /// Optional split payout configuration for bundle subscriptions.
    /// If non-empty, the plan amount will be distributed to the configured
    /// `SettlementSplit` recipients on each subscription charge.
    pub payout_splits: Vec<SettlementSplit>,
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
    /// Stores operator notes keyed by dispute_id for on-chain transparency.
    DisputeOperatorNote(String),
    /// Locked stake for a dispute arbitrator: (dispute_id, arbitrator) → amount
    DisputeStake(String, Address),
    /// Vote cast by an arbitrator: (dispute_id, arbitrator) → VoteChoice
    DisputeVote(String, Address),
    /// Tally of votes for a dispute
    DisputeVoteTally(String),
}

// When building for WASM deployment, only the active contract's #[contractimpl]
// is compiled to avoid duplicate exported symbols. On non-WASM targets (tests,
// tooling), all impls compile so that *Client types are available everywhere.
#[cfg_attr(
    any(not(target_arch = "wasm32"), feature = "contract-refund-manager"),
    contractimpl
)]
#[allow(deprecated)] // events::publish — migrate to #[contractevent] in a follow-up
impl RefundManager {
    pub fn version() -> u32 {
        1
    }

    fn validate_init_address(env: &Env, address: Address) -> Result<(), Error> {
        let zero_address = Address::from_str(env, ZERO_CONTRACT_STRKEY);
        if address == zero_address {
            return Err(Error::InvalidAddress);
        }
        Ok(())
    }

    fn validate_admin_and_token(
        env: &Env,
        admin: Address,
        token_address: Address,
    ) -> Result<(), Error> {
        if admin == token_address {
            return Err(Error::InvalidAddress);
        }
        Self::validate_init_address(env, admin)?;
        Self::validate_init_address(env, token_address)
    }

    pub fn initialize_refund_manager(
        env: Env,
        admin: Address,
        usdc_token_address: Address,
    ) -> Result<(), Error> {
        Self::validate_admin_and_token(&env, admin.clone(), usdc_token_address.clone())?;
        AccessControl::initialize(&env, admin);
        env.storage()
            .persistent()
            .set(&DataKey::UsdcToken, &usdc_token_address);
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
                metadata_hash: None,
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

        // Issue #167: Query merchant's KYC tier and apply tiered refund fee
        let payment: PaymentCharge = env
            .storage()
            .persistent()
            .get::<DataKey, PaymentCharge>(&DataKey::Payment(refund.payment_id.clone()))
            .ok_or(Error::PaymentNotFound)?;

        let fee_bps = if let Some(registry_address) = env
            .storage()
            .persistent()
            .get::<DataKey, Address>(&DataKey::MerchantRegistryAddress)
        {
            let registry_client =
                crate::merchant_registry::MerchantRegistryClient::new(env, &registry_address);
            match registry_client.try_get_merchant(&payment.merchant_id) {
                Ok(Ok(merchant)) => {
                    use crate::merchant_registry::KycTier;
                    match merchant.kyc_tier {
                        KycTier::Business => REFUND_FEE_BPS_BUSINESS,
                        KycTier::Full => REFUND_FEE_BPS_FULL,
                        KycTier::Basic => REFUND_FEE_BPS_BASIC,
                        KycTier::Unverified => REFUND_FEE_BPS, // Default 1%
                    }
                }
                _ => REFUND_FEE_BPS, // Default if registry lookup fails
            }
        } else {
            REFUND_FEE_BPS // Default if no registry configured
        };

        let fee = refund.amount * fee_bps / 10_000;
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
    /// Instantly refund a payment without operator approval.
    ///
    /// Only merchants with KYC tier `Full` or `Business` may call this.
    /// The merchant must be the `merchant_id` on the original payment.
    /// Executes the USDC transfer immediately (no `Pending` state).
    pub fn refund_instantly(
        env: Env,
        merchant_id: Address,
        payment_id: String,
        refund_amount: i128,
        reason: String,
        registry_address: Address,
    ) -> Result<String, Error> {
        merchant_id.require_auth();

        // Verify merchant KYC tier is Full or Business via cross-contract call
        let registry_client =
            crate::merchant_registry::MerchantRegistryClient::new(&env, &registry_address);
        let merchant = registry_client
            .try_get_merchant(&merchant_id)
            .map_err(|_| Error::Unauthorized)?
            .map_err(|_| Error::Unauthorized)?;

        let is_high_trust = merchant.kyc_tier == crate::merchant_registry::KycTier::Full
            || merchant.kyc_tier == crate::merchant_registry::KycTier::Business;
        if !is_high_trust {
            return Err(Error::Unauthorized);
        }

        // Validate payment belongs to this merchant and is Confirmed
        let payment: PaymentCharge = env
            .storage()
            .persistent()
            .get(&DataKey::Payment(payment_id.clone()))
            .ok_or(Error::PaymentNotFound)?;

        if payment.merchant_id != merchant_id {
            return Err(Error::Unauthorized);
        }
        if payment.status != PaymentStatus::Confirmed {
            return Err(Error::PaymentAlreadyProcessed);
        }

        // Create the refund record (validates amount, checks totals)
        let refund_id = Self::create_refund_internal(
            &env,
            payment_id,
            refund_amount,
            reason,
            payment.payer_address.clone().ok_or(Error::Unauthorized)?,
        )?;

        // Execute transfer immediately — no operator approval needed
        let usdc_token_address: Address = env
            .storage()
            .persistent()
            .get(&DataKey::UsdcToken)
            .ok_or(Error::Unauthorized)?;
        let token_client = token::TokenClient::new(&env, &usdc_token_address);

        let fee = refund_amount * REFUND_FEE_BPS / 10_000;
        let net_amount = refund_amount - fee;

        let mut refund = Self::get_refund_internal(&env, &refund_id)?;
        refund.status = RefundStatus::Completed;
        refund.processed_at = Some(env.ledger().timestamp());

        // Effects before interaction (CEI)
        env.storage()
            .persistent()
            .set(&DataKey::Refund(refund_id.clone()), &refund);
        Self::bump_refund_ttl(&env, &refund_id, &refund.status);

        let from = env.current_contract_address();
        let to: MuxedAddress = (&refund.requester).into();
        let _ = token_client.try_transfer(&from, &to, &net_amount);

        if fee > 0 {
            if let Some(admin) = AccessControl::get_admin(&env) {
                let admin_muxed: MuxedAddress = (&admin).into();
                let _ = token_client.try_transfer(&from, &admin_muxed, &fee);
            }
        }

        env.events().publish(
            (Symbol::new(&env, "REFUND"), Symbol::new(&env, "COMPLETED")),
            (refund.payment_id, refund_id.clone(), refund_amount),
        );

        Ok(refund_id)
    }

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
            (Symbol::new(&env, "DISPUTE"), Symbol::new(&env, "REVIEWED")),
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
        operator_signature: String,
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

        let now = env.ledger().timestamp();

        // Persist operator note on-chain for full transparency.
        let note = DisputeOperatorNote {
            dispute_id: dispute_id.clone(),
            operator: operator.clone(),
            resolution_notes: resolution_notes.clone(),
            operator_signature: operator_signature.clone(),
            recorded_at: now,
        };
        env.storage()
            .persistent()
            .set(&DataKey::DisputeOperatorNote(dispute_id.clone()), &note);
        Self::bump_ttl(
            &env,
            &DataKey::DisputeOperatorNote(dispute_id.clone()),
            LONG_LIVE_TTL,
        );

        // Emit full note + signature so off-chain indexers have the complete record.
        env.events().publish(
            (
                Symbol::new(&env, "DISPUTE"),
                Symbol::new(&env, "OPERATOR_NOTE"),
            ),
            (
                dispute_id.clone(),
                operator.clone(),
                resolution_notes.clone(),
                operator_signature,
            ),
        );

        // Update dispute status
        dispute.status = DisputeStatus::Resolved;
        dispute.refund_id = Some(refund_id.clone());
        dispute.resolved_at = Some(now);
        dispute.resolution_notes = Some(resolution_notes);

        env.storage()
            .persistent()
            .set(&DataKey::Dispute(dispute_id.clone()), &dispute);
        Self::bump_dispute_ttl(&env, &dispute_id, &dispute.status);

        // Emit DISPUTE_RESOLVED event
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
        operator_signature: String,
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

        let now = env.ledger().timestamp();

        // Persist operator note on-chain for full transparency.
        let note = DisputeOperatorNote {
            dispute_id: dispute_id.clone(),
            operator: operator.clone(),
            resolution_notes: resolution_notes.clone(),
            operator_signature: operator_signature.clone(),
            recorded_at: now,
        };
        env.storage()
            .persistent()
            .set(&DataKey::DisputeOperatorNote(dispute_id.clone()), &note);
        Self::bump_ttl(
            &env,
            &DataKey::DisputeOperatorNote(dispute_id.clone()),
            LONG_LIVE_TTL,
        );

        // Emit full note + signature so off-chain indexers have the complete record.
        env.events().publish(
            (
                Symbol::new(&env, "DISPUTE"),
                Symbol::new(&env, "OPERATOR_NOTE"),
            ),
            (
                dispute_id.clone(),
                operator.clone(),
                resolution_notes.clone(),
                operator_signature,
            ),
        );

        dispute.status = DisputeStatus::Rejected;
        dispute.resolved_at = Some(now);
        dispute.resolution_notes = Some(resolution_notes);

        env.storage()
            .persistent()
            .set(&DataKey::Dispute(dispute_id.clone()), &dispute);
        Self::bump_dispute_ttl(&env, &dispute_id, &dispute.status);

        // Emit DISPUTE_REJECTED event
        env.events().publish(
            (Symbol::new(&env, "DISPUTE"), Symbol::new(&env, "REJECTED")),
            (dispute_id, dispute.payment_id),
        );

        Ok(())
    }

    /// Retrieve the persisted operator note for a dispute.
    pub fn get_dispute_operator_note(
        env: Env,
        dispute_id: String,
    ) -> Result<DisputeOperatorNote, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::DisputeOperatorNote(dispute_id))
            .ok_or(Error::DisputeNotFound)
    }

    // ─── Stake-weighted dispute voting (issue #33) ────────────────────────────

    /// Lock a governance-token stake to participate in dispute voting.
    ///
    /// The arbitrator transfers `amount` tokens into the contract as a stake.
    /// The stake is slashed if the arbitrator votes against the majority.
    ///
    /// # Parameters
    /// * `arbitrator`  – Address locking the stake; must sign.
    /// * `dispute_id`  – Dispute to vote on.
    /// * `token`       – Governance token contract address.
    /// * `amount`      – Amount to lock (must be > 0).
    pub fn lock_stake(
        env: Env,
        arbitrator: Address,
        dispute_id: String,
        token: Address,
        amount: i128,
    ) -> Result<(), Error> {
        arbitrator.require_auth();

        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        // Dispute must exist and be open / under review
        let dispute = Self::get_dispute_internal(&env, &dispute_id)?;
        if dispute.status == DisputeStatus::Resolved || dispute.status == DisputeStatus::Rejected {
            return Err(Error::DisputeAlreadyResolved);
        }

        // Prevent double-staking
        let stake_key = DataKey::DisputeStake(dispute_id.clone(), arbitrator.clone());
        if env.storage().persistent().has(&stake_key) {
            return Err(Error::Unauthorized);
        }

        // Effects: record stake before token transfer
        env.storage().persistent().set(&stake_key, &amount);
        Self::bump_ttl(&env, &stake_key, LONG_LIVE_TTL);

        // Interaction: pull stake from arbitrator
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&arbitrator, &env.current_contract_address(), &amount);

        env.events().publish(
            (Symbol::new(&env, "DISPUTE"), Symbol::new(&env, "STAKE_LOCKED")),
            (dispute_id, arbitrator, amount),
        );

        Ok(())
    }

    /// Cast a stake-weighted vote on a dispute.
    ///
    /// The arbitrator must have locked a stake first via `lock_stake`.
    /// Each arbitrator may only vote once per dispute.
    ///
    /// # Parameters
    /// * `arbitrator` – Voting arbitrator; must sign.
    /// * `dispute_id` – Dispute to vote on.
    /// * `choice`     – `VoteChoice::Favour` or `VoteChoice::Against`.
    pub fn cast_vote(
        env: Env,
        arbitrator: Address,
        dispute_id: String,
        choice: VoteChoice,
    ) -> Result<(), Error> {
        arbitrator.require_auth();

        // Dispute must be open / under review
        let dispute = Self::get_dispute_internal(&env, &dispute_id)?;
        if dispute.status == DisputeStatus::Resolved || dispute.status == DisputeStatus::Rejected {
            return Err(Error::DisputeAlreadyResolved);
        }

        // Arbitrator must have a locked stake
        let stake_key = DataKey::DisputeStake(dispute_id.clone(), arbitrator.clone());
        let stake: i128 = env
            .storage()
            .persistent()
            .get(&stake_key)
            .ok_or(Error::Unauthorized)?;

        // Prevent double-voting
        let vote_key = DataKey::DisputeVote(dispute_id.clone(), arbitrator.clone());
        if env.storage().persistent().has(&vote_key) {
            return Err(Error::Unauthorized);
        }

        // Record vote
        env.storage().persistent().set(&vote_key, &choice);
        Self::bump_ttl(&env, &vote_key, LONG_LIVE_TTL);

        // Update tally
        let tally_key = DataKey::DisputeVoteTally(dispute_id.clone());
        let mut tally: VoteTally = env
            .storage()
            .persistent()
            .get(&tally_key)
            .unwrap_or(VoteTally {
                favour_weight: 0,
                against_weight: 0,
                vote_count: 0,
            });

        match choice {
            VoteChoice::Favour => tally.favour_weight = tally.favour_weight.saturating_add(stake),
            VoteChoice::Against => {
                tally.against_weight = tally.against_weight.saturating_add(stake)
            }
        }
        tally.vote_count = tally.vote_count.saturating_add(1);

        env.storage().persistent().set(&tally_key, &tally);
        Self::bump_ttl(&env, &tally_key, LONG_LIVE_TTL);

        env.events().publish(
            (Symbol::new(&env, "DISPUTE"), Symbol::new(&env, "VOTE_CAST")),
            (dispute_id, arbitrator, stake),
        );

        Ok(())
    }

    /// Finalize a dispute based on stake-weighted votes.
    ///
    /// The majority side wins. Arbitrators who voted against the majority
    /// lose 10% of their stake (slashed to the contract admin). Winners
    /// receive their stake back.
    ///
    /// # Parameters
    /// * `operator`    – Settlement operator or oracle; must sign.
    /// * `dispute_id`  – Dispute to finalize.
    /// * `token`       – Governance token used for stakes.
    /// * `arbitrators` – List of all arbitrators who participated.
    pub fn finalize_dispute_vote(
        env: Env,
        operator: Address,
        dispute_id: String,
        token: Address,
        arbitrators: Vec<Address>,
    ) -> Result<(), Error> {
        operator.require_auth();

        let has_settlement =
            AccessControl::has_role(&env, &role_settlement_operator(&env), &operator);
        let has_oracle = AccessControl::has_role(&env, &role_oracle(&env), &operator);
        if !has_settlement && !has_oracle {
            return Err(Error::Unauthorized);
        }

        let dispute = Self::get_dispute_internal(&env, &dispute_id)?;
        if dispute.status == DisputeStatus::Resolved || dispute.status == DisputeStatus::Rejected {
            return Err(Error::DisputeAlreadyResolved);
        }

        let tally_key = DataKey::DisputeVoteTally(dispute_id.clone());
        let tally: VoteTally = env
            .storage()
            .persistent()
            .get(&tally_key)
            .unwrap_or(VoteTally {
                favour_weight: 0,
                against_weight: 0,
                vote_count: 0,
            });

        // Determine majority
        let favour_wins = tally.favour_weight >= tally.against_weight;
        let majority = if favour_wins {
            VoteChoice::Favour
        } else {
            VoteChoice::Against
        };

        let token_client = token::Client::new(&env, &token);
        let slash_bps: i128 = 1_000; // 10% slash

        // Return stakes; slash minority voters
        for arb in arbitrators.iter() {
            let stake_key = DataKey::DisputeStake(dispute_id.clone(), arb.clone());
            let stake: i128 = match env.storage().persistent().get(&stake_key) {
                Some(s) => s,
                None => continue,
            };

            let vote_key = DataKey::DisputeVote(dispute_id.clone(), arb.clone());
            let vote: VoteChoice = match env.storage().persistent().get(&vote_key) {
                Some(v) => v,
                None => continue,
            };

            let voted_with_majority = vote == majority;

            // Effects: remove stake record
            env.storage().persistent().remove(&stake_key);

            if voted_with_majority {
                // Return full stake
                token_client.transfer(&env.current_contract_address(), &arb, &stake);
            } else {
                // Slash 10%, return remainder
                let slash = stake * slash_bps / 10_000;
                let remainder = stake.saturating_sub(slash);
                if remainder > 0 {
                    token_client.transfer(&env.current_contract_address(), &arb, &remainder);
                }
                if slash > 0 {
                    if let Some(admin) = AccessControl::get_admin(&env) {
                        token_client.transfer(&env.current_contract_address(), &admin, &slash);
                    }
                }
            }
        }

        // Resolve or reject the dispute based on vote outcome
        if favour_wins {
            // Majority voted in favour — issue refund
            let refund_reason =
                String::from_str(&env, "Resolved by stake-weighted arbitration vote");
            if let Ok(refund_id) = Self::create_refund_internal(
                &env,
                dispute.payment_id.clone(),
                dispute.amount,
                refund_reason,
                dispute.disputer.clone(),
            ) {
                let _ = Self::process_refund_internal(&env, &operator, refund_id);
            }

            let mut d = Self::get_dispute_internal(&env, &dispute_id)?;
            d.status = DisputeStatus::Resolved;
            d.resolved_at = Some(env.ledger().timestamp());
            env.storage()
                .persistent()
                .set(&DataKey::Dispute(dispute_id.clone()), &d);
            Self::bump_dispute_ttl(&env, &dispute_id, &d.status);
        } else {
            let mut d = Self::get_dispute_internal(&env, &dispute_id)?;
            d.status = DisputeStatus::Rejected;
            d.resolved_at = Some(env.ledger().timestamp());
            env.storage()
                .persistent()
                .set(&DataKey::Dispute(dispute_id.clone()), &d);
            Self::bump_dispute_ttl(&env, &dispute_id, &d.status);
        }

        env.events().publish(
            (
                Symbol::new(&env, "DISPUTE"),
                Symbol::new(&env, "VOTE_FINALIZED"),
            ),
            (
                dispute_id,
                tally.favour_weight,
                tally.against_weight,
                favour_wins,
            ),
        );

        Ok(())
    }

    /// Get the current vote tally for a dispute.
    pub fn get_vote_tally(env: Env, dispute_id: String) -> VoteTally {
        env.storage()
            .persistent()
            .get(&DataKey::DisputeVoteTally(dispute_id))
            .unwrap_or(VoteTally {
                favour_weight: 0,
                against_weight: 0,
                vote_count: 0,
            })
    }

    pub fn get_dispute(env: Env, dispute_id: String) -> Result<Dispute, Error> {
        let mut dispute = Self::get_dispute_internal(&env, &dispute_id)?;
        let now = env.ledger().timestamp();
        if let Some(deadline) = dispute.review_deadline {
            if now > deadline
                && !dispute.escalated
                && dispute.status != DisputeStatus::Resolved
                && dispute.status != DisputeStatus::Rejected
            {
                dispute.escalated = true;
                env.storage()
                    .persistent()
                    .set(&DataKey::Dispute(dispute_id.clone()), &dispute);
                Self::bump_dispute_ttl(&env, &dispute_id, &dispute.status);
                env.events().publish(
                    (Symbol::new(&env, "DISPUTE"), Symbol::new(&env, "ESCALATED")),
                    (dispute.payment_id.clone(), dispute_id, dispute.amount),
                );
            }
        }
        Ok(dispute)
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
        billing_interval: BillingInterval,
    ) -> Result<(), Error> {
        merchant.require_auth();

        if !AccessControl::has_role(&env, &role_merchant(&env), &merchant) {
            return Err(Error::Unauthorized);
        }

        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let interval_secs = billing_interval.to_secs();

        let plan = SubscriptionPlan {
            plan_id: plan_id.clone(),
            merchant_id: merchant,
            name,
            description,
            amount,
            currency,
            interval_secs,
            billing_interval,
            active: true,
            payout_splits: Vec::new(&env),
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
        affiliate: Option<Address>,
        affiliate_fee_bps: Option<u32>,
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
            retry_count: 0,
            next_retry_at: None,
            resume_at: None,
            affiliate: affiliate.clone(),
            affiliate_fee_bps,
        };

        env.storage().persistent().set(
            &DataKey::Subscription(subscription_id.clone()),
            &subscription,
        );

        let mut payer_subscriptions = Self::get_payer_subscriptions_internal(&env, &payer);
        payer_subscriptions.push_back(subscription_id.clone());
        env.storage().persistent().set(
            &DataKey::PayerSubscriptions(payer.clone()),
            &payer_subscriptions,
        );

        env.events().publish(
            (
                Symbol::new(&env, "SUBSCRIPTION"),
                Symbol::new(&env, "CREATED"),
            ),
            (subscription_id.clone(), payer, plan_id),
        );

        Ok(subscription_id)
    }

    pub fn get_subscription(env: Env, subscription_id: String) -> Result<Subscription, Error> {
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

        let mut subscription = Self::get_subscription_internal(&env, &subscription_id)?;

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

    /// Pause a subscription until a specific timestamp, after which it
    /// automatically resumes on the next `charge_subscription` call.
    ///
    /// # Parameters
    /// * `payer`           – Must be the subscription owner; must sign.
    /// * `subscription_id` – Subscription to pause.
    /// * `resume_timestamp`– Unix timestamp at which the subscription should
    ///                       resume. Must be strictly in the future.
    pub fn pause_subscription_with_resume_date(
        env: Env,
        payer: Address,
        subscription_id: String,
        resume_timestamp: u64,
    ) -> Result<(), Error> {
        payer.require_auth();

        let now = env.ledger().timestamp();
        if resume_timestamp <= now {
            return Err(Error::InvalidResumeTimestamp);
        }

        let mut subscription = Self::get_subscription_internal(&env, &subscription_id)?;

        if subscription.payer_address != payer {
            return Err(Error::Unauthorized);
        }

        if subscription.status != SubscriptionStatus::Active {
            return Err(Error::PaymentAlreadyProcessed);
        }

        subscription.status = SubscriptionStatus::Paused;
        subscription.resume_at = Some(resume_timestamp);

        env.storage()
            .persistent()
            .set(&DataKey::Subscription(subscription_id.clone()), &subscription);

        env.events().publish(
            (
                Symbol::new(&env, "SUBSCRIPTION"),
                Symbol::new(&env, "PAUSED"),
            ),
            (subscription_id, payer, resume_timestamp),
        );

        Ok(())
    }

    /// Attempt to charge a subscription.
    ///
    /// Handles the full lifecycle including:
    /// - Auto-resuming a paused subscription whose `resume_at` has passed.
    /// - Pulling the due amount via a pre-authorization (if one exists) or
    ///   directly via the token contract.
    /// - On insufficient balance: entering a grace period with up to
    ///   `SUBSCRIPTION_MAX_RETRIES` retries spaced `SUBSCRIPTION_RETRY_INTERVAL_SECS`
    ///   apart before marking the subscription as `Cancelled`.
    ///
    /// # Parameters
    /// * `operator`        – Oracle or settlement operator; must sign.
    /// * `subscription_id` – Subscription to charge.
    /// * `token`           – Token contract to pull payment from.
    pub fn charge_subscription(
        env: Env,
        operator: Address,
        subscription_id: String,
        token: Address,
    ) -> Result<SubscriptionStatus, Error> {
        operator.require_auth();

        if !AccessControl::has_role(&env, &role_oracle(&env), &operator)
            && !AccessControl::has_role(&env, &role_settlement_operator(&env), &operator)
        {
            return Err(Error::Unauthorized);
        }

        let mut subscription = Self::get_subscription_internal(&env, &subscription_id)?;
        let now = env.ledger().timestamp();

        // ── Auto-resume if the pause window has expired ───────────────────────
        if subscription.status == SubscriptionStatus::Paused {
            if let Some(resume_at) = subscription.resume_at {
                if now >= resume_at {
                    subscription.status = SubscriptionStatus::Active;
                    subscription.resume_at = None;
                    // Push next payment forward from the resume point.
                    subscription.next_payment_at =
                        resume_at.saturating_add(subscription.interval_secs);

                    env.events().publish(
                        (
                            Symbol::new(&env, "SUBSCRIPTION"),
                            Symbol::new(&env, "RESUMED"),
                        ),
                        (subscription_id.clone(), subscription.payer_address.clone()),
                    );
                }
            }
        }

        // Only charge Active subscriptions.
        if subscription.status != SubscriptionStatus::Active {
            env.storage().persistent().set(
                &DataKey::Subscription(subscription_id.clone()),
                &subscription,
            );
            return Ok(subscription.status);
        }

        // Check whether we are in a retry window or a normal due-date window.
        let is_retry = subscription.next_retry_at.is_some();
        let due = if is_retry {
            subscription.next_retry_at.unwrap_or(0)
        } else {
            subscription.next_payment_at
        };

        if now < due {
            // Not yet due — nothing to do.
            env.storage().persistent().set(
                &DataKey::Subscription(subscription_id.clone()),
                &subscription,
            );
            return Ok(subscription.status);
        }

        // ── Attempt token transfer ────────────────────────────────────────────
        let token_client = token::Client::new(&env, &token);
        let payer = subscription.payer_address.clone();
        let merchant = subscription.merchant_id.clone();
        let amount = subscription.amount;

        // Pull the full amount into this contract so we can distribute splits/fees.
        let transfer_ok = token_client
            .try_transfer(&payer, &env.current_contract_address(), &amount)
            .is_ok();

        if transfer_ok {
            // ── Success path ──────────────────────────────────────────────────
            // Distribute according to plan splits or affiliate settings.
            // First try to resolve the plan and its payout splits.
            if let Ok(plan) = Self::get_subscription_plan(env.clone(), subscription.plan_id.clone()) {
                if plan.payout_splits.len() > 0 {
                    // If payout_splits configured, send each recipient their configured amount.
                    for s in plan.payout_splits.iter() {
                        let _ = token_client.try_transfer(
                            &env.current_contract_address(),
                            &s.recipient,
                            &s.amount,
                        );
                    }
                } else if let (Some(aff), Some(bps)) = (subscription.affiliate.clone(), subscription.affiliate_fee_bps) {
                    // Pay affiliate fee then merchant receives remainder.
                    let fee = amount.saturating_mul(bps as i128) / 10_000i128;
                    let merchant_amount = amount.saturating_sub(fee);
                    if fee > 0 {
                        let _ = token_client.try_transfer(
                            &env.current_contract_address(),
                            &aff,
                            &fee,
                        );
                    }
                    let _ = token_client.try_transfer(
                        &env.current_contract_address(),
                        &merchant,
                        &merchant_amount,
                    );
                } else {
                    // Default: send full amount to merchant.
                    let _ = token_client.try_transfer(
                        &env.current_contract_address(),
                        &merchant,
                        &amount,
                    );
                }
            } else {
                // If plan can't be loaded, fall back to sending full amount to merchant.
                let _ = token_client.try_transfer(
                    &env.current_contract_address(),
                    &merchant,
                    &amount,
                );
            }

            subscription.last_payment_at = Some(now);
            subscription.total_payments = subscription.total_payments.saturating_add(1);
            subscription.retry_count = 0;
            subscription.next_retry_at = None;
            subscription.next_payment_at = now.saturating_add(subscription.interval_secs);

            // Check max_payments cap.
            if let Some(max) = subscription.max_payments {
                if subscription.total_payments >= max {
                    subscription.status = SubscriptionStatus::Expired;
                }
            }

            env.storage().persistent().set(
                &DataKey::Subscription(subscription_id.clone()),
                &subscription,
            );

            env.events().publish(
                (
                    Symbol::new(&env, "SUBSCRIPTION"),
                    Symbol::new(&env, "CHARGED"),
                ),
                (
                    subscription_id.clone(),
                    payer.clone(),
                    merchant.clone(),
                    amount,
                    subscription.total_payments,
                ),
            );

            // Emit explicit expired event when the subscription reached its cap.
            if subscription.status == SubscriptionStatus::Expired {
                env.events().publish((Symbol::new(&env, "SUBSCRIPTION_EXPIRED"),), (subscription_id, payer));
            }
        } else {
            // ── Failure path — grace period / retry logic ─────────────────────
            subscription.retry_count = subscription.retry_count.saturating_add(1);

            if subscription.retry_count >= SUBSCRIPTION_MAX_RETRIES {
                // Exhausted all retries — cancel the subscription.
                subscription.status = SubscriptionStatus::Cancelled;
                subscription.next_retry_at = None;

                env.storage().persistent().set(
                    &DataKey::Subscription(subscription_id.clone()),
                    &subscription,
                );

                env.events().publish(
                    (
                        Symbol::new(&env, "SUBSCRIPTION"),
                        Symbol::new(&env, "CANCELLED"),
                    ),
                    (
                        subscription_id,
                        payer,
                        subscription.retry_count,
                    ),
                );

                // Also emit a single-topic cancellation event for indexers.
                env.events().publish((Symbol::new(&env, "SUBSCRIPTION_CANCELLED"),), (subscription_id, payer));

                return Err(Error::SubscriptionRetryExhausted);
            } else {
                // Schedule the next retry attempt.
                let next_retry = now.saturating_add(SUBSCRIPTION_RETRY_INTERVAL_SECS);
                subscription.next_retry_at = Some(next_retry);

                env.storage().persistent().set(
                    &DataKey::Subscription(subscription_id.clone()),
                    &subscription,
                );

                env.events().publish(
                    (
                        Symbol::new(&env, "SUBSCRIPTION"),
                        Symbol::new(&env, "PAYMENT_FAILED"),
                    ),
                    (
                        subscription_id,
                        payer,
                        subscription.retry_count,
                        next_retry,
                    ),
                );

                return Err(Error::SubscriptionInGracePeriod);
            }
        }

        Ok(subscription.status)
    }

    pub fn resume_subscription(
        env: Env,
        payer: Address,
        subscription_id: String,
    ) -> Result<(), Error> {
        payer.require_auth();

        let mut subscription = Self::get_subscription_internal(&env, &subscription_id)?;

        if subscription.payer_address != payer {
            return Err(Error::Unauthorized);
        }

        if subscription.status != SubscriptionStatus::Paused {
            return Err(Error::PaymentAlreadyProcessed);
        }

        subscription.status = SubscriptionStatus::Active;
        subscription.next_payment_at = env
            .ledger()
            .timestamp()
            .saturating_add(subscription.interval_secs);
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

        let mut subscription = Self::get_subscription_internal(&env, &subscription_id)?;

        if subscription.payer_address != payer {
            return Err(Error::Unauthorized);
        }

        subscription.status = SubscriptionStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::Subscription(subscription_id), &subscription);

        // Emit cancellation event for consumers and indexers
        env.events().publish((Symbol::new(&env, "SUBSCRIPTION_CANCELLED"),), (payer,));

        Ok(())
    }

    /// Submit usage metrics for a metered subscription.
    ///
    /// Operators call this to record usage units consumed since the last
    /// billing cycle. The subscription amount is scaled by
    /// `units_used * unit_price` and charged immediately via `charge_subscription`.
    ///
    /// # Parameters
    /// * `operator`         – Must hold oracle or settlement_operator role.
    /// * `subscription_id`  – Target subscription.
    /// * `units_used`       – Number of usage units consumed this period.
    /// * `unit_price`       – Price per unit in the subscription token's smallest unit.
    /// * `token`            – Token contract address used for the charge.
    pub fn submit_usage_metrics(
        env: Env,
        operator: Address,
        subscription_id: String,
        units_used: i128,
        unit_price: i128,
        token: Address,
    ) -> Result<SubscriptionStatus, Error> {
        operator.require_auth();

        if !AccessControl::has_role(&env, &role_oracle(&env), &operator)
            && !AccessControl::has_role(&env, &role_settlement_operator(&env), &operator)
        {
            return Err(Error::Unauthorized);
        }

        if units_used <= 0 || unit_price <= 0 {
            return Err(Error::InvalidAmount);
        }

        let mut subscription = Self::get_subscription_internal(&env, &subscription_id)?;

        // Override the subscription amount with the metered charge for this cycle.
        let metered_amount = units_used.saturating_mul(unit_price);
        subscription.amount = metered_amount;
        env.storage()
            .persistent()
            .set(&DataKey::Subscription(subscription_id.clone()), &subscription);

        env.events().publish(
            (
                Symbol::new(&env, "SUBSCRIPTION"),
                Symbol::new(&env, "USAGE_RECORDED"),
            ),
            (subscription_id.clone(), units_used, unit_price, metered_amount),
        );

        // Trigger the charge at the updated metered amount.
        Self::charge_subscription(env, operator, subscription_id, token)
    }

    /// Process due subscriptions - called by an operator or oracle
    pub fn process_due_subscriptions(env: Env, operator: Address) -> Result<u32, Error> {
        operator.require_auth();

        if !AccessControl::has_role(&env, &role_oracle(&env), &operator)
            && !AccessControl::has_role(&env, &role_settlement_operator(&env), &operator)
        {
            return Err(Error::Unauthorized);
        }

        let processed_count = 0u32;

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

#[cfg_attr(
    any(not(target_arch = "wasm32"), feature = "contract-payment-processor"),
    contractimpl
)]
#[allow(deprecated)] // events::publish — migrate to #[contractevent] in a follow-up
impl PaymentProcessor {
    pub fn version() -> u32 {
        1
    }

    fn validate_init_admin(env: &Env, admin: Address) -> Result<(), Error> {
        let zero_address = Address::from_str(env, ZERO_CONTRACT_STRKEY);
        if admin == zero_address {
            return Err(Error::InvalidAddress);
        }
        Ok(())
    }

    pub fn initialize_payment_processor(env: Env, admin: Address) -> Result<(), Error> {
        Self::validate_init_admin(&env, admin.clone())?;
        AccessControl::initialize(&env, admin);

        let empty_reason = String::from_str(&env, "");
        let initial_state = PauseState {
            paused: false,
            reason: empty_reason,
            admin: None,
            timestamp: env.ledger().timestamp(),
        };

        env.storage()
            .persistent()
            .set(&DataKey::Paused, &initial_state);
        env.storage()
            .persistent()
            .set(&DataKey::CreationPaused, &initial_state);
        Ok(())
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
    pub fn set_global_pause(
        env: Env,
        admin: Address,
        paused: bool,
        reason: String,
    ) -> Result<(), Error> {
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
    pub fn set_creation_pause(
        env: Env,
        admin: Address,
        paused: bool,
        reason: String,
    ) -> Result<(), Error> {
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

        env.storage()
            .persistent()
            .set(&DataKey::CreationPaused, &state);

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

        let global = env
            .storage()
            .persistent()
            .get::<DataKey, PauseState>(&DataKey::Paused)
            .unwrap_or_else(|| default_state.clone());

        let creation = env
            .storage()
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
        env.storage().persistent().get(&DataKey::GlobalAmountLimits)
    }

    /// Enforce amount limits: merchant-specific limits take precedence over global limits.
    fn enforce_amount_limits(env: &Env, merchant_id: &Address, amount: i128) -> Result<(), Error> {
        let limits: Option<AmountLimits> = env
            .storage()
            .persistent()
            .get(&DataKey::MerchantAmountLimits(merchant_id.clone()))
            .or_else(|| env.storage().persistent().get(&DataKey::GlobalAmountLimits));

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
    pub fn create_payment(env: Env, args: CreatePaymentArgs) -> Result<PaymentCharge, Error> {
        Self::require_creation_not_paused(&env)?;
        args.merchant_id.require_auth();

        // Idempotency check: if client_token was already used, return the existing payment
        // (or error if it maps to a different payment_id).
        if let Some(ref token) = args.client_token {
            let key = DataKey::IdempotencyKey(token.clone());
            if let Some(existing_id) = env.storage().persistent().get::<DataKey, String>(&key) {
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

        // Issue #164: Validate token against admin-approved allowlist
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
            None => now.saturating_add(args.duration_secs.unwrap_or(DEFAULT_PAYMENT_DURATION_SECS)),
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
            metadata_hash: args.metadata_hash.clone(),
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

        // Issue #166: Optimize event topics for high-volume ingestions
        // Use standardized (Symbol, Symbol, Address) format for efficient wildcard filtering
        env.events().publish(
            (
                Symbol::new(&env, "PAYMENT"),
                Symbol::new(&env, "CREATED"),
                args.merchant_id.clone(),
            ),
            (args.payment_id.clone(), args.amount),
        );

        // Persist idempotency key → payment_id mapping so retries are safe.
        if let Some(token) = args.client_token {
            let key = DataKey::IdempotencyKey(token);
            env.storage().persistent().set(&key, &args.payment_id);
            Self::bump_ttl(&env, &key, LONG_LIVE_TTL);
        }

        Ok(payment)
    }

    /// Issue #165: Batch payment creation for optimized gas usage.
    /// Creates multiple payment charges in a single transaction.
    /// Reverts all if any element violates validation rules.
    #[allow(deprecated)]
    pub fn create_payments_batch(
        env: Env,
        args_list: Vec<CreatePaymentArgs>,
    ) -> Result<Vec<String>, Error> {
        Self::require_creation_not_paused(&env)?;

        if args_list.is_empty() {
            return Ok(vec![&env]);
        }

        // Validate all payments first before creating any
        for args in args_list.iter() {
            args.merchant_id.require_auth();

            // Verify merchant role
            if !AccessControl::has_role(&env, &role_merchant(&env), &args.merchant_id) {
                return Err(Error::Unauthorized);
            }

            // Issue #164: Validate token against allowlist
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

            // Validate merchant is verified and active
            if let Some(registry_address) = env
                .storage()
                .persistent()
                .get::<DataKey, Address>(&DataKey::MerchantRegistryAddress)
            {
                let registry_client =
                    crate::merchant_registry::MerchantRegistryClient::new(&env, &registry_address);
                match registry_client.try_get_merchant(&args.merchant_id) {
                    Ok(Ok(merchant)) => {
                        if merchant.kyc_tier == crate::merchant_registry::KycTier::Unverified
                            || !merchant.active
                            || merchant.suspension_reason.is_some()
                        {
                            return Err(Error::Unauthorized);
                        }
                    }
                    _ => {
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

            // Check idempotency
            if let Some(ref token) = args.client_token {
                let key = DataKey::IdempotencyKey(token.clone());
                if let Some(existing_id) = env.storage().persistent().get::<DataKey, String>(&key) {
                    if existing_id != args.payment_id {
                        return Err(Error::DuplicateIdempotencyKey);
                    }
                }
            }
        }

        // All validations passed, now create all payments
        let mut payment_ids = vec![&env];
        let now = env.ledger().timestamp();

        for args in args_list.iter() {
            // Rate limit check per merchant
            Self::enforce_create_payment_rate_limit(&env, &args.merchant_id)?;

            let resolved_expires_at = match args.expires_at {
                Some(ts) => ts,
                None => now.saturating_add(args.duration_secs.unwrap_or(DEFAULT_PAYMENT_DURATION_SECS)),
            };
            if resolved_expires_at <= now {
                return Err(Error::InvalidExpiry);
            }

            let payment = PaymentCharge {
                payment_id: args.payment_id.clone(),
                merchant_id: args.merchant_id.clone(),
                amount: args.amount,
                currency: args.currency.clone(),
                deposit_address: args.deposit_address.clone(),
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

            // Issue #166: Optimize event topics
            env.events().publish(
                (
                    Symbol::new(&env, "PAYMENT"),
                    Symbol::new(&env, "CREATED"),
                    args.merchant_id.clone(),
                ),
                (args.payment_id.clone(), args.amount),
            );

            // Persist idempotency key
            if let Some(ref token) = args.client_token {
                let key = DataKey::IdempotencyKey(token.clone());
                env.storage().persistent().set(&key, &args.payment_id);
                Self::bump_ttl(&env, &key, LONG_LIVE_TTL);
            }

            payment_ids.push_back(args.payment_id.clone());
        }

        // Emit batch creation event
        env.events().publish(
            (
                Symbol::new(&env, "PAYMENT"),
                Symbol::new(&env, "BATCH_CREATED"),
            ),
            payment_ids.len(),
        );

        Ok(payment_ids)
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
                let exp = decimals - 6;
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

        // Issue #166: Optimize event topics for efficient indexing
        env.events().publish(
            (Symbol::new(&env, "PAYMENT"), event_name, payment.merchant_id.clone()),
            (payment_id.clone(), payment.amount, amount_received),
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

            // Issue #166: Optimize event topics
            env.events().publish(
                (
                    Symbol::new(&env, "PAYMENT"),
                    Symbol::new(&env, "EXPIRED"),
                    payment.merchant_id.clone(),
                ),
                (payment_id.clone(), payment.amount),
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

        // Issue #166: Optimize event topics
        env.events().publish(
            (
                Symbol::new(&env, "PAYMENT"),
                Symbol::new(&env, "CANCELLED"),
                payment.merchant_id.clone(),
            ),
            (payment_id.clone(), payment.amount),
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

        // Issue #166: Optimize event topics
        env.events().publish(
            (
                Symbol::new(&env, "PAYMENT"),
                Symbol::new(&env, "EXPIRED"),
                payment.merchant_id.clone(),
            ),
            (payment_id.clone(), payment.amount),
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

        // Issue #166: Optimize event topics
        env.events().publish(
            (
                Symbol::new(&env, "PAYMENT"),
                Symbol::new(&env, "SETTLED"),
                payment.merchant_id.clone(),
            ),
            (payment_id.clone(), payment.amount),
        );

        Ok(())
    }

    /// Validate DEX path quotes before executing a swap.
    /// Blocks circular routes and rejects paths whose quoted output is below the minimum.
    fn validate_path_returns(
        env: &Env,
        dex_router: &Address,
        token_in: &Address,
        amount_in: i128,
        amount_out_min: i128,
        path: &Vec<Address>,
    ) -> Result<Vec<i128>, Error> {
        if path.len() < 2 {
            return Err(Error::SwapPathInvalid);
        }

        if path.get(0) != Some(token_in.clone()) {
            return Err(Error::SwapPathInvalid);
        }

        // Circular paths are a common arbitrage exploitation pattern.
        for i in 0..path.len() {
            for j in (i + 1)..path.len() {
                if path.get(i) == path.get(j) {
                    return Err(Error::ArbitrageDetected);
                }
            }
        }

        let dex_client = DexRouterClient::new(env, dex_router);
        let amounts = dex_client.get_amounts_out(&amount_in, path);

        if amounts.len() != path.len() {
            return Err(Error::SwapPathInvalid);
        }

        if amounts.get(0) != Some(amount_in) {
            return Err(Error::SwapPathInvalid);
        }

        let quoted_out = amounts.get(path.len() - 1).ok_or(Error::SwapPathInvalid)?;
        if quoted_out < amount_out_min {
            return Err(Error::SwapPathInvalid);
        }

        Ok(amounts)
    }

    /// Compare DEX quoted output against a fresh oracle reference rate.
    fn validate_oracle_swap_rate(
        env: &Env,
        fx_oracle: &Address,
        oracle_pair: &Symbol,
        amount_in: i128,
        dex_quoted_out: i128,
        max_deviation_bps: u32,
    ) -> Result<(), Error> {
        let oracle_client = FXOracleClient::new(env, fx_oracle);
        let rate_data = match oracle_client.try_get_rate(oracle_pair) {
            Ok(Ok(data)) => data,
            _ => return Err(Error::OraclePriceDeviation),
        };

        let mut divisor = 1i128;
        for _ in 0..rate_data.decimals {
            divisor = divisor.saturating_mul(10);
        }

        let expected_out = amount_in
            .saturating_mul(rate_data.rate)
            .checked_div(divisor)
            .unwrap_or(0);
        if expected_out <= 0 {
            return Err(Error::OraclePriceDeviation);
        }

        let diff = if dex_quoted_out > expected_out {
            dex_quoted_out - expected_out
        } else {
            expected_out - dex_quoted_out
        };

        let deviation_bps = diff.saturating_mul(10_000) / expected_out;
        if deviation_bps > max_deviation_bps as i128 {
            return Err(Error::OraclePriceDeviation);
        }

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
    pub fn swap_and_pay(env: Env, args: SwapAndPayArgs) -> Result<PaymentCharge, Error> {
        args.payer.require_auth();

        // Validate inputs
        if args.amount <= 0 || args.amount_in <= 0 {
            return Err(Error::InvalidAmount);
        }

        if args.amount_out_min < args.amount {
            return Err(Error::SwapPathInvalid);
        }

        // Issue #226: check path returns before executing the swap.
        let quoted_amounts = Self::validate_path_returns(
            &env,
            &args.dex_router,
            &args.token_in,
            args.amount_in,
            args.amount_out_min,
            &args.path,
        )?;

        if let (Some(fx_oracle), Some(oracle_pair)) = (&args.fx_oracle, &args.oracle_pair) {
            let quoted_out = quoted_amounts
                .get(args.path.len() - 1)
                .ok_or(Error::SwapPathInvalid)?;
            Self::validate_oracle_swap_rate(
                &env,
                fx_oracle,
                oracle_pair,
                args.amount_in,
                quoted_out,
                args.max_deviation_bps,
            )?;
        }

        // Execute atomic swap via DEX router
        let deadline = env.ledger().timestamp().saturating_add(3_600); // 1 hour deadline

        let dex_client = DexRouterClient::new(&env, &args.dex_router);

        // Perform the swap - this transfers tokens from payer and sends output to deposit_address
        let swap_result = dex_client.swap_exact_tokens_for_tokens(
            &args.amount_in,
            &args.amount_out_min,
            &args.path,
            &args.deposit_address,
            &deadline,
        );

        let actual_out = swap_result
            .get(args.path.len() - 1)
            .ok_or(Error::SwapPathInvalid)?;
        if actual_out < args.amount_out_min {
            return Err(Error::SwapPathInvalid);
        }

        let quoted_out = quoted_amounts
            .get(args.path.len() - 1)
            .ok_or(Error::SwapPathInvalid)?;
        if actual_out < quoted_out {
            return Err(Error::ArbitrageDetected);
        }

        // Now create the payment with the swapped amount
        let settlement_token = args
            .path
            .get(args.path.len() - 1)
            .unwrap_or(args.token_in.clone());
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
            token_address: Some(settlement_token),
            client_token: None,
            metadata_hash: None,
        };

        let payment = Self::create_payment(env.clone(), create_args)?;

        // Emit SWAP/AND/PAY event
        env.events().publish(
            (
                Symbol::new(&env, "SWAP"),
                Symbol::new(&env, "AND"),
                Symbol::new(&env, "PAY"),
            ),
            (
                args.payment_id,
                args.payer,
                args.amount_in,
                args.token_in,
                args.amount,
            ),
        );
        Ok(payment)
    }

    #[allow(dead_code)]
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

    // ─── Merchant pre-authorization (pull payments) ───────────────────────────

    /// Customer grants a merchant permission to pull up to `limit_per_period`
    /// tokens per `period_secs`-second window.
    pub fn pre_authorize_merchant(
        env: Env,
        customer: Address,
        merchant: Address,
        token: Address,
        limit_per_period: i128,
        period_secs: u64,
    ) -> Result<MerchantAuthorization, MerchantAuthError> {
        MerchantPreAuth::pre_authorize_merchant(
            env,
            customer,
            merchant,
            token,
            limit_per_period,
            period_secs,
        )
    }

    /// Customer revokes a previously granted merchant authorization.
    pub fn revoke_merchant_authorization(
        env: Env,
        customer: Address,
        merchant: Address,
    ) -> Result<(), MerchantAuthError> {
        MerchantPreAuth::revoke_authorization(env, customer, merchant)
    }

    /// Merchant pulls `amount` tokens from the customer's account against
    /// an existing pre-authorization.
    pub fn pull_payment(
        env: Env,
        merchant: Address,
        customer: Address,
        amount: i128,
    ) -> Result<i128, MerchantAuthError> {
        MerchantPreAuth::pull_payment(env, merchant, customer, amount)
    }

    /// Return the stored authorization for a (customer, merchant) pair.
    pub fn get_merchant_authorization(
        env: Env,
        customer: Address,
        merchant: Address,
    ) -> Result<MerchantAuthorization, MerchantAuthError> {
        MerchantPreAuth::get_authorization(env, customer, merchant)
    }

    /// Return the remaining pull budget for the current period.
    pub fn merchant_authorization_remaining(
        env: Env,
        customer: Address,
        merchant: Address,
    ) -> Result<i128, MerchantAuthError> {
        MerchantPreAuth::remaining_limit(env, customer, merchant)
    }

    pub fn cancel_stream(env: Env, sender: Address, stream_id: String) -> Result<(), StreamError> {
        PaymentStreaming::cancel_stream(env, sender, stream_id)
    }
    pub fn cancel_multiple_streams(
        env: Env,
        sender: Address,
        stream_ids: Vec<String>,
    ) -> Result<Vec<String>, StreamError> {
        PaymentStreaming::cancel_multiple_streams(env, sender, stream_ids)
    }

    pub fn batch_cancel_streams(
        env: Env,
        sender: Address,
        stream_ids: Vec<String>,
    ) -> Result<Vec<String>, StreamError> {
        PaymentStreaming::batch_cancel_streams(env, sender, stream_ids)
    }

    pub fn batch_withdraw_to(
        env: Env,
        recipient: Address,
        withdrawals: Vec<WithdrawalRecipient>,
    ) -> Result<Vec<String>, StreamError> {
        PaymentStreaming::batch_withdraw_to(env, recipient, withdrawals)
    }

    pub fn withdraw_all_for_recipient(
        env: Env,
        recipient: Address,
        max_streams: u32,
    ) -> Result<Vec<String>, StreamError> {
        PaymentStreaming::withdraw_all_for_recipient(env, recipient, max_streams)
    }

    pub fn trigger_withdrawal(env: Env, stream_id: String) -> Result<String, StreamError> {
        PaymentStreaming::trigger_withdrawal(env, stream_id)
    }

    pub fn set_stream_destination(
        env: Env,
        recipient: Address,
        stream_id: String,
        destination: Address,
    ) -> Result<(), StreamError> {
        PaymentStreaming::set_stream_destination(env, recipient, stream_id, destination)
    }

    pub fn get_sender_streams(
        env: Env,
        sender: Address,
        page: u32,
        page_size: u32,
    ) -> Vec<PaymentStream> {
        PaymentStreaming::get_sender_streams(env, sender, page, page_size)
    }

    pub fn get_stream(env: Env, stream_id: String) -> Result<PaymentStream, StreamError> {
        PaymentStreaming::get_stream(env, stream_id)
    }

    /// Create a new payment stream. Tokens are pulled from `sender` into the contract.
    pub fn create_stream(
        env: Env,
        sender: Address,
        receiver: Address,
        token: Address,
        rate_per_second: i128,
        deposit: i128,
        stream_id: String,
    ) -> Result<PaymentStream, StreamError> {
        PaymentStreaming::create_stream(
            env,
            sender,
            receiver,
            token,
            rate_per_second,
            deposit,
            stream_id,
        )
    }

    pub fn top_up_multiple_streams(
        env: Env,
        sender: Address,
        top_ups: Vec<(String, i128)>,
    ) -> Result<(), StreamError> {
        PaymentStreaming::top_up_multiple_streams(env, sender, top_ups)
    }

    /// Update the flow rate of an active stream (increase or decrease).
    pub fn update_stream_rate(
        env: Env,
        sender: Address,
        stream_id: String,
        new_rate: i128,
    ) -> Result<(), StreamError> {
        PaymentStreaming::update_stream_rate(env, sender, stream_id, new_rate)
    }

    /// Close a terminal (Exhausted/Cancelled) stream and remove its storage entry.
    pub fn close_expired_stream(env: Env, stream_id: String) -> Result<(), StreamError> {
        PaymentStreaming::close_expired_stream(env, stream_id)
    }
}

#[cfg(test)]
mod arbitrage_test;
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
#[cfg(test)]
mod oracle_sanitization_test;
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
pub use stream::{PaymentStream, PaymentStreaming, StreamError, StreamStatus};
#[cfg(test)]
mod stream_test;

pub mod utils;
pub use utils::format_id;

pub mod gas_estimator;
pub use gas_estimator::{CostEstimate, GasEstimator, GasEstimatorClient, Operation};
