#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, log, symbol_short, token, vec, Address,
    BytesN, Env, Vec, Symbol,
};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserSpending {
    pub last_reset_time: u64,
    pub accumulated_amount: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Payment {
    pub sender: Address,
    pub recipient: Address,
    pub token_address: Address,
    pub amount: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    PlatformTreasury,
    FeeBps,
    FeeCap,
    Paused,
    MaxAmount,
    UserVolume(Address),
    UserSpending(Address),
    Blacklist(Address),
}

/// Contract-level errors returned instead of panicking, so callers get a
/// specific, stable error code to branch on rather than an opaque trap.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    /// Caller is not authorized to perform this action (e.g. not the admin).
    Unauthorized = 1,
    /// Sender's token balance is lower than the requested payment amount.
    InsufficientBalance = 2,
    /// Requested amount is outside allowed bounds, or a spending limit was exceeded.
    LimitExceeded = 3,
    /// `initialize` was called on a contract that already has an admin set.
    AlreadyInitialized = 4,
    /// An admin-configured value (treasury, fee, admin) was read before `initialize`.
    NotInitialized = 5,
    Paused = 6,
    InvalidFeeRate = 7,
    /// Sender and recipient addresses are the same (self-routing not allowed).
    InvalidRecipient = 8,
    /// Recipient address is blacklisted.
    Blacklisted = 9,
}

#[contract]
pub struct PaymentRouter;

#[contractimpl]
impl PaymentRouter {
    const BPS_DIVISOR: i128 = 10_000;
    const XLM_DECIMALS: i128 = 10_000_000;
    const MAX_AMOUNT: i128 = 1_000_000_000_000_000; // 100M tokens with 7 decimals
    const DAILY_MAX_LIMIT: i128 = 1_000_000 * Self::XLM_DECIMALS; // 1M tokens limit
    const VOLUME_THRESHOLD: i128 = 10_000 * Self::XLM_DECIMALS; // 10,000 XLM threshold for tiered fee discount
    const SECONDS_IN_24H: u64 = 24 * 3600;
    const VERSION: u32 = 1;

    const DAY_IN_LEDGERS: u32 = 17280;
    const INSTANCE_BUMP_AMOUNT: u32 = 7 * Self::DAY_IN_LEDGERS;
    const INSTANCE_LIFETIME_THRESHOLD: u32 = Self::INSTANCE_BUMP_AMOUNT - Self::DAY_IN_LEDGERS;

    const USER_BUMP_AMOUNT: u32 = 30 * Self::DAY_IN_LEDGERS;
    const USER_LIFETIME_THRESHOLD: u32 = Self::USER_BUMP_AMOUNT - Self::DAY_IN_LEDGERS;
    const PERSISTENT_BUMP_AMOUNT: u32 = Self::USER_BUMP_AMOUNT;
    const PERSISTENT_LIFETIME_THRESHOLD: u32 = Self::USER_LIFETIME_THRESHOLD;

    // ── Private helpers ──────────────────────────────────────────────────────

