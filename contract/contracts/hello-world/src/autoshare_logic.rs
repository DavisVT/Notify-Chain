use crate::base::errors::Error;
use crate::base::events::{
    AdminTransferred, AutoshareCreated, AutoshareUpdated, ContractPaused, ContractUnpaused,
    GroupActivated, GroupDeactivated, Withdrawal,
};
use crate::base::types::{AutoShareDetails, GroupMember, PaymentHistory};
use soroban_sdk::{contracttype, token, Address, BytesN, Env, String, Vec};

/// Storage key layout (optimized):
///
/// # Instance storage  (cheap reads, evicted together with the contract instance)
/// - `Admin`           – single admin address, read on every privileged call
/// - `SupportedTokens` – token allow-list, read on every create/topup
/// - `UsageFee`        – single u32 fee, read on every create/topup
/// - `IsPaused`        – bool flag, read on every mutating call
///
/// # Persistent storage (survives TTL renewal, per-entry cost)
/// - `AutoShare(id)`          – full group details incl. members
/// - `AllGroups`              – ordered list of all group IDs
/// - `UserPaymentHistory(addr)` – per-user payment records
/// - `GroupPaymentHistory(id)`  – per-group payment records
///
/// # Removed (was duplicate / wasted storage)
/// - `GroupMembers(id)` – members are embedded in `AutoShareDetails.members`
///   and were being written twice on every mutation.  Reads now go directly
///   to `AutoShareDetails`, halving storage writes for member operations.
#[contracttype]
pub enum DataKey {
    AutoShare(BytesN<32>),
    AllGroups,
    UserPaymentHistory(Address),
    GroupPaymentHistory(BytesN<32>),
}

// ============================================================================
// Instance-storage helpers for hot config data
// (instance storage costs less per-read than persistent and shares TTL with
//  the contract instance, making it ideal for values accessed on every call)
// ============================================================================

const INSTANCE_ADMIN: &str = "Admin";
const INSTANCE_PAUSED: &str = "IsPaused";
const INSTANCE_FEE: &str = "UsageFee";
const INSTANCE_TOKENS: &str = "SuppTkns";

pub fn create_autoshare(
    env: Env,
    id: BytesN<32>,
    name: String,
    creator: Address,
    usage_count: u32,
    payment_token: Address,
) -> Result<(), Error> {
    creator.require_auth();

    // Check if contract is paused
    if get_paused_status(&env) {
        return Err(Error::ContractPaused);
    }

    let key = DataKey::AutoShare(id.clone());

    // Check if it already exists to prevent overwriting
    if env.storage().persistent().has(&key) {
        return Err(Error::AlreadyExists);
    }

    // Validate usage count
    if usage_count == 0 {
        return Err(Error::InvalidUsageCount);
    }

    // Verify token is supported
    if !is_token_supported(env.clone(), payment_token.clone()) {
        return Err(Error::UnsupportedToken);
    }

    // Calculate total cost
    let usage_fee = get_usage_fee(env.clone());
    let total_cost = (usage_count as i128) * (usage_fee as i128);

    // Transfer tokens from creator to contract
    let token_client = token::Client::new(&env, &payment_token);
    token_client.transfer(&creator, env.current_contract_address(), &total_cost);

    let details = AutoShareDetails {
        id: id.clone(),
        name,
        creator: creator.clone(),
        usage_count,
        total_usages_paid: usage_count,
        members: Vec::new(&env),
        is_active: true,
    };

    // Store the details in persistent storage (members are embedded inside details,
    // no separate GroupMembers entry needed – saves one persistent write per creation)
    env.storage().persistent().set(&key, &details);

    // Add to all groups list
    let all_groups_key = DataKey::AllGroups;
    let mut all_groups: Vec<BytesN<32>> = env
        .storage()
        .persistent()
        .get(&all_groups_key)
        .unwrap_or(Vec::new(&env));
    all_groups.push_back(id.clone());
    env.storage().persistent().set(&all_groups_key, &all_groups);

    // Record payment history
    record_payment(
        env.clone(),
        creator.clone(),
        id.clone(),
        usage_count,
        total_cost,
    );

    AutoshareCreated {
        creator: creator.clone(),
        id: id.clone(),
    }
    .publish(&env);
    Ok(())
}

