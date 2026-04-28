use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, vec, Address, Env, String, Symbol, Vec,
};

#[contract]
pub struct MerchantRegistry;

/// KYC tier for merchants, replacing the binary `verified: bool` field.
/// Allows payment limits and settlement schedules to vary by tier.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KycTier {
    Unverified,
    Basic,
    Full,
    Business,
}

/// Fee configuration for a merchant.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeeConfig {
    /// Platform fee in basis points (100 bps = 1%). 0 means no fee.
    pub platform_fee_bps: i32,
    /// Fixed fee per transaction in the smallest currency unit.
    pub fixed_fee: i128,
    /// Optional: custom fee recipient address (defaults to admin if None).
    pub fee_recipient: Option<Address>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Merchant {
    pub merchant_id: Address,
    pub business_name: String,
    pub settlement_currency: String,
    /// On-chain address where settled funds are sent.
    pub payout_address: Option<Address>,
    /// Off-chain bank account reference for fiat payouts.
    pub bank_account: Option<String>,
    /// KYC tier replaces the old `verified: bool` field.
    pub kyc_tier: KycTier,
    pub active: bool,
    pub created_at: u64,
    pub suspension_reason: Option<String>,
    pub suspended_at: Option<u64>,
    /// Custom fee configuration for this merchant.
    pub fee_config: Option<FeeConfig>,
}

#[contracttype]
pub enum MerchantDataKey {
    Merchant(Address),
    Admin,
    /// Stores the list of all registered merchants for enumeration
    MerchantList,
    /// Optional: Address of the RefundManager contract for automatic MERCHANT role granting
    RefundManagerAddress,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum MerchantError {
    MerchantAlreadyExists = 1,
    MerchantNotFound = 2,
    Unauthorized = 3,
    NotVerified = 4,
    AdminAlreadySet = 5,
}

#[contractimpl]
#[allow(deprecated)] // events::publish — migrate to #[contractevent] in a follow-up
impl MerchantRegistry {
    pub fn version() -> u32 {
        1
    }

    /// Initialize the contract with an admin address
    pub fn initialize(env: Env, admin: Address) -> Result<(), MerchantError> {
        if env.storage().persistent().has(&MerchantDataKey::Admin) {
            return Err(MerchantError::AdminAlreadySet);
        }
        env.storage()
            .persistent()
            .set(&MerchantDataKey::Admin, &admin);
        Ok(())
    }

    /// Register a new merchant
    pub fn register_merchant(
        env: Env,
        merchant_id: Address,
        business_name: String,
        settlement_currency: String,
        payout_address: Option<Address>,
        bank_account: Option<String>,
        fee_config: Option<FeeConfig>,
    ) -> Result<(), MerchantError> {
        merchant_id.require_auth();

        if env
            .storage()
            .persistent()
            .has(&MerchantDataKey::Merchant(merchant_id.clone()))
        {
            return Err(MerchantError::MerchantAlreadyExists);
        }

        let merchant = Merchant {
            merchant_id: merchant_id.clone(),
            business_name,
            settlement_currency,
            payout_address,
            bank_account,
            kyc_tier: KycTier::Unverified,
            active: true,
            created_at: env.ledger().timestamp(),
            suspension_reason: None,
            suspended_at: None,
            fee_config,
        };

        env.storage()
            .persistent()
            .set(&MerchantDataKey::Merchant(merchant_id.clone()), &merchant);

        Self::add_to_merchant_list(&env, &merchant_id);

        Ok(())
    }

    /// Update merchant settings
    pub fn update_merchant(
        env: Env,
        merchant_id: Address,
        business_name: Option<String>,
        settlement_currency: Option<String>,
        active: Option<bool>,
        payout_address: Option<Address>,
        bank_account: Option<String>,
        fee_config: Option<FeeConfig>,
    ) -> Result<(), MerchantError> {
        merchant_id.require_auth();

        let mut merchant = Self::get_merchant_internal(&env, &merchant_id)?;

        if let Some(name) = business_name {
            merchant.business_name = name;
        }
        if let Some(currency) = settlement_currency {
            merchant.settlement_currency = currency;
        }
        if let Some(is_active) = active {
            merchant.active = is_active;
        }
        if let Some(addr) = payout_address {
            merchant.payout_address = Some(addr);
        }
        if let Some(acct) = bank_account {
            merchant.bank_account = Some(acct);
        }
        if let Some(config) = fee_config {
            merchant.fee_config = Some(config);
        }

        env.storage()
            .persistent()
            .set(&MerchantDataKey::Merchant(merchant_id.clone()), &merchant);

        env.events().publish(
            (Symbol::new(&env, "MERCHANT"), Symbol::new(&env, "UPDATED")),
            merchant_id,
        );

        Ok(())
    }

