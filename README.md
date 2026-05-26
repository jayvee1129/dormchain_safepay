# DormChain SafePay

> A Soroban smart contract that automatically splits dorm utility bills and holds security deposits in escrow until both tenants and landlords approve a digital move-out inspection.

---

## Problem & Solution

**Problem.** Students renting shared dorms in Quezon City frequently argue over unpaid utility bills and unfair security-deposit deductions. Payments and room damages are tracked manually — usually in group chats — leaving no enforceable proof when disputes arise.

**Solution.** DormChain SafePay runs on Stellar's Soroban platform and:

1. **Splits utility bills** equally among registered roommates, collects each share in USDC, and forwards the full amount to the landlord only once every tenant has paid.
2. **Escrows security deposits** on-chain and releases them only when both the landlord and the tenants approve the move-out inspection. Disagreement keeps funds locked — no unilateral seizure.

---

## Hackathon Alignment & Vision

| Track | Alignment |
|---|---|
| **Region** | South-East Asia — built for Metro Manila student-housing market |
| **User Type** | Students (tenants) & SMEs (boarding-house owners) |
| **Theme** | Finance & Payments (split billing) + Commerce & Loyalty (marketplace escrow) |
| **Complexity** | Soroban smart contract + Web app front-end |

**Vision.** Make small-scale rental finance — the most common transactional pain point for young Filipinos — fully transparent, dispute-free, and verifiable on a public ledger, without forcing either party to learn crypto jargon.

---

## Stellar Features Used

- **Soroban smart contracts** — bill-splitting logic, escrow state machine, and authorization.
- **USDC transfers** — via the standard SEP-41 token interface (`soroban_sdk::token::Client`), letting deposits and bill payments settle in a stablecoin Filipino users can trust.

---

## Setup & Compilation Prerequisites

You will need:

- **Rust** (stable toolchain, ≥ 1.78)
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf [sh.rustup.rs](https://sh.rustup.rs) | sh
  rustup target add wasm32-unknown-unknown
  ```
- **Stellar / Soroban CLI** (≥ 21.x)
  ```bash
  cargo install --locked stellar-cli
  ```
- **A funded testnet identity**
  ```bash
  stellar keys generate --global alice --network testnet --fund
  ```

---

## Local Compilation

```bash
stellar contract build
```

The optimized Wasm artifact will be written to:

```
target/wasm32-unknown-unknown/release/dormchain_safepay.wasm
```

---

## Running the Tests

```bash
cargo test
```

All five tests (happy path, unauthorized caller, double-pay, balance verification, escrow lock) should pass against the mocked USDC token.

---

## Testnet Deployment Workflow

```bash
# 1. Build the contract
stellar contract build

# 2. Deploy the Wasm to testnet
stellar contract deploy \
  --wasm target/wasm32-unknown-unknown/release/dormchain_safepay.wasm \
  --source alice \
  --network testnet

# → returns CONTRACT_ID (save it)

# 3. Initialize the room
stellar contract invoke \
  --id <CONTRACT_ID> \
  --source alice \
  --network testnet \
  -- \
  initialize_room \
  --landlord <LANDLORD_ADDRESS> \
  --tenants '["<TENANT_A_ADDRESS>","<TENANT_B_ADDRESS>"]' \
  --deposit_amount 1000 \
  --usdc_token <USDC_CONTRACT_ADDRESS>
```

---

## Sample CLI Invocations

**Create a utility bill (landlord-only):**

```bash
stellar contract invoke \
  --id <CONTRACT_ID> \
  --source landlord \
  --network testnet \
  -- \
  create_bill \
  --caller <LANDLORD_ADDRESS> \
  --total_amount 500
```

**Pay a bill share (each tenant):**

```bash
stellar contract invoke \
  --id <CONTRACT_ID> \
  --source tenant_a \
  --network testnet \
  -- \
  pay_bill_share \
  --tenant <TENANT_A_ADDRESS>
```

**Resolve move-out escrow (both parties must call):**

```bash
# Landlord vote
stellar contract invoke --id <CONTRACT_ID> --source landlord --network testnet \
  -- resolve_escrow --caller <LANDLORD_ADDRESS> --landlord_approved true --tenant_approved false

# Tenant vote (collective decision relayed by the front-end)
stellar contract invoke --id <CONTRACT_ID> --source tenant_a --network testnet \
  -- resolve_escrow --caller <TENANT_A_ADDRESS> --landlord_approved false --tenant_approved true
```

When both stored votes are `true`, all tenant deposits are refunded atomically in a single transaction.

---

## License

This project is released under the **MIT License**. See `LICENSE` for the full text.


CBI6IB6IFXUJXRFJSW7OEJBMPFKH7FBQ4OFKMR5ICCHKNOVDG2W4CSWW 