pub fn get_autoshare(env: Env, id: BytesN<32>) -> Result<AutoShareDetails, Error> {
    let key = DataKey::AutoShare(id);
    env.storage().persistent().get(&key).ok_or(Error::NotFound)
}

pub fn get_all_groups(env: Env) -> Vec<AutoShareDetails> {
    let all_groups_key = DataKey::AllGroups;
    let group_ids: Vec<BytesN<32>> = env
        .storage()
        .persistent()
        .get(&all_groups_key)
        .unwrap_or(Vec::new(&env));

    let mut result: Vec<AutoShareDetails> = Vec::new(&env);
    for id in group_ids.iter() {
        if let Ok(details) = get_autoshare(env.clone(), id) {
            result.push_back(details);
        }
    }
    result
}

pub fn get_groups_by_creator(env: Env, creator: Address) -> Vec<AutoShareDetails> {
    let all_groups = get_all_groups(env.clone());
    let mut result: Vec<AutoShareDetails> = Vec::new(&env);

    for group in all_groups.iter() {
        if group.creator == creator {
            result.push_back(group);
        }
    }
    result
}

pub fn is_group_member(env: Env, id: BytesN<32>, address: Address) -> Result<bool, Error> {
    // Load the group (also validates it exists)
    let details = get_autoshare(env, id)?;
    for member in details.members.iter() {
        if member.address == address {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn get_group_members(env: Env, id: BytesN<32>) -> Result<Vec<GroupMember>, Error> {
    let details = get_autoshare(env, id)?;
    Ok(details.members)
}

pub fn add_group_member(
    env: Env,
    id: BytesN<32>,
    address: Address,
    percentage: u32,
) -> Result<(), Error> {
    // Check if contract is paused
    if get_paused_status(&env) {
        return Err(Error::ContractPaused);
    }

    let key = DataKey::AutoShare(id.clone());
    let mut details: AutoShareDetails = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(Error::NotFound)?;

    // Check if already a member
    for member in details.members.iter() {
        if member.address == address {
            return Err(Error::AlreadyExists);
        }
    }

    // Add new member
    details.members.push_back(GroupMember {
        address,
        percentage,
    });

    // Validate total percentage after adding
    validate_members(&details.members)?;

    // Save updated details
    env.storage().persistent().set(&key, &details);
    Ok(())
}

// ============================================================================
// Admin Management
// ============================================================================

pub fn initialize_admin(env: Env, admin: Address) {
    admin.require_auth();

    // Only set if not already initialized (instance storage)
    if !env.storage().instance().has(&INSTANCE_ADMIN) {
        env.storage().instance().set(&INSTANCE_ADMIN, &admin);

        // Initialize default usage fee (10 tokens per usage) in instance storage
        env.storage().instance().set(&INSTANCE_FEE, &10u32);

        // Initialize empty supported tokens list in instance storage
        let empty_tokens: Vec<Address> = Vec::new(&env);
        env.storage().instance().set(&INSTANCE_TOKENS, &empty_tokens);
    }
}

fn require_admin(env: &Env, caller: &Address) -> Result<(), Error> {
    let admin: Address = env
        .storage()
        .instance()
        .get(&INSTANCE_ADMIN)
        .ok_or(Error::Unauthorized)?;

    if admin != *caller {
        return Err(Error::Unauthorized);
    }

    Ok(())
}

pub fn get_admin(env: Env) -> Result<Address, Error> {
    env.storage()
        .instance()
        .get(&INSTANCE_ADMIN)
        .ok_or(Error::NotFound)
}

pub fn transfer_admin(env: Env, current_admin: Address, new_admin: Address) -> Result<(), Error> {
    current_admin.require_auth();
    require_admin(&env, &current_admin)?;

    env.storage().instance().set(&INSTANCE_ADMIN, &new_admin);
    AdminTransferred {
        old_admin: current_admin,
        new_admin,
    }
    .publish(&env);
    Ok(())
}

// ============================================================================
// Pause Management
// (IsPaused moved to instance storage – it is read on every mutating call)
// ============================================================================

pub fn pause(env: Env, admin: Address) -> Result<(), Error> {
    admin.require_auth();
    require_admin(&env, &admin)?;

    let is_paused: bool = env
        .storage()
        .instance()
        .get(&INSTANCE_PAUSED)
        .unwrap_or(false);

    if is_paused {
        return Err(Error::AlreadyPaused);
    }

    env.storage().instance().set(&INSTANCE_PAUSED, &true);
    ContractPaused {}.publish(&env);
    Ok(())
}

pub fn unpause(env: Env, admin: Address) -> Result<(), Error> {
    admin.require_auth();
    require_admin(&env, &admin)?;

    let is_paused: bool = env
        .storage()
        .instance()
        .get(&INSTANCE_PAUSED)
        .unwrap_or(false);

    if !is_paused {
        return Err(Error::NotPaused);
    }

    env.storage().instance().set(&INSTANCE_PAUSED, &false);
    ContractUnpaused {}.publish(&env);
    Ok(())
}

pub fn get_paused_status(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&INSTANCE_PAUSED)
        .unwrap_or(false)
}

// ============================================================================
// Supported Tokens Management
// (SupportedTokens moved to instance storage – checked on every create/topup)
// ============================================================================

pub fn add_supported_token(env: Env, token: Address, admin: Address) -> Result<(), Error> {
    admin.require_auth();
    require_admin(&env, &admin)?;

    let mut tokens: Vec<Address> = env
        .storage()
        .instance()
        .get(&INSTANCE_TOKENS)
        .unwrap_or(Vec::new(&env));

    // Check if token is already supported
    for existing_token in tokens.iter() {
        if existing_token == token {
            return Err(Error::AlreadyExists);
        }
    }

    tokens.push_back(token);
    env.storage().instance().set(&INSTANCE_TOKENS, &tokens);
    Ok(())
}

pub fn remove_supported_token(env: Env, token: Address, admin: Address) -> Result<(), Error> {
    admin.require_auth();
    require_admin(&env, &admin)?;

    let tokens: Vec<Address> = env
        .storage()
        .instance()
        .get(&INSTANCE_TOKENS)
        .unwrap_or(Vec::new(&env));

    let mut new_tokens: Vec<Address> = Vec::new(&env);
    let mut found = false;

    for existing_token in tokens.iter() {
        if existing_token != token {
            new_tokens.push_back(existing_token);
        } else {
            found = true;
        }
    }

    if !found {
        return Err(Error::NotFound);
    }

    env.storage().instance().set(&INSTANCE_TOKENS, &new_tokens);
    Ok(())
}

pub fn get_supported_tokens(env: Env) -> Vec<Address> {
    env.storage()
        .instance()
        .get(&INSTANCE_TOKENS)
        .unwrap_or(Vec::new(&env))
}

pub fn is_token_supported(env: Env, token: Address) -> bool {
    let tokens = get_supported_tokens(env);
    for supported_token in tokens.iter() {
        if supported_token == token {
            return true;
        }
    }
    false
}

// ============================================================================
// Payment Configuration
// (UsageFee moved to instance storage – read on every create/topup)
// ============================================================================

pub fn set_usage_fee(env: Env, fee: u32, admin: Address) -> Result<(), Error> {
    admin.require_auth();
    require_admin(&env, &admin)?;
    if fee == 0 {
        return Err(Error::InvalidAmount);
    }

    env.storage().instance().set(&INSTANCE_FEE, &fee);
    Ok(())
}

pub fn get_usage_fee(env: Env) -> u32 {
    env.storage()
        .instance()
        .get(&INSTANCE_FEE)
        .unwrap_or(10u32)
}

// ============================================================================
// Subscription Management
// ============================================================================

pub fn topup_subscription(
    env: Env,
    id: BytesN<32>,
    additional_usages: u32,
    payment_token: Address,
    payer: Address,
) -> Result<(), Error> {
    payer.require_auth();

    // Check if contract is paused
    if get_paused_status(&env) {
        return Err(Error::ContractPaused);
    }

    // Validate usage count
    if additional_usages == 0 {
        return Err(Error::InvalidUsageCount);
    }

    // Verify group exists
    let key = DataKey::AutoShare(id.clone());
    let mut details: AutoShareDetails = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(Error::NotFound)?;

    // Verify token is supported
    if !is_token_supported(env.clone(), payment_token.clone()) {
        return Err(Error::UnsupportedToken);
    }

    // Calculate cost
    let usage_fee = get_usage_fee(env.clone());
    let total_cost = (additional_usages as i128) * (usage_fee as i128);

    // Transfer tokens from payer to contract
    let token_client = token::Client::new(&env, &payment_token);
    token_client.transfer(&payer, env.current_contract_address(), &total_cost);

    // Update usage counts
    details.usage_count += additional_usages;
    details.total_usages_paid += additional_usages;

    // Save updated details
    env.storage().persistent().set(&key, &details);

    // Record payment history
    record_payment(env, payer, id, additional_usages, total_cost);

    Ok(())
}

// ============================================================================
// Payment History
// ============================================================================

fn record_payment(
    env: Env,
    user: Address,
    group_id: BytesN<32>,
    usages_purchased: u32,
    amount_paid: i128,
) {
    let timestamp = env.ledger().timestamp();

    let payment = PaymentHistory {
        user: user.clone(),
        group_id: group_id.clone(),
        usages_purchased,
        amount_paid,
        timestamp,
    };

    // Add to user's payment history
    let user_history_key = DataKey::UserPaymentHistory(user.clone());
    let mut user_history: Vec<PaymentHistory> = env
        .storage()
        .persistent()
        .get(&user_history_key)
        .unwrap_or(Vec::new(&env));
    user_history.push_back(payment.clone());
    env.storage()
        .persistent()
        .set(&user_history_key, &user_history);

    // Add to group's payment history
    let group_history_key = DataKey::GroupPaymentHistory(group_id);
    let mut group_history: Vec<PaymentHistory> = env
        .storage()
        .persistent()
        .get(&group_history_key)
        .unwrap_or(Vec::new(&env));
    group_history.push_back(payment);
    env.storage()
        .persistent()
        .set(&group_history_key, &group_history);
}

pub fn get_user_payment_history(env: Env, user: Address) -> Vec<PaymentHistory> {
    let user_history_key = DataKey::UserPaymentHistory(user);
    env.storage()
        .persistent()
        .get(&user_history_key)
        .unwrap_or(Vec::new(&env))
}

pub fn get_group_payment_history(env: Env, id: BytesN<32>) -> Vec<PaymentHistory> {
    let group_history_key = DataKey::GroupPaymentHistory(id);
    env.storage()
        .persistent()
        .get(&group_history_key)
        .unwrap_or(Vec::new(&env))
}

// ============================================================================
// Usage Tracking
// ============================================================================

pub fn get_remaining_usages(env: Env, id: BytesN<32>) -> Result<u32, Error> {
    let key = DataKey::AutoShare(id);
    let details: AutoShareDetails = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(Error::NotFound)?;
    Ok(details.usage_count)
}

pub fn get_total_usages_paid(env: Env, id: BytesN<32>) -> Result<u32, Error> {
    let key = DataKey::AutoShare(id);
    let details: AutoShareDetails = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(Error::NotFound)?;
    Ok(details.total_usages_paid)
}

pub fn reduce_usage(env: Env, id: BytesN<32>) -> Result<(), Error> {
    let key = DataKey::AutoShare(id);
    let mut details: AutoShareDetails = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(Error::NotFound)?;

    if details.usage_count == 0 {
        return Err(Error::NoUsagesRemaining);
    }

    details.usage_count -= 1;
    env.storage().persistent().set(&key, &details);
    Ok(())
}

// ============================================================================
// Group Activation Management
// ============================================================================

pub fn update_members(
    env: Env,
    id: BytesN<32>,
    caller: Address,
    new_members: Vec<GroupMember>,
) -> Result<(), Error> {
    caller.require_auth();

    if get_paused_status(&env) {
        return Err(Error::ContractPaused);
    }

    let key = DataKey::AutoShare(id.clone());
    let mut details: AutoShareDetails = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(Error::NotFound)?;

    if details.creator != caller {
        return Err(Error::Unauthorized);
    }

    if !details.is_active {
        return Err(Error::GroupInactive);
    }

    // Validate new members
    if new_members.is_empty() {
        return Err(Error::EmptyMembers);
    }

    let mut total_percentage: u32 = 0;
    let mut seen_addresses = Vec::new(&env);

    for member in new_members.iter() {
        total_percentage += member.percentage;

        for seen in seen_addresses.iter() {
            if seen == member.address {
                return Err(Error::DuplicateMember);
            }
        }
        seen_addresses.push_back(member.address.clone());
    }

    if total_percentage != 100 {
        return Err(Error::InvalidTotalPercentage);
    }

    // Update members in details (single write – no separate GroupMembers key)
    details.members = new_members.clone();
    env.storage().persistent().set(&key, &details);

    AutoshareUpdated {
        id: id.clone(),
        updater: caller,
    }
    .publish(&env);
    Ok(())
}

pub fn deactivate_group(env: Env, id: BytesN<32>, caller: Address) -> Result<(), Error> {
    caller.require_auth();

    if get_paused_status(&env) {
        return Err(Error::ContractPaused);
    }

    let key = DataKey::AutoShare(id.clone());
    let mut details: AutoShareDetails = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(Error::NotFound)?;

    if details.creator != caller {
        return Err(Error::Unauthorized);
    }

    if !details.is_active {
        return Err(Error::GroupAlreadyInactive);
    }

    details.is_active = false;
    env.storage().persistent().set(&key, &details);

    GroupDeactivated {
        id: id.clone(),
        creator: caller,
    }
    .publish(&env);
    Ok(())
}

pub fn activate_group(env: Env, id: BytesN<32>, caller: Address) -> Result<(), Error> {
    caller.require_auth();

    if get_paused_status(&env) {
        return Err(Error::ContractPaused);
    }

    let key = DataKey::AutoShare(id.clone());
    let mut details: AutoShareDetails = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(Error::NotFound)?;

    if details.creator != caller {
        return Err(Error::Unauthorized);
    }

    if details.is_active {
        return Err(Error::GroupAlreadyActive);
    }

    details.is_active = true;
    env.storage().persistent().set(&key, &details);

    GroupActivated {
        id: id.clone(),
        creator: caller,
    }
    .publish(&env);
    Ok(())
}

pub fn is_group_active(env: Env, id: BytesN<32>) -> Result<bool, Error> {
    let key = DataKey::AutoShare(id);
    let details: AutoShareDetails = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(Error::NotFound)?;
    Ok(details.is_active)
}

pub fn get_contract_balance(env: Env, token: Address) -> i128 {
    let client = token::TokenClient::new(&env, &token);
    client.balance(&env.current_contract_address())
}

pub fn withdraw(
    env: Env,
    admin: Address,
    token: Address,
    amount: i128,
    recipient: Address,
) -> Result<(), Error> {
    admin.require_auth();
    require_admin(&env, &admin)?;

    if amount <= 0 {
        return Err(Error::InvalidAmount);
    }

    let contract_balance = get_contract_balance(env.clone(), token.clone());
    if contract_balance < amount {
        return Err(Error::InsufficientContractBalance);
    }

    let client = token::TokenClient::new(&env, &token);
    client.transfer(&env.current_contract_address(), &recipient, &amount);

    Withdrawal {
        token,
        amount,
        recipient,
    }
    .publish(&env);
    Ok(())
}

fn validate_members(members: &Vec<GroupMember>) -> Result<(), Error> {
    if members.is_empty() {
        return Err(Error::EmptyMembers);
    }
    let env = members.env();
    let mut total_percentage: u32 = 0;
    let mut seen_addresses = Vec::new(env);

    for member in members.iter() {
        total_percentage += member.percentage;
        for seen in seen_addresses.iter() {
            if seen == member.address {
                return Err(Error::DuplicateMember);
            }
        }
        seen_addresses.push_back(member.address.clone());
    }

    if total_percentage != 100 {
        return Err(Error::InvalidTotalPercentage);
    }
    Ok(())
}
