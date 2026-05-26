#![cfg(test)]

use crate::{DormChainSafePay, DormChainSafePayClient, Error};
use soroban_sdk::{
    testutils::{Address as _, MockAuth, MockAuthInvoke},
    token::{StellarAssetClient, TokenClient},
    vec, Address, Env, IntoVal, Vec,
};

/// Test harness — sets up a contract, a mock USDC token, two tenants,
/// one landlord, and pre-funds the tenants with USDC.
struct Setup<'a> {
    env: Env,
    contract: DormChainSafePayClient<'a>,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    landlord: Address,
    tenant_a: Address,
    tenant_b: Address,
    deposit_amount: i128,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let landlord = Address::generate(&env);
    let tenant_a = Address::generate(&env);
    let tenant_b = Address::generate(&env);

    // Deploy a Stellar Asset Contract to act as USDC.
    let token_admin_addr = Address::generate(&env);
    let token_contract = env.register_stellar_asset_contract_v2(token_admin_addr.clone());
    let token = TokenClient::new(&env, &token_contract.address());
    let token_admin = StellarAssetClient::new(&env, &token_contract.address());

    // Fund both tenants with 10_000 units of USDC.
    token_admin.mint(&tenant_a, &10_000);
    token_admin.mint(&tenant_b, &10_000);

    // Deploy the DormChain SafePay contract.
    let contract_id = env.register(DormChainSafePay, ());
    let contract = DormChainSafePayClient::new(&env, &contract_id);

    let tenants: Vec<Address> = vec![&env, tenant_a.clone(), tenant_b.clone()];
    let deposit_amount: i128 = 1_000;

    contract.initialize_room(
        &landlord,
        &tenants,
        &deposit_amount,
        &token_contract.address(),
    );

    Setup {
        env,
        contract,
        token,
        token_admin,
        landlord,
        tenant_a,
        tenant_b,
        deposit_amount,
    }
}

// ----------------------------------------------------------------------------
// Test 1 — Happy Path
// ----------------------------------------------------------------------------
#[test]
fn test_happy_path_end_to_end() {
    let s = setup();

    // Both tenants pay deposits.
    s.contract.deposit_funds(&s.tenant_a);
    s.contract.deposit_funds(&s.tenant_b);

    assert_eq!(s.contract.get_total_deposited(), s.deposit_amount * 2);
    assert_eq!(s.token.balance(&s.contract.address), s.deposit_amount * 2);

    // Landlord creates a 500-unit bill.
    s.contract.create_bill(&s.landlord, &500);

    // Tenants pay their shares (250 each).
    s.contract.pay_bill_share(&s.tenant_a);
    s.contract.pay_bill_share(&s.tenant_b);

    // Landlord should have received the full 500.
    assert_eq!(s.token.balance(&s.landlord), 500);

    // Bill should now be settled.
    let bill = s.contract.get_bill();
    assert!(bill.settled);
    assert_eq!(bill.collected, 500);

    // Move-out: both sides approve, deposits released.
    s.contract.resolve_escrow(&s.landlord, &true, &false);
    s.contract.resolve_escrow(&s.tenant_a, &false, &true);

    assert_eq!(s.token.balance(&s.tenant_a), 10_000 - 250); // refunded deposit
    assert_eq!(s.token.balance(&s.tenant_b), 10_000 - 250);
    assert_eq!(s.contract.get_total_deposited(), 0);
}

// ----------------------------------------------------------------------------
// Test 2 — Unauthorized caller
// ----------------------------------------------------------------------------
#[test]
fn test_unauthorized_caller_cannot_create_bill_or_resolve() {
    let s = setup();
    let intruder = Address::generate(&s.env);

    // Bill creation by non-landlord must fail.
    let create_result = s
        .contract
        .try_create_bill(&intruder, &500)
        .err()
        .expect("expected error from unauthorized bill creation")
        .expect("expected contract error");
    assert_eq!(create_result, Error::UnauthorizedLandlord);

    // Escrow resolution by an unrelated address must fail.
    let resolve_result = s
        .contract
        .try_resolve_escrow(&intruder, &true, &true)
        .err()
        .expect("expected error from unauthorized escrow vote")
        .expect("expected contract error");
    assert_eq!(resolve_result, Error::UnauthorizedTenant);
}