    fn require_admin(env: &Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)
    }

    fn load_fee_config(env: &Env) -> Result<(Address, i128, i128), Error> {
        let platform_treasury: Address = env
            .storage()
            .instance()
            .get(&DataKey::PlatformTreasury)
            .ok_or(Error::NotInitialized)?;
        let fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeBps)
            .ok_or(Error::NotInitialized)?;
        let fee_cap: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeCap)
            .ok_or(Error::NotInitialized)?;

        env.storage().instance().extend_ttl(
            Self::INSTANCE_LIFETIME_THRESHOLD,
            Self::INSTANCE_BUMP_AMOUNT,
        );

        Ok((platform_treasury, fee_bps, fee_cap))
    }

    /// Core payment logic shared by `route_payment` and `route_payments`.
    fn process_single_payment(
        env: &Env,
        sender: &Address,
        recipient: &Address,
        token_address: &Address,
        amount: i128,
        platform_treasury: &Address,
        fee_bps: i128,
        fee_cap: i128,
    ) -> Result<(), Error> {
        // Require sender auth
        sender.require_auth();

        env.events().publish(
            (Symbol::new(env, "payment_initiated"), sender.clone()),
            amount,
        );

        // Prevent self-routing
        if sender == recipient {
            return Err(Error::InvalidRecipient);
        }

        // Check if recipient is blacklisted
        if Self::is_blacklisted(env.clone(), recipient.clone()) {
            return Err(Error::Blacklisted);
        }

        // Validate amount bounds
        let max_amount: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MaxAmount)
            .unwrap_or(Self::MAX_AMOUNT);
        if amount <= 0 || amount > max_amount {
            return Err(Error::LimitExceeded);
        }

        // Apply tiered fee discount for high-volume users
        let user_volume: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::UserVolume(sender.clone()))
            .unwrap_or(0);
        let effective_fee_bps = if user_volume > Self::VOLUME_THRESHOLD {
            fee_bps / 2
        } else {
            fee_bps
        };

        // Check time-based daily spending limits
        let current_time = env.ledger().timestamp();
        let spending_key = DataKey::UserSpending(sender.clone());
        let mut spending: UserSpending = env
            .storage()
            .persistent()
            .get(&spending_key)
            .unwrap_or(UserSpending {
                last_reset_time: current_time,
                accumulated_amount: 0,
            });

        if current_time - spending.last_reset_time >= Self::SECONDS_IN_24H {
            spending.last_reset_time = current_time;
            spending.accumulated_amount = 0;
        }

        spending.accumulated_amount += amount;
        if spending.accumulated_amount > Self::DAILY_MAX_LIMIT {
            return Err(Error::LimitExceeded);
        }

        env.storage().persistent().set(&spending_key, &spending);
        env.storage().persistent().extend_ttl(
            &spending_key,
            Self::PERSISTENT_LIFETIME_THRESHOLD,
            Self::PERSISTENT_BUMP_AMOUNT,
        );

        // Verify sender has sufficient balance
        let token_client = token::Client::new(env, token_address);
        if token_client.balance(sender) < amount {
            return Err(Error::InsufficientBalance);
        }

        // Calculate fee
        let mut fee_amount = (amount * effective_fee_bps) / Self::BPS_DIVISOR;
        if fee_amount > fee_cap {
            fee_amount = fee_cap;
        }
        if fee_amount > amount {
            fee_amount = amount;
        }
        let remainder = amount - fee_amount;

        // Execute transfers
        if fee_amount > 0 {
            token_client.transfer(sender, platform_treasury, &fee_amount);
        }
        if remainder > 0 {
            token_client.transfer(sender, recipient, &remainder);
        }

        // Record cumulative volume
        let volume_key = DataKey::UserVolume(sender.clone());
        let prev_volume: i128 = env.storage().persistent().get(&volume_key).unwrap_or(0);
        env.storage()
            .persistent()
            .set(&volume_key, &(prev_volume + amount));
        env.storage().persistent().extend_ttl(
            &volume_key,
            Self::PERSISTENT_LIFETIME_THRESHOLD,
            Self::PERSISTENT_BUMP_AMOUNT,
        );

        // Emit routed event
        env.events().publish(
            (symbol_short!("routed"), sender.clone(), recipient.clone()),
            amount,
        );

        log!(env, "Platform fee routed to treasury");
        log!(env, "Remaining balance routed to recipient");

        Ok(())
    }

    // ── Public contract methods ──────────────────────────────────────────────

    /// One-time setup: records the admin and the initial fee configuration
    /// in instance storage. Must be called before `route_payment`.
    pub fn initialize(
        env: Env,
        admin: Address,
        platform_treasury: Address,
        fee_bps: i128,
        fee_cap: i128,
        max_amount: i128,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::PlatformTreasury, &platform_treasury);
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage().instance().set(&DataKey::FeeCap, &fee_cap);
        env.storage().instance().set(&DataKey::MaxAmount, &max_amount);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().extend_ttl(
            Self::INSTANCE_LIFETIME_THRESHOLD,
            Self::INSTANCE_BUMP_AMOUNT,
        );

        Ok(())
    }

    /// Updates the treasury address that receives the platform fee. Admin-only.
    pub fn set_platform_treasury(env: Env, new_treasury: Address) -> Result<(), Error> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();

        env.storage()
            .instance()
            .set(&DataKey::PlatformTreasury, &new_treasury);
        env.storage().instance().extend_ttl(
            Self::INSTANCE_LIFETIME_THRESHOLD,
            Self::INSTANCE_BUMP_AMOUNT,
        );
        Ok(())
    }

    /// Updates the fee basis points and fee cap. Admin-only.
    pub fn set_fee_config_legacy(env: Env, fee_bps: i128, fee_cap: i128) -> Result<(), Error> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();

        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage().instance().set(&DataKey::FeeCap, &fee_cap);
        env.storage().instance().extend_ttl(
            Self::INSTANCE_LIFETIME_THRESHOLD,
            Self::INSTANCE_BUMP_AMOUNT,
        );
        Ok(())
    }

    /// Alias for `set_fee_config_legacy`. Admin-only.
    pub fn set_fee_config(env: Env, fee_bps: i128, fee_cap: i128) -> Result<(), Error> {
        Self::set_fee_config_legacy(env, fee_bps, fee_cap)
    }

    /// Updates the fee basis points. Admin-only.
    pub fn set_fee_bps(env: Env, new_fee_bps: i128) -> Result<(), Error> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();

        env.storage().instance().set(&DataKey::FeeBps, &new_fee_bps);
        env.storage().instance().extend_ttl(
            Self::INSTANCE_LIFETIME_THRESHOLD,
            Self::INSTANCE_BUMP_AMOUNT,
        );
        Ok(())
    }

    /// Returns the current protocol fee percentage in basis points.
    pub fn get_fee(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::FeeBps)
            .unwrap_or(0)
    }

    /// Pauses or unpauses the payment router. Admin-only.
    pub fn set_pause(env: Env, paused: bool) -> Result<(), Error> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();

        env.storage().instance().set(&DataKey::Paused, &paused);
        env.storage().instance().extend_ttl(
            Self::INSTANCE_LIFETIME_THRESHOLD,
            Self::INSTANCE_BUMP_AMOUNT,
        );

        env.events().publish((symbol_short!("pause"),), (paused,));

        Ok(())
    }

    /// Alias for `set_pause`. Admin-only.
    pub fn set_paused(env: Env, paused: bool) -> Result<(), Error> {
        Self::set_pause(env, paused)
    }

    /// Returns whether the contract is currently paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Returns the cumulative amount a given sender has routed through the contract.
    pub fn get_user_volume(env: Env, user: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::UserVolume(user))
            .unwrap_or(0)
    }

    /// Adds an address to the blacklist. Admin-only.
    pub fn blacklist_address(env: Env, address: Address) -> Result<(), Error> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();

        env.storage().persistent().set(&DataKey::Blacklist(address.clone()), &true);
        env.storage().persistent().extend_ttl(
            &DataKey::Blacklist(address),
            Self::PERSISTENT_LIFETIME_THRESHOLD,
            Self::PERSISTENT_BUMP_AMOUNT,
        );

        Ok(())
    }

    /// Removes an address from the blacklist. Admin-only.
    pub fn unblacklist_address(env: Env, address: Address) -> Result<(), Error> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();

        env.storage().persistent().remove(&DataKey::Blacklist(address));

        Ok(())
    }

    /// Returns whether an address is blacklisted.
    pub fn is_blacklisted(env: Env, address: Address) -> bool {
        env.storage().persistent().get(&DataKey::Blacklist(address)).unwrap_or(false)
    }

    /// Returns the effective fee_bps for a sender after applying any
    /// volume-based tiered discount.
    pub fn get_effective_fee_bps(env: Env, sender: Address) -> i128 {
        let fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeBps)
            .unwrap_or(0);
        let user_volume = Self::get_user_volume(env.clone(), sender);
        if user_volume > Self::VOLUME_THRESHOLD {
            fee_bps / 2
        } else {
            fee_bps
        }
    }

    /// Set a new admin. Gated by the current admin if one exists.
    pub fn set_admin(env: Env, new_admin: Address) -> Result<(), Error> {
        if let Some(admin) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::Admin)
        {
            admin.require_auth();
        }
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage().instance().extend_ttl(
            Self::INSTANCE_LIFETIME_THRESHOLD,
            Self::INSTANCE_BUMP_AMOUNT,
        );
        Ok(())
    }

    /// Transfers admin rights to a new address. Requires the current admin's authorization.
    pub fn transfer_admin(env: Env, new_admin: Address) -> Result<(), Error> {
        let current_admin = Self::require_admin(&env)?;
        current_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage().instance().extend_ttl(
            Self::INSTANCE_LIFETIME_THRESHOLD,
            Self::INSTANCE_BUMP_AMOUNT,
        );
        Ok(())
    }

    /// Recovers tokens accidentally sent directly to the contract address. Admin-only.
    pub fn recover_tokens(env: Env, token: Address, amount: i128) -> Result<(), Error> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();

        let contract_address = env.current_contract_address();
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&contract_address, &admin, &amount);

        Ok(())
    }

    /// Records a token as supported (no-op; routing accepts any token contract ID).
    pub fn add_supported_token(_env: Env, _token: Address) -> Result<(), Error> {
        Ok(())
    }

    /// Routes a payment from a sender to a recipient, deducting a platform fee.
    pub fn route_payment(
        env: Env,
        sender: Address,
        recipient: Address,
        token_address: Address,
        amount: i128,
    ) -> Result<(), Error> {
        if Self::is_paused(env.clone()) {
            return Err(Error::Paused);
        }

        let (platform_treasury, fee_bps, fee_cap) = Self::load_fee_config(&env)?;

        Self::process_single_payment(
            &env,
            &sender,
            &recipient,
            &token_address,
            amount,
            &platform_treasury,
            fee_bps,
            fee_cap,
        )
    }

    /// Routes multiple payments in a single transaction. If any payment fails,
    /// the entire batch is reverted atomically.
    pub fn route_payments(env: Env, payments: Vec<Payment>) -> Result<(), Error> {
        if Self::is_paused(env.clone()) {
            return Err(Error::Paused);
        }

        let (platform_treasury, fee_bps, fee_cap) = Self::load_fee_config(&env)?;

        for payment in payments.iter() {
            Self::process_single_payment(
                &env,
                &payment.sender,
                &payment.recipient,
                &payment.token_address,
                payment.amount,
                &platform_treasury,
                fee_bps,
                fee_cap,
            )?;
        }

        Ok(())
    }

    /// Admin-only emergency withdrawal of tokens held by this contract.
    pub fn emergency_withdraw(env: Env, token: Address, amount: i128) -> Result<(), Error> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();

        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&env.current_contract_address(), &admin, &amount);

        log!(&env, "Emergency withdraw executed by admin");
        Ok(())
    }

    /// Replaces this contract's WASM with a previously uploaded version. Admin-only.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();

        env.deployer().update_current_contract_wasm(new_wasm_hash);
        Ok(())
    }

    /// Returns the contract version.
    pub fn version(_env: Env) -> u32 {
        Self::VERSION
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Events, Ledger as _, LedgerInfo},
        token::StellarAssetClient,
        Address, Env, Symbol, TryIntoVal,
    };

    /// Returns (env, client, contract_id).
    fn setup_env() -> (Env, PaymentRouterClient<'static>, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, PaymentRouter);
        let client = PaymentRouterClient::new(&env, &contract_id);
        (env, client, contract_id)
    }

    /// Deploys a Stellar Asset Contract test token. Returns
    /// (token_address, token_client, stellar_asset_admin_client).
    fn setup_token(
        env: &Env,
    ) -> (
        Address,
        token::Client<'static>,
        token::StellarAssetClient<'static>,
    ) {
        let token_admin = Address::generate(env);
        let token_address = env.register_stellar_asset_contract(token_admin);
        let token_client = token::Client::new(env, &token_address);
        let token_admin_client = token::StellarAssetClient::new(env, &token_address);
        (token_address, token_client, token_admin_client)
    }

    #[test]
    fn test_get_fee() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);

        // Before initialization, get_fee returns 0
        assert_eq!(client.get_fee(), 0);

        // Initialize with 150 bps
        client.initialize(&admin, &treasury, &150, &5000, &PaymentRouter::MAX_AMOUNT);
        assert_eq!(client.get_fee(), 150);

        // Update via set_fee_bps
        client.set_fee_bps(&250);
        assert_eq!(client.get_fee(), 250);

        // Update via set_fee_config
        client.set_fee_config(&300, &10000);
        assert_eq!(client.get_fee(), 300);
    }

    #[test]
    fn test_admin_restrictions_and_updates() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let new_admin = Address::generate(&env);

        client.initialize(&admin, &treasury, &100, &1000, &PaymentRouter::MAX_AMOUNT);

        // Trying to initialize again should fail
        let res = client.try_initialize(&admin, &treasury, &100, &1000, &PaymentRouter::MAX_AMOUNT);
        assert_eq!(res.unwrap_err().unwrap(), Error::AlreadyInitialized);

        client.set_admin(&new_admin);

        // Modify config
        client.set_fee_config(&200, &2000);
        client.set_fee_bps(&200);
        assert_eq!(client.get_fee(), 200);

        let new_treasury = Address::generate(&env);
        client.set_platform_treasury(&new_treasury);
    }

    #[test]
    fn test_recover_tokens() {
        let (env, client, contract_id) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);

        client.initialize(&admin, &treasury, &100, &1000, &PaymentRouter::MAX_AMOUNT);

        let (token_address, token_client, stellar_asset_client) = setup_token(&env);

        // Simulate tokens accidentally sent directly to the contract address
        let accidental_amount = 5_000i128;
        stellar_asset_client.mint(&contract_id, &accidental_amount);

        assert_eq!(token_client.balance(&contract_id), accidental_amount);
        assert_eq!(token_client.balance(&admin), 0);

        // Admin recovers tokens
        let recover_amount = 3_000i128;
        client.recover_tokens(&token_address, &recover_amount);

        assert_eq!(token_client.balance(&admin), recover_amount);
        assert_eq!(
            token_client.balance(&contract_id),
            accidental_amount - recover_amount
        );
    }

    #[test]
    fn test_set_pause_emits_event() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);

        client.initialize(&admin, &treasury, &100, &1000, &PaymentRouter::MAX_AMOUNT);

        client.set_pause(&true);

        let events = env.events().all();
        assert!(!events.is_empty());
        let (_, topics, _) = events.get(0).unwrap();
        assert_eq!(topics.len(), 1);
        let topic: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(topic, symbol_short!("pause"));
    }

    #[test]
    fn test_route_payment_emits_payment_initiated_event() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let (token_address, _token_client, sac) = setup_token(&env);
        sac.mint(&sender, &10_000);

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);

        client.mock_all_auths().route_payment(&sender, &recipient, &token_address, &5_000);

        let events = env.events().all();
        assert!(!events.is_empty());
        
        let mut found = false;
        for (_, topics, data) in events.iter() {
            if topics.len() > 0 {
                if let Ok(topic_sym) = topics.get(0).unwrap().try_into_val(&env) {
                    let sym: Symbol = topic_sym;
                    if sym == Symbol::new(&env, "payment_initiated") {
                        found = true;
                        let amt: i128 = data.try_into_val(&env).unwrap();
                        assert_eq!(amt, 5_000);
                        break;
                    }
                }
            }
        }
        assert!(found, "payment_initiated event not found");
    }

    #[test]
    fn test_route_payment_emits_routed_event() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let (token_address, _token_client, _token_admin_client) = setup_token(&env);
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token_address);
        sac.mint(&sender, &10_000);

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);
        client.add_supported_token(&token_address);

        let amount = 2_000i128;
        client.route_payment(&sender, &recipient, &token_address, &amount);

        let events = env.events().all();
        assert!(!events.is_empty());

        // Find the "routed" event by topic
        let mut found = None;
        for evt in events.iter() {
            let (_contract_id, topics, _data) = evt.clone();
            if topics.len() != 3 {
                continue;
            }
            let topic0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
            if topic0 == symbol_short!("routed") {
                found = Some(evt.clone());
                break;
            }
        }
        let routed = found.expect("route_payment should publish a \"routed\" event");

        let (_contract_id, topics, data) = routed;
        assert_eq!(topics.len(), 3);

        let topic_sender: Address = topics.get(1).unwrap().try_into_val(&env).unwrap();
        let topic_recipient: Address = topics.get(2).unwrap().try_into_val(&env).unwrap();
        assert_eq!(topic_sender, sender);
        assert_eq!(topic_recipient, recipient);

        let event_amount: i128 = data.try_into_val(&env).unwrap();
        assert_eq!(event_amount, amount);
    }

    #[test]
    fn test_admin_pause_functionality() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let (token_address, _token_client, _token_admin_client) = setup_token(&env);
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token_address);
        sac.mint(&sender, &10_000);

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);

        // Initially not paused
        assert_eq!(client.is_paused(), false);

        // Pause
        client.set_pause(&true);
        assert_eq!(client.is_paused(), true);

        // Route payment should fail when paused
        let res = client.try_route_payment(&sender, &recipient, &token_address, &1000);
        assert_eq!(res.unwrap_err().unwrap(), Error::Paused);

        // Unpause via set_paused alias
        client.set_paused(&false);
        assert_eq!(client.is_paused(), false);

        // Route payment should succeed now
        client.route_payment(&sender, &recipient, &token_address, &1000);
    }

    #[test]
    fn test_route_payment_calculates_and_sends_fee() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let (token_address, token_client, _token_admin_client) = setup_token(&env);

        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token_address);
        let initial_balance = 10_000i128;
        sac.mint(&sender, &initial_balance);

        // Initialize router with 1% fee (100 bps) and cap of 50
        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);
        client.add_supported_token(&token_address);

        // Test normal fee calculation: 1% of 2000 = 20, below cap of 50
        let amount_1 = 2000i128;
        client.route_payment(&sender, &recipient, &token_address, &amount_1);

        assert_eq!(token_client.balance(&treasury), 20);
        assert_eq!(token_client.balance(&recipient), 1980);
        assert_eq!(token_client.balance(&sender), initial_balance - amount_1);
        assert_eq!(client.get_user_volume(&sender), amount_1);

        // Test fee capped at 50: 1% of 8000 = 80, capped to 50
        let amount_2 = 8000i128;
        client.route_payment(&sender, &recipient, &token_address, &amount_2);

        assert_eq!(token_client.balance(&treasury), 70);
        assert_eq!(token_client.balance(&recipient), 9930);
        assert_eq!(
            token_client.balance(&sender),
            initial_balance - amount_1 - amount_2
        );
        assert_eq!(client.get_user_volume(&sender), amount_1 + amount_2);
    }

    #[test]
    fn test_insufficient_balance() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let (token_address, _token_client, sac) = setup_token(&env);
        sac.mint(&sender, &100);

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);
        client.add_supported_token(&token_address);

        // Route payment of 500 when balance is only 100
        let res = client.try_route_payment(&sender, &recipient, &token_address, &500);
        assert_eq!(res.unwrap_err().unwrap(), Error::InsufficientBalance);
    }

    #[test]
    fn test_daily_limit_and_reset() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let (token_address, token_client, _token_admin_client) = setup_token(&env);

        let limit = 10_000_000_000_000i128;
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token_address);
        sac.mint(&sender, &(limit + 2000));

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);
        client.add_supported_token(&token_address);

        // Route amount up to daily limit
        client.route_payment(&sender, &recipient, &token_address, &limit);

        // Next payment should exceed daily limit
        let res = client.try_route_payment(&sender, &recipient, &token_address, &2000);
        assert_eq!(res.unwrap_err().unwrap(), Error::LimitExceeded);

        // Advance time past 24 hours to reset the daily limit
        let current_time = env.ledger().timestamp();
        let current_protocol_version = env.ledger().protocol_version();
        env.ledger().set(LedgerInfo {
            timestamp: current_time + 86400,
            protocol_version: current_protocol_version,
            sequence_number: 1,
            network_id: env.ledger().network_id().into(),
            base_reserve: 100,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6312000,
        });

        // Now routing should succeed again. The first payment pushed volume past
        // VOLUME_THRESHOLD, so the halved rate applies: 2000 * 50 bps = 10.
        client.route_payment(&sender, &recipient, &token_address, &2000);
        assert_eq!(
            token_client.balance(&recipient),
            (limit - 50) + (2000 - 10)
        );
    }

    #[test]
    fn test_prevent_self_routing() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);

        let (token_address, _token_client, _token_admin_client) = setup_token(&env);
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token_address);
        sac.mint(&sender, &10_000);

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);

        let res = client.try_route_payment(&sender, &sender, &token_address, &1000);
        assert_eq!(res.unwrap_err().unwrap(), Error::InvalidRecipient);
    }

    #[test]
    fn test_tiered_fee_discount_applied_after_volume_threshold() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let (token_address, token_client, _token_admin_client) = setup_token(&env);

        // Threshold is 10,000 XLM = 10,000 * 10,000,000 (7 decimals)
        let threshold = 100_000_000_000i128;
        let first_amount = threshold + 1;
        let second_amount = 1000i128;
        let total_mint = first_amount + second_amount + 10_000_000;
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token_address);
        sac.mint(&sender, &total_mint);

        // Initialize with 1% fee (100 bps) and no cap
        client.initialize(&admin, &treasury, &100, &i128::MAX, &PaymentRouter::MAX_AMOUNT);

        // First payment: volume is 0 (< threshold), full fee applies
        client.route_payment(&sender, &recipient, &token_address, &first_amount);

        let full_fee_first = (first_amount * 100) / 10_000;
        assert_eq!(token_client.balance(&treasury), full_fee_first);
        assert_eq!(token_client.balance(&recipient), first_amount - full_fee_first);
        assert_eq!(client.get_user_volume(&sender), first_amount);
        // Volume is now past threshold, so next call gets the discount
        assert_eq!(client.get_effective_fee_bps(&sender), 50);

        // Second payment: volume > threshold, 50% discount applies
        client.route_payment(&sender, &recipient, &token_address, &second_amount);

        let discounted_fee = (second_amount * 50) / 10_000;
        assert_eq!(token_client.balance(&treasury), full_fee_first + discounted_fee);
        assert_eq!(
            token_client.balance(&recipient),
            (first_amount - full_fee_first) + (second_amount - discounted_fee)
        );
    }

    #[test]
    fn test_get_effective_fee_bps_no_discount_below_threshold() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let (token_address, _token_client, _token_admin_client) = setup_token(&env);
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token_address);
        sac.mint(&sender, &1_000_000);

        client.initialize(&admin, &treasury, &100, &i128::MAX, &PaymentRouter::MAX_AMOUNT);

        // No volume yet
        assert_eq!(client.get_effective_fee_bps(&sender), 100);

        // Route a small payment (below threshold)
        client.route_payment(&sender, &recipient, &token_address, &1000);

        // Volume is 1000, far below 10,000 XLM threshold
        assert_eq!(client.get_effective_fee_bps(&sender), 100);
    }

    #[test]
    fn test_successful_xlm_routing() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let platform_treasury = Address::generate(&env);

        let contract_id = env.register_contract(None, PaymentRouter);
        let client = PaymentRouterClient::new(&env, &contract_id);

        client.initialize(&admin, &platform_treasury, &40, &i128::MAX, &PaymentRouter::MAX_AMOUNT);

        let token_admin = Address::generate(&env);
        let token_address = env.register_stellar_asset_contract(token_admin.clone());
        let sac = StellarAssetClient::new(&env, &token_address);
        let token_client = token::Client::new(&env, &token_address);

        let initial_balance = 1_000_000_000i128;
        sac.mint(&sender, &initial_balance);

        client.add_supported_token(&token_address);

        let amount = 100_000_000i128;
        client.route_payment(&sender, &recipient, &token_address, &amount);

        let expected_fee = 400_000i128;
        let expected_recipient_amount = amount - expected_fee;

        assert_eq!(token_client.balance(&sender), initial_balance - amount);
        assert_eq!(token_client.balance(&recipient), expected_recipient_amount);
        assert_eq!(token_client.balance(&platform_treasury), expected_fee);
    }

    #[test]
    fn test_initialize_sets_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let contract_addr = env.register_contract(None, PaymentRouter);
        let client = PaymentRouterClient::new(&env, &contract_addr);

        client.initialize(&admin, &treasury, &100, &1000, &PaymentRouter::MAX_AMOUNT);

        let stored_admin: Option<Address> = env.as_contract(&contract_addr, || {
            env.storage().instance().get(&DataKey::Admin)
        });
        assert_eq!(stored_admin, Some(admin));
    }

    #[test]
    fn test_emergency_withdraw_stores_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let contract_addr = env.register_contract(None, PaymentRouter);
        let client = PaymentRouterClient::new(&env, &contract_addr);

        client.initialize(&admin, &treasury, &100, &1000, &PaymentRouter::MAX_AMOUNT);

        let stored_admin: Option<Address> = env.as_contract(&contract_addr, || {
            env.storage().instance().get(&DataKey::Admin)
        });
        assert_eq!(stored_admin.clone(), Some(admin.clone()));
        assert_eq!(stored_admin.unwrap(), admin);
    }

    #[test]
    fn test_blacklist_recipient() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let (token_address, _token_client, sac) = setup_token(&env);
        sac.mint(&sender, &10_000);

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);

        // Blacklist the recipient
        client.blacklist_address(&recipient);
        assert!(client.is_blacklisted(&recipient));

        // Route payment should fail
        let res = client.try_route_payment(&sender, &recipient, &token_address, &1000);
        assert_eq!(res.unwrap_err().unwrap(), Error::Blacklisted);

        // Unblacklist and try again
        client.unblacklist_address(&recipient);
        assert!(!client.is_blacklisted(&recipient));

        client.mock_all_auths().route_payment(&sender, &recipient, &token_address, &1000);
    }

    #[test]
    fn test_routes_multiple_distinct_assets() {
        let (env, client, _) = setup_env();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        client.initialize(&admin, &treasury, &100, &1_000_000, &PaymentRouter::MAX_AMOUNT);

        let (usdc_like_address, usdc_like_client, usdc_like_admin_client) = setup_token(&env);
        let (eurc_like_address, eurc_like_client, eurc_like_admin_client) = setup_token(&env);
        assert_ne!(usdc_like_address, eurc_like_address);

        usdc_like_admin_client.mint(&sender, &10_000);
        eurc_like_admin_client.mint(&sender, &5_000);

        client.route_payment(&sender, &recipient, &usdc_like_address, &2_000);
        client.route_payment(&sender, &recipient, &eurc_like_address, &1_000);

        assert_eq!(usdc_like_client.balance(&sender), 8_000);
        assert_eq!(usdc_like_client.balance(&recipient), 1_980);
        assert_eq!(eurc_like_client.balance(&sender), 4_000);
        assert_eq!(eurc_like_client.balance(&recipient), 990);
        assert_eq!(client.get_user_volume(&sender), 3_000);
    }

    // ── New edge-case tests added for #522 ──────────────────────────────────

    /// `amount = 0` must be rejected with `LimitExceeded` (amount <= 0 guard).
    #[test]
    fn test_zero_amount_rejected() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let (token_address, _tc, sac) = setup_token(&env);
        sac.mint(&sender, &10_000);

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);

        let res = client.try_route_payment(&sender, &recipient, &token_address, &0);
        assert_eq!(res.unwrap_err().unwrap(), Error::LimitExceeded);
    }

    /// A negative amount must also be rejected with `LimitExceeded`.
    #[test]
    fn test_negative_amount_rejected() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let (token_address, _tc, sac) = setup_token(&env);
        sac.mint(&sender, &10_000);

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);

        let res = client.try_route_payment(&sender, &recipient, &token_address, &-1);
        assert_eq!(res.unwrap_err().unwrap(), Error::LimitExceeded);
    }

    /// An amount exceeding max_amount must be rejected with `LimitExceeded`.
    #[test]
    fn test_amount_exceeds_max_amount_rejected() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let (token_address, _tc, sac) = setup_token(&env);

        let max_amount = 5_000i128;
        client.initialize(&admin, &treasury, &100, &50, &max_amount);

        sac.mint(&sender, &100_000);

        // Exactly at max_amount — should succeed
        client.route_payment(&sender, &recipient, &token_address, &max_amount);

        // One over — should fail
        let res = client.try_route_payment(&sender, &recipient, &token_address, &(max_amount + 1));
        assert_eq!(res.unwrap_err().unwrap(), Error::LimitExceeded);
    }

    /// `route_payment` before `initialize` returns `NotInitialized`.
    #[test]
    fn test_route_payment_not_initialized() {
        let (env, client, _) = setup_env();
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let (token_address, _tc, sac) = setup_token(&env);
        sac.mint(&sender, &10_000);

        let res = client.try_route_payment(&sender, &recipient, &token_address, &1_000);
        assert_eq!(res.unwrap_err().unwrap(), Error::NotInitialized);
    }

    /// `route_payments` (batch) — happy path: all payments succeed and balances
    /// are updated correctly.
    #[test]
    fn test_route_payments_batch_success() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient1 = Address::generate(&env);
        let recipient2 = Address::generate(&env);
        let (token_address, token_client, sac) = setup_token(&env);
        sac.mint(&sender, &10_000);

        // fee = 100 bps, cap = i128::MAX (no cap)
        client.initialize(&admin, &treasury, &100, &i128::MAX, &PaymentRouter::MAX_AMOUNT);

        let payments = vec![
            &env,
            Payment {
                sender: sender.clone(),
                recipient: recipient1.clone(),
                token_address: token_address.clone(),
                amount: 1_000,
            },
            Payment {
                sender: sender.clone(),
                recipient: recipient2.clone(),
                token_address: token_address.clone(),
                amount: 2_000,
            },
        ];

        client.route_payments(&payments);

        // fee for 1_000 @ 100 bps = 10; recipient1 gets 990
        // fee for 2_000 @ 100 bps = 20; recipient2 gets 1980
        assert_eq!(token_client.balance(&recipient1), 990);
        assert_eq!(token_client.balance(&recipient2), 1_980);
        assert_eq!(token_client.balance(&treasury), 30);
        assert_eq!(token_client.balance(&sender), 10_000 - 3_000);
        assert_eq!(client.get_user_volume(&sender), 3_000);
    }

    /// `route_payments` — if any payment in the batch fails, the whole call
    /// returns an error (atomic batch).
    #[test]
    fn test_route_payments_batch_fails_on_bad_payment() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let (token_address, _tc, sac) = setup_token(&env);
        sac.mint(&sender, &10_000);

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);

        // Second payment uses sender == recipient (self-routing) which is invalid
        let payments = vec![
            &env,
            Payment {
                sender: sender.clone(),
                recipient: recipient.clone(),
                token_address: token_address.clone(),
                amount: 1_000,
            },
            Payment {
                sender: sender.clone(),
                recipient: sender.clone(), // invalid — self-routing
                token_address: token_address.clone(),
                amount: 500,
            },
        ];

        let res = client.try_route_payments(&payments);
        assert_eq!(res.unwrap_err().unwrap(), Error::InvalidRecipient);
    }

    /// `route_payments` — rejects when contract is paused.
    #[test]
    fn test_route_payments_paused() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let (token_address, _tc, sac) = setup_token(&env);
        sac.mint(&sender, &10_000);

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);
        client.set_pause(&true);

        let payments = vec![
            &env,
            Payment {
                sender: sender.clone(),
                recipient: recipient.clone(),
                token_address: token_address.clone(),
                amount: 1_000,
            },
        ];

        let res = client.try_route_payments(&payments);
        assert_eq!(res.unwrap_err().unwrap(), Error::Paused);
    }

    /// `route_payments` — a blacklisted recipient in the batch is rejected.
    #[test]
    fn test_route_payments_blacklisted_recipient() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let bad_recipient = Address::generate(&env);
        let (token_address, _tc, sac) = setup_token(&env);
        sac.mint(&sender, &10_000);

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);
        client.blacklist_address(&bad_recipient);

        let payments = vec![
            &env,
            Payment {
                sender: sender.clone(),
                recipient: bad_recipient.clone(),
                token_address: token_address.clone(),
                amount: 1_000,
            },
        ];

        let res = client.try_route_payments(&payments);
        assert_eq!(res.unwrap_err().unwrap(), Error::Blacklisted);
    }

    /// `transfer_admin` correctly moves admin rights to a new address.
    #[test]
    fn test_transfer_admin() {
        let (env, client, contract_addr) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let new_admin = Address::generate(&env);

        client.initialize(&admin, &treasury, &100, &1_000, &PaymentRouter::MAX_AMOUNT);
        client.transfer_admin(&new_admin);

        let stored: Option<Address> = env.as_contract(&contract_addr, || {
            env.storage().instance().get(&DataKey::Admin)
        });
        assert_eq!(stored, Some(new_admin));
    }

    /// `transfer_admin` before `initialize` returns `NotInitialized`.
    #[test]
    fn test_transfer_admin_not_initialized() {
        let (env, client, _) = setup_env();
        let new_admin = Address::generate(&env);

        let res = client.try_transfer_admin(&new_admin);
        assert_eq!(res.unwrap_err().unwrap(), Error::NotInitialized);
    }

    /// `emergency_withdraw` correctly transfers tokens from the contract to admin.
    #[test]
    fn test_emergency_withdraw() {
        let (env, client, contract_addr) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);

        client.initialize(&admin, &treasury, &100, &1_000, &PaymentRouter::MAX_AMOUNT);

        let (token_address, token_client, sac) = setup_token(&env);
        let deposited = 8_000i128;
        sac.mint(&contract_addr, &deposited);

        assert_eq!(token_client.balance(&contract_addr), deposited);

        let withdraw = 3_000i128;
        client.emergency_withdraw(&token_address, &withdraw);

        assert_eq!(token_client.balance(&admin), withdraw);
        assert_eq!(token_client.balance(&contract_addr), deposited - withdraw);
    }

    /// `version()` returns the compile-time constant `1`.
    #[test]
    fn test_version() {
        let (_env, client, _) = setup_env();
        assert_eq!(client.version(), 1);
    }

    /// `add_supported_token` is a no-op and never errors.
    #[test]
    fn test_add_supported_token_noop() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let (token_address, _tc, _sac) = setup_token(&env);

        client.initialize(&admin, &treasury, &100, &1_000, &PaymentRouter::MAX_AMOUNT);
        // Should not panic or error
        client.add_supported_token(&token_address);
    }

    /// `set_fee_config_legacy` updates both fee_bps and fee_cap.
    #[test]
    fn test_set_fee_config_legacy() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let (token_address, token_client, sac) = setup_token(&env);
        sac.mint(&sender, &10_000);

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);

        // Update to 200 bps with a higher cap
        client.set_fee_config_legacy(&200, &500);
        assert_eq!(client.get_fee(), 200);

        // Route and verify new fee applies: 200 bps of 1_000 = 20
        client.route_payment(&sender, &recipient, &token_address, &1_000);
        assert_eq!(token_client.balance(&treasury), 20);
        assert_eq!(token_client.balance(&recipient), 980);
    }

    /// `get_effective_fee_bps` returns 0 when the contract is not initialized.
    #[test]
    fn test_get_effective_fee_bps_uninitialized() {
        let (env, client, _) = setup_env();
        let sender = Address::generate(&env);
        // No storage entry for FeeBps — should return 0
        assert_eq!(client.get_effective_fee_bps(&sender), 0);
    }

    /// `get_user_volume` returns 0 for a user who has never sent a payment.
    #[test]
    fn test_get_user_volume_no_history() {
        let (env, client, _) = setup_env();
        let user = Address::generate(&env);
        assert_eq!(client.get_user_volume(&user), 0);
    }

    /// Fee is capped at the payment amount when fee_cap is larger than amount.
    /// With fee_bps = 10_000 (100%) the fee equals the full amount, so
    /// the remainder = 0 and only the fee transfer is executed.
    #[test]
    fn test_fee_capped_at_amount() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let (token_address, token_client, sac) = setup_token(&env);
        sac.mint(&sender, &1_000);

        // 100% fee, cap far above amount
        client.initialize(&admin, &treasury, &10_000, &i128::MAX, &PaymentRouter::MAX_AMOUNT);

        client.route_payment(&sender, &recipient, &token_address, &1_000);

        // All goes to treasury; recipient gets nothing
        assert_eq!(token_client.balance(&treasury), 1_000);
        assert_eq!(token_client.balance(&recipient), 0);
    }

    // ── Additional coverage tests for #522 ──────────────────────────────────

    /// `set_platform_treasury` updates the stored treasury and subsequent
    /// payments route fees to the new address.
    #[test]
    fn test_set_platform_treasury_updates_fee_destination() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let old_treasury = Address::generate(&env);
        let new_treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let (token_address, token_client, sac) = setup_token(&env);
        sac.mint(&sender, &10_000);

        // 1% fee, cap 50
        client.initialize(&admin, &old_treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);

        // Route once — fee goes to old_treasury
        client.route_payment(&sender, &recipient, &token_address, &1_000);
        assert_eq!(token_client.balance(&old_treasury), 10);
        assert_eq!(token_client.balance(&new_treasury), 0);

        // Swap treasury
        client.set_platform_treasury(&new_treasury);

        // Route again — fee now goes to new_treasury
        client.route_payment(&sender, &recipient, &token_address, &1_000);
        assert_eq!(token_client.balance(&old_treasury), 10);  // unchanged
        assert_eq!(token_client.balance(&new_treasury), 10);  // received fee
    }

    /// `set_platform_treasury` before `initialize` returns `NotInitialized`.
    #[test]
    fn test_set_platform_treasury_not_initialized() {
        let (env, client, _) = setup_env();
        let new_treasury = Address::generate(&env);

        let res = client.try_set_platform_treasury(&new_treasury);
        assert_eq!(res.unwrap_err().unwrap(), Error::NotInitialized);
    }

    /// `set_fee_bps` before `initialize` returns `NotInitialized`.
    #[test]
    fn test_set_fee_bps_not_initialized() {
        let (env, client, _) = setup_env();
        let res = client.try_set_fee_bps(&200);
        assert_eq!(res.unwrap_err().unwrap(), Error::NotInitialized);
    }

    /// `set_fee_config_legacy` before `initialize` returns `NotInitialized`.
    #[test]
    fn test_set_fee_config_legacy_not_initialized() {
        let (env, client, _) = setup_env();
        let res = client.try_set_fee_config_legacy(&200, &500);
        assert_eq!(res.unwrap_err().unwrap(), Error::NotInitialized);
    }

    /// `set_pause` before `initialize` returns `NotInitialized`.
    #[test]
    fn test_set_pause_not_initialized() {
        let (env, client, _) = setup_env();
        let res = client.try_set_pause(&true);
        assert_eq!(res.unwrap_err().unwrap(), Error::NotInitialized);
    }

    /// `blacklist_address` before `initialize` returns `NotInitialized`.
    #[test]
    fn test_blacklist_not_initialized() {
        let (env, client, _) = setup_env();
        let addr = Address::generate(&env);
        let res = client.try_blacklist_address(&addr);
        assert_eq!(res.unwrap_err().unwrap(), Error::NotInitialized);
    }

    /// `unblacklist_address` before `initialize` returns `NotInitialized`.
    #[test]
    fn test_unblacklist_not_initialized() {
        let (env, client, _) = setup_env();
        let addr = Address::generate(&env);
        let res = client.try_unblacklist_address(&addr);
        assert_eq!(res.unwrap_err().unwrap(), Error::NotInitialized);
    }

    /// `recover_tokens` before `initialize` returns `NotInitialized`.
    #[test]
    fn test_recover_tokens_not_initialized() {
        let (env, client, _) = setup_env();
        let (token_address, _tc, _sac) = setup_token(&env);
        let res = client.try_recover_tokens(&token_address, &100);
        assert_eq!(res.unwrap_err().unwrap(), Error::NotInitialized);
    }

    /// `emergency_withdraw` before `initialize` returns `NotInitialized`.
    #[test]
    fn test_emergency_withdraw_not_initialized() {
        let (env, client, _) = setup_env();
        let (token_address, _tc, _sac) = setup_token(&env);
        let res = client.try_emergency_withdraw(&token_address, &100);
        assert_eq!(res.unwrap_err().unwrap(), Error::NotInitialized);
    }

    /// `is_paused` returns `false` on a freshly initialized contract.
    #[test]
    fn test_is_paused_initially_false() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);
        assert_eq!(client.is_paused(), false);
    }

    /// `is_paused` returns `false` before `initialize` (storage defaults to false).
    #[test]
    fn test_is_paused_before_initialize() {
        let (_env, client, _) = setup_env();
        assert_eq!(client.is_paused(), false);
    }

    /// `set_admin` on an uninitialised contract sets the admin without
    /// requiring existing-admin auth (no admin exists yet).
    #[test]
    fn test_set_admin_when_uninitialized() {
        let (env, client, contract_addr) = setup_env();
        let new_admin = Address::generate(&env);

        // No existing admin — set_admin should succeed
        client.set_admin(&new_admin);

        let stored: Option<Address> = env.as_contract(&contract_addr, || {
            env.storage().instance().get(&DataKey::Admin)
        });
        assert_eq!(stored, Some(new_admin));
    }

    /// `set_fee_config` (alias) delegates to `set_fee_config_legacy` and the
    /// updated fee is reflected by `get_fee`.
    #[test]
    fn test_set_fee_config_alias() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);

        client.initialize(&admin, &treasury, &100, &500, &PaymentRouter::MAX_AMOUNT);
        client.set_fee_config(&250, &1_000);
        assert_eq!(client.get_fee(), 250);
    }

    /// `get_fee` returns 0 before `initialize` (no FeeBps in storage).
    #[test]
    fn test_get_fee_before_initialize() {
        let (_env, client, _) = setup_env();
        assert_eq!(client.get_fee(), 0);
    }

    /// `route_payments` with an empty batch succeeds without error.
    #[test]
    fn test_route_payments_empty_batch() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);

        let payments: soroban_sdk::Vec<Payment> = soroban_sdk::vec![&env];
        client.route_payments(&payments);
    }

    /// Zero-amount payment via `route_payments` batch is rejected.
    #[test]
    fn test_route_payments_zero_amount_rejected() {
        let (env, client, _) = setup_env();
        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let (token_address, _tc, sac) = setup_token(&env);
        sac.mint(&sender, &10_000);

        client.initialize(&admin, &treasury, &100, &50, &PaymentRouter::MAX_AMOUNT);

        let payments = soroban_sdk::vec![
            &env,
            Payment {
                sender: sender.clone(),
                recipient: recipient.clone(),
                token_address: token_address.clone(),
                amount: 0,
            },
        ];
        let res = client.try_route_payments(&payments);
        assert_eq!(res.unwrap_err().unwrap(), Error::LimitExceeded);
    }

    /// `route_payments` before `initialize` returns `NotInitialized`.
    #[test]
    fn test_route_payments_not_initialized() {
        let (env, client, _) = setup_env();
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let (token_address, _tc, sac) = setup_token(&env);
        sac.mint(&sender, &10_000);

        let payments = soroban_sdk::vec![
            &env,
            Payment {
                sender: sender.clone(),
                recipient: recipient.clone(),
                token_address: token_address.clone(),
                amount: 1_000,
            },
        ];
        let res = client.try_route_payments(&payments);
        assert_eq!(res.unwrap_err().unwrap(), Error::NotInitialized);
    }

    /// `add_supported_token` before `initialize` still returns `Ok(())` — it is
    /// a no-op that does not read admin storage.
    #[test]
    fn test_add_supported_token_no_init_needed() {
        let (env, client, _) = setup_env();
        let (token_address, _tc, _sac) = setup_token(&env);
        // Should not panic — add_supported_token is always Ok
        client.add_supported_token(&token_address);
    }
}
