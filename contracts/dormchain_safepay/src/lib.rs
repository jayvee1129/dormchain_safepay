#![no_std]

//! DormChain SafePay
//!
//! A Soroban smart contract that:
//!  1. Splits dormitory utility bills equally among registered tenants.
//!  2. Holds security deposits in escrow until both landlord and tenants
//!     mutually approve a digital move-out inspection.
//!
//! Design notes:
//!  - Persistent storage is used for the room configuration and escrow state
//!    because they live across many ledgers (entire tenancy duration).
//!  - Temporary storage is used for the active bill cycle, which is short-lived
//!    and frequently rotated — this minimizes rent costs.
//!  - All token movements go through the standard SEP-41 token interface
//!    (`soroban_sdk::token::Client`) so the contract works with USDC on Stellar.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, token, Address, Env,
    Vec,
};

/// Storage key namespace. Keeping these in a single enum avoids stringly-typed
/// keys and prevents accidental key collisions across upgrades.
#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    /// Room configuration (landlord, tenants, deposit per tenant, token).
    Room,
    /// Active utility bill state.
    Bill,
    /// Escrow registry — per-tenant deposit accounting.
    Escrow(Address),
    /// Total amount currently locked as deposits in the contract.
    TotalDeposited,
    /// Boolean flag — landlord's move-out inspection vote.
    LandlordApproval,
    /// Boolean flag — collective tenant move-out inspection vote.
    TenantApproval,
}

/// Room registry — created once at initialization.
#[derive(Clone)]
#[contracttype]
pub struct Room {
    pub landlord: Address,
    pub tenants: Vec<Address>,
    pub deposit_per_tenant: i128,
    pub usdc_token: Address,
}

/// Active utility bill cycle. `paid_flags[i]` corresponds to `tenants[i]`.
#[derive(Clone)]
#[contracttype]
pub struct Bill {
    pub total_amount: i128,
    pub share_per_tenant: i128,
    pub paid_flags: Vec<bool>,
    pub collected: i128,
    pub settled: bool,
}

/// Per-tenant escrow accounting record.
#[derive(Clone)]
#[contracttype]
pub struct DepositEscrow {
    pub tenant: Address,
    pub amount: i128,
    pub refunded: bool,
}

/// Domain-specific errors. Numeric codes are stable across versions.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    UnauthorizedTenant = 3,
    UnauthorizedLandlord = 4,
    NoTenants = 5,
    InvalidAmount = 6,
    DepositAlreadyPaid = 7,
    NoActiveBill = 8,
    BillAlreadyActive = 9,
    ShareAlreadyPaid = 10,
    BillAlreadySettled = 11,
    EscrowNotResolved = 12,
    DepositMissing = 13,
}

#[contract]
pub struct DormChainSafePay;