    /// Get merchant info
    pub fn get_merchant(env: Env, merchant_id: Address) -> Result<Merchant, MerchantError> {
        Self::get_merchant_internal(&env, &merchant_id)
    }

    /// Verify merchant (admin only) — sets KycTier::Basic for backward compatibility.
    /// If a RefundManager address is configured, also grants the MERCHANT role there.
    pub fn verify_merchant(
        env: Env,
        admin: Address,
        merchant_id: Address,
    ) -> Result<(), MerchantError> {
        admin.require_auth();

        let stored_admin: Address = env
            .storage()
            .persistent()
            .get(&MerchantDataKey::Admin)
            .ok_or(MerchantError::Unauthorized)?;

        if admin != stored_admin {
            return Err(MerchantError::Unauthorized);
        }

        let mut merchant = Self::get_merchant_internal(&env, &merchant_id)?;
        merchant.kyc_tier = KycTier::Basic;

        env.storage()
            .persistent()
            .set(&MerchantDataKey::Merchant(merchant_id.clone()), &merchant);

        // If RefundManager is configured, grant the MERCHANT role
        if let Some(refund_manager_addr) = env
            .storage()
            .persistent()
            .get::<MerchantDataKey, Address>(&MerchantDataKey::RefundManagerAddress)
        {
            let rm_client = crate::RefundManagerClient::new(&env, &refund_manager_addr);
            let _ = rm_client.try_grant_role(&admin, &Symbol::new(&env, "MERCHANT"), &merchant_id);
        }

        env.events().publish(
            (Symbol::new(&env, "MERCHANT"), Symbol::new(&env, "VERIFIED")),
            merchant_id,
        );

        Ok(())
    }

    /// Set a specific KYC tier for a merchant (admin only).
    pub fn set_kyc_tier(
        env: Env,
        admin: Address,
        merchant_id: Address,
        tier: KycTier,
    ) -> Result<(), MerchantError> {
        admin.require_auth();

        let stored_admin: Address = env
            .storage()
            .persistent()
            .get(&MerchantDataKey::Admin)
            .ok_or(MerchantError::Unauthorized)?;

        if admin != stored_admin {
            return Err(MerchantError::Unauthorized);
        }

        let mut merchant = Self::get_merchant_internal(&env, &merchant_id)?;
        merchant.kyc_tier = tier;

        env.storage()
            .persistent()
            .set(&MerchantDataKey::Merchant(merchant_id), &merchant);

        Ok(())
    }

    /// Set the RefundManager contract address for automatic MERCHANT role granting
    pub fn set_refund_manager_address(
        env: Env,
        admin: Address,
        refund_manager: Address,
    ) -> Result<(), MerchantError> {
        admin.require_auth();

        let stored_admin: Address = env
            .storage()
            .persistent()
            .get(&MerchantDataKey::Admin)
            .ok_or(MerchantError::Unauthorized)?;

        if admin != stored_admin {
            return Err(MerchantError::Unauthorized);
        }

        env.storage()
            .persistent()
            .set(&MerchantDataKey::RefundManagerAddress, &refund_manager);

        Ok(())
    }

    /// Suspend a merchant (admin only)
    pub fn suspend_merchant(
        env: Env,
        admin: Address,
        merchant_id: Address,
        reason: String,
    ) -> Result<(), MerchantError> {
        admin.require_auth();

        let stored_admin: Address = env
            .storage()
            .persistent()
            .get(&MerchantDataKey::Admin)
            .ok_or(MerchantError::Unauthorized)?;

        if admin != stored_admin {
            return Err(MerchantError::Unauthorized);
        }

        let mut merchant = Self::get_merchant_internal(&env, &merchant_id)?;
        merchant.active = false;
        merchant.suspension_reason = Some(reason);
        merchant.suspended_at = Some(env.ledger().timestamp());

        env.storage()
            .persistent()
            .set(&MerchantDataKey::Merchant(merchant_id.clone()), &merchant);

        env.events().publish(
            (
                Symbol::new(&env, "MERCHANT"),
                Symbol::new(&env, "SUSPENDED"),
            ),
            merchant_id,
        );

        Ok(())
    }