// ----------------------------------------------------------------------------
// Test 3 — Double payment / overpayment
// ----------------------------------------------------------------------------
#[test]
fn test_double_pay_share_is_rejected() {
    let s = setup();

    s.contract.deposit_funds(&s.tenant_a);
    s.contract.deposit_funds(&s.tenant_b);
    s.contract.create_bill(&s.landlord, &400);

    s.contract.pay_bill_share(&s.tenant_a);

    // Tenant A tries to pay again before the bill settles.
    let err = s
        .contract
        .try_pay_bill_share(&s.tenant_a)
        .err()
        .expect("expected double-pay error")
        .expect("expected contract error");
    assert_eq!(err, Error::ShareAlreadyPaid);

    // Tenant B completes the bill — should settle cleanly.
    s.contract.pay_bill_share(&s.tenant_b);

    // Any further payment after settlement must also fail.
    let post_settle = s
        .contract
        .try_pay_bill_share(&s.tenant_a)
        .err()
        .expect("expected post-settlement error")
        .expect("expected contract error");
    assert_eq!(post_settle, Error::BillAlreadySettled);
}

// ----------------------------------------------------------------------------
// Test 4 — Balance state verification across bill aggregation
// ----------------------------------------------------------------------------
#[test]
fn test_balances_track_state_before_and_after_settlement() {
    let s = setup();

    s.contract.deposit_funds(&s.tenant_a);
    s.contract.deposit_funds(&s.tenant_b);

    let bill_total: i128 = 777; // odd number to verify remainder handling
    s.contract.create_bill(&s.landlord, &bill_total);

    let before_landlord = s.token.balance(&s.landlord);
    let before_contract = s.token.balance(&s.contract.address);
    let before_a = s.token.balance(&s.tenant_a);
    let before_b = s.token.balance(&s.tenant_b);

    assert_eq!(before_landlord, 0);
    assert_eq!(before_contract, s.deposit_amount * 2);

    // Tenant A pays the floor share (777 / 2 = 388).
    s.contract.pay_bill_share(&s.tenant_a);
    assert_eq!(s.token.balance(&s.tenant_a), before_a - 388);

    // Contract holds deposits + partial bill, landlord not yet paid.
    assert_eq!(s.token.balance(&s.contract.address), before_contract + 388);
    assert_eq!(s.token.balance(&s.landlord), 0);

    // Tenant B pays the remainder (777 - 388 = 389) — this triggers payout.
    s.contract.pay_bill_share(&s.tenant_b);
    assert_eq!(s.token.balance(&s.tenant_b), before_b - 389);

    // Landlord received exactly the bill total.
    assert_eq!(s.token.balance(&s.landlord), bill_total);
    // Contract retains only the deposits.
    assert_eq!(s.token.balance(&s.contract.address), s.deposit_amount * 2);

    let bill = s.contract.get_bill();
    assert!(bill.settled);
    assert_eq!(bill.collected, bill_total);
}

// ----------------------------------------------------------------------------
// Test 5 — Escrow stays locked on dispute / mismatched votes
// ----------------------------------------------------------------------------
#[test]
fn test_escrow_remains_locked_on_disagreement() {
    let s = setup();

    s.contract.deposit_funds(&s.tenant_a);
    s.contract.deposit_funds(&s.tenant_b);
    let locked_total = s.deposit_amount * 2;
    assert_eq!(s.token.balance(&s.contract.address), locked_total);

    // Landlord rejects; tenant approves → mismatch, no release.
    s.contract.resolve_escrow(&s.landlord, &false, &false);
    s.contract.resolve_escrow(&s.tenant_a, &false, &true);

    assert_eq!(s.token.balance(&s.contract.address), locked_total);
    assert_eq!(s.contract.get_total_deposited(), locked_total);

    let escrow_a = s.contract.get_escrow(&s.tenant_a);
    let escrow_b = s.contract.get_escrow(&s.tenant_b);
    assert!(!escrow_a.refunded);
    assert!(!escrow_b.refunded);
    assert_eq!(escrow_a.amount, s.deposit_amount);
    assert_eq!(escrow_b.amount, s.deposit_amount);

    // Once landlord flips to approve, funds release.
    s.contract.resolve_escrow(&s.landlord, &true, &false);

    assert_eq!(s.token.balance(&s.contract.address), 0);
    assert_eq!(s.contract.get_total_deposited(), 0);

    // Silence unused warnings for `token_admin` in test harness.
    let _ = &s.token_admin;
}