#[contractimpl]
impl DormChainSafePay {
    /// Initializes the room contract.
    ///
    /// - `landlord`: the address that ultimately receives utility payments and
    ///   that votes on move-out inspections.
    /// - `tenants`: a non-empty list of unique tenant addresses.
    /// - `deposit_amount`: the per-tenant security deposit (in token base units).
    /// - `usdc_token`: address of the SEP-41 USDC token contract.
    pub fn initialize_room(
        env: Env,
        landlord: Address,
        tenants: Vec<Address>,
        deposit_amount: i128,
        usdc_token: Address,
    ) {
        // Initialization can only happen once.
        if env.storage().persistent().has(&DataKey::Room) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }
        if tenants.is_empty() {
            panic_with_error!(&env, Error::NoTenants);
        }
        if deposit_amount <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }

        // The landlord must authorize the room creation — prevents griefing.
        landlord.require_auth();

        let room = Room {
            landlord: landlord.clone(),
            tenants: tenants.clone(),
            deposit_per_tenant: deposit_amount,
            usdc_token,
        };
        env.storage().persistent().set(&DataKey::Room, &room);
        env.storage()
            .persistent()
            .set(&DataKey::TotalDeposited, &0i128);
        env.storage()
            .persistent()
            .set(&DataKey::LandlordApproval, &false);
        env.storage()
            .persistent()
            .set(&DataKey::TenantApproval, &false);

        // Pre-create empty escrow records for each tenant so subsequent
        // bookkeeping calls don't need conditional initialization branches.
        for t in tenants.iter() {
            let rec = DepositEscrow {
                tenant: t.clone(),
                amount: 0,
                refunded: false,
            };
            env.storage().persistent().set(&DataKey::Escrow(t), &rec);
        }
    }

    /// Tenant transfers their security deposit (in USDC) into the contract.
    /// Funds are held until escrow resolution.
    pub fn deposit_funds(env: Env, tenant: Address) {
        let room = Self::load_room(&env);
        Self::require_tenant(&env, &room, &tenant);
        tenant.require_auth();

        let key = DataKey::Escrow(tenant.clone());
        let mut record: DepositEscrow = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NotInitialized));

        if record.amount > 0 {
            panic_with_error!(&env, Error::DepositAlreadyPaid);
        }

        // Pull the deposit from the tenant's wallet into the contract.
        let token_client = token::Client::new(&env, &room.usdc_token);
        token_client.transfer(
            &tenant,
            &env.current_contract_address(),
            &room.deposit_per_tenant,
        );

        record.amount = room.deposit_per_tenant;
        env.storage().persistent().set(&key, &record);

        let total: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalDeposited)
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::TotalDeposited, &(total + room.deposit_per_tenant));
    }

    /// Landlord creates a new utility bill, splitting it equally among tenants.
    ///
    /// Any rounding remainder is added to the **last** tenant's share so the
    /// landlord always receives the exact `total_amount` requested.
    pub fn create_bill(env: Env, caller: Address, total_amount: i128) {
        let room = Self::load_room(&env);

        // Only the landlord can issue bills.
        if caller != room.landlord {
            panic_with_error!(&env, Error::UnauthorizedLandlord);
        }
        caller.require_auth();

        if total_amount <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }

        // Disallow overlapping bill cycles — one active bill at a time.
        if let Some(existing) = env.storage().temporary().get::<DataKey, Bill>(&DataKey::Bill) {
            if !existing.settled {
                panic_with_error!(&env, Error::BillAlreadyActive);
            }
        }

        let count = room.tenants.len() as i128;
        let share = total_amount / count;

        let mut flags: Vec<bool> = Vec::new(&env);
        for _ in 0..room.tenants.len() {
            flags.push_back(false);
        }

        let bill = Bill {
            total_amount,
            share_per_tenant: share,
            paid_flags: flags,
            collected: 0,
            settled: false,
        };
        env.storage().temporary().set(&DataKey::Bill, &bill);
    }

    /// A tenant pays their share of the active bill. When the final share
    /// arrives, the contract immediately remits the full bill to the landlord.
    pub fn pay_bill_share(env: Env, tenant: Address) {
        let room = Self::load_room(&env);
        let idx = Self::tenant_index(&env, &room, &tenant);
        tenant.require_auth();

        let mut bill: Bill = env
            .storage()
            .temporary()
            .get(&DataKey::Bill)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NoActiveBill));

        if bill.settled {
            panic_with_error!(&env, Error::BillAlreadySettled);
        }
        if bill.paid_flags.get(idx).unwrap_or(false) {
            panic_with_error!(&env, Error::ShareAlreadyPaid);
        }

        // The last tenant absorbs the rounding remainder so the landlord
        // receives `total_amount` exactly.
        let last_idx = room.tenants.len() - 1;
        let due = if idx == last_idx {
            let already_assigned = bill.share_per_tenant * (room.tenants.len() as i128 - 1);
            bill.total_amount - already_assigned
        } else {
            bill.share_per_tenant
        };

        let token_client = token::Client::new(&env, &room.usdc_token);
        token_client.transfer(&tenant, &env.current_contract_address(), &due);

        bill.paid_flags.set(idx, true);
        bill.collected += due;

        // Check whether every tenant has now paid.
        let mut all_paid = true;
        for i in 0..bill.paid_flags.len() {
            if !bill.paid_flags.get(i).unwrap_or(false) {
                all_paid = false;
                break;
            }
        }

        if all_paid {
            // Forward the full bill to the landlord in a single atomic step.
            token_client.transfer(
                &env.current_contract_address(),
                &room.landlord,
                &bill.collected,
            );
            bill.settled = true;
        }

        env.storage().temporary().set(&DataKey::Bill, &bill);
    }

    /// Records a move-out inspection vote and, when both sides agree, releases
    /// the escrowed deposits. Disagreement leaves funds locked, preserving
    /// dispute integrity.
    ///
    /// The `caller` must be either the landlord or one of the tenants. Tenant
    /// votes are collectively represented by a single boolean — in practice the
    /// off-chain dApp coordinates the tenants' shared decision before invoking
    /// this function with `tenant_approved = true`.
    pub fn resolve_escrow(
        env: Env,
        caller: Address,
        landlord_approved: bool,
        tenant_approved: bool,
    ) {
        let room = Self::load_room(&env);
        caller.require_auth();

        let is_landlord = caller == room.landlord;
        let is_tenant = Self::is_tenant(&room, &caller);
        if !is_landlord && !is_tenant {
            panic_with_error!(&env, Error::UnauthorizedTenant);
        }

        // Each side may only update its own vote, preventing impersonation.
        if is_landlord {
            env.storage()
                .persistent()
                .set(&DataKey::LandlordApproval, &landlord_approved);
        }
        if is_tenant {
            env.storage()
                .persistent()
                .set(&DataKey::TenantApproval, &tenant_approved);
        }

        let l_ok: bool = env
            .storage()
            .persistent()
            .get(&DataKey::LandlordApproval)
            .unwrap_or(false);
        let t_ok: bool = env
            .storage()
            .persistent()
            .get(&DataKey::TenantApproval)
            .unwrap_or(false);

        // Mutual approval → refund all tenants.
        if l_ok && t_ok {
            let token_client = token::Client::new(&env, &room.usdc_token);
            let mut total_refunded: i128 = 0;

            for t in room.tenants.iter() {
                let key = DataKey::Escrow(t.clone());
                let mut rec: DepositEscrow = env
                    .storage()
                    .persistent()
                    .get(&key)
                    .unwrap_or_else(|| panic_with_error!(&env, Error::DepositMissing));

                if !rec.refunded && rec.amount > 0 {
                    token_client.transfer(&env.current_contract_address(), &t, &rec.amount);
                    total_refunded += rec.amount;
                    rec.refunded = true;
                    env.storage().persistent().set(&key, &rec);
                }
            }

            let total: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::TotalDeposited)
                .unwrap_or(0);
            env.storage()
                .persistent()
                .set(&DataKey::TotalDeposited, &(total - total_refunded));
        }
    }

    // ---------- Read-only helpers (useful for the front-end dashboard) ----------

    pub fn get_room(env: Env) -> Room {
        Self::load_room(&env)
    }

    pub fn get_bill(env: Env) -> Bill {
        env.storage()
            .temporary()
            .get(&DataKey::Bill)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NoActiveBill))
    }

    pub fn get_escrow(env: Env, tenant: Address) -> DepositEscrow {
        env.storage()
            .persistent()
            .get(&DataKey::Escrow(tenant))
            .unwrap_or_else(|| panic_with_error!(&env, Error::DepositMissing))
    }

    pub fn get_total_deposited(env: Env) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::TotalDeposited)
            .unwrap_or(0)
    }

    // ---------- Internal helpers ----------

    fn load_room(env: &Env) -> Room {
        env.storage()
            .persistent()
            .get(&DataKey::Room)
            .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized))
    }

    fn tenant_index(env: &Env, room: &Room, tenant: &Address) -> u32 {
        for (i, t) in room.tenants.iter().enumerate() {
            if &t == tenant {
                return i as u32;
            }
        }
        panic_with_error!(env, Error::UnauthorizedTenant);
    }

    fn require_tenant(env: &Env, room: &Room, tenant: &Address) {
        if !Self::is_tenant(room, tenant) {
            panic_with_error!(env, Error::UnauthorizedTenant);
        }
    }

    fn is_tenant(room: &Room, addr: &Address) -> bool {
        for t in room.tenants.iter() {
            if &t == addr {
                return true;
            }
        }
        false
    }
}

mod test;