    /// Reinstate a suspended merchant (admin only)
    pub fn reinstate_merchant(
        env: Env,
        admin: Address,
        merchant_id: Address,
    ) -> Result<(), MerchantError> {
        admin.require_auth();

        let stored_admin: Address = env
            .storage()
            .persistent()
            .get(&MerchantDataKey::Admin)
            .ok_or(MerchantError::Unauthorized)?;

        if admin != stored_admin {
            return Err(MerchantError::Unauthorized);
        }

        let mut merchant = Self::get_merchant_internal(&env, &merchant_id)?;
        merchant.active = true;
        merchant.suspension_reason = None;
        merchant.suspended_at = None;

        env.storage()
            .persistent()
            .set(&MerchantDataKey::Merchant(merchant_id.clone()), &merchant);

        env.events().publish(
            (
                Symbol::new(&env, "MERCHANT"),
                Symbol::new(&env, "REINSTATED"),
            ),
            merchant_id,
        );

        Ok(())
    }

    /// Get all registered merchants with pagination support
    pub fn get_all_merchants(env: Env, offset: u32, limit: u32) -> Vec<Merchant> {
        let merchant_ids: Vec<Address> = env
            .storage()
            .persistent()
            .get(&MerchantDataKey::MerchantList)
            .unwrap_or_else(|| vec![&env]);

        if limit == 0 {
            return vec![&env];
        }

        let mut result = vec![&env];
        let end = core::cmp::min(merchant_ids.len(), offset.saturating_add(limit));

        let mut i = offset;
        while i < end {
            if let Some(merchant_id) = merchant_ids.get(i) {
                if let Ok(merchant) = Self::get_merchant_internal(&env, &merchant_id) {
                    result.push_back(merchant);
                }
            }
            i += 1;
        }

        result
    }

    /// Get all verified merchants (kyc_tier != Unverified)
    pub fn get_verified_merchants(env: Env) -> Vec<Merchant> {
        let merchant_ids: Vec<Address> = env
            .storage()
            .persistent()
            .get(&MerchantDataKey::MerchantList)
            .unwrap_or_else(|| vec![&env]);

        let mut result = vec![&env];
        for merchant_id in merchant_ids.iter() {
            if let Ok(merchant) = Self::get_merchant_internal(&env, &merchant_id) {
                if merchant.kyc_tier != KycTier::Unverified {
                    result.push_back(merchant);
                }
            }
        }

        result
    }

    // Helper functions
    fn add_to_merchant_list(env: &Env, merchant_id: &Address) {
        let key = MerchantDataKey::MerchantList;
        let mut merchants: Vec<Address> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| vec![env]);

        // Only add if not already present
        let mut found = false;
        for m in merchants.iter() {
            if m == *merchant_id {
                found = true;
                break;
            }
        }

        if !found {
            merchants.push_back(merchant_id.clone());
            env.storage().persistent().set(&key, &merchants);
        }
    }

    /// Calculate the platform fee for a given amount based on merchant's FeeConfig.
    /// Returns (platform_fee, net_amount).
    pub fn calculate_fee(env: Env, merchant_id: Address, amount: i128) -> Result<(i128, i128), MerchantError> {
        let merchant = Self::get_merchant_internal(&env, &merchant_id)?;
        
        if let Some(ref config) = merchant.fee_config {
            if config.platform_fee_bps == 0 && config.fixed_fee == 0 {
                return Ok((0, amount));
            }

            // Calculate percentage fee
            let percentage_fee = (amount * (config.platform_fee_bps as i128)) / 10_000;
            
            // Total fee is percentage + fixed
            let total_fee = percentage_fee.saturating_add(config.fixed_fee);
            
            // Ensure fee doesn't exceed amount
            if total_fee >= amount {
                return Ok((amount, 0));
            }

            let net_amount = amount.saturating_sub(total_fee);
            Ok((total_fee, net_amount))
        } else {
            // No fee config, no fee
            Ok((0, amount))
        }
    }

    /// Get the fee recipient address for a merchant.
    /// Returns the custom fee recipient if set, otherwise the admin address.
    pub fn get_fee_recipient(env: Env, merchant_id: Address) -> Result<Address, MerchantError> {
        let merchant = Self::get_merchant_internal(&env, &merchant_id)?;
        
        if let Some(ref config) = merchant.fee_config {
            if let Some(recipient) = &config.fee_recipient {
                return Ok(recipient.clone());
            }
        }
        
        // Default to admin if no custom recipient
        let admin: Address = env
            .storage()
            .persistent()
            .get(&MerchantDataKey::Admin)
            .ok_or(MerchantError::Unauthorized)?;
        
        Ok(admin)
    }

    fn get_merchant_internal(env: &Env, merchant_id: &Address) -> Result<Merchant, MerchantError> {
        env.storage()
            .persistent()
            .get(&MerchantDataKey::Merchant(merchant_id.clone()))
            .ok_or(MerchantError::MerchantNotFound)
    }
}
