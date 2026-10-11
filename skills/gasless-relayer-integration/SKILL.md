---
name: gasless-relayer-integration
description: Best practices for designing a Solana program and frontend that use the hub's gasless relayer (relayer pays fees, can front rent through a registered payer slot, closes accounts with a cron sweep). Use when designing or reviewing a Solana program's accounts/instructions, deciding who pays rent, building transactions in a frontend that sends them to the relayer, writing the relayer policy for a project, or preparing a project to register on the relayer.
---

# Gasless Relayer Integration — Best Practices

> Source of truth: `solana-poc/relayer/skills/gasless-relayer-integration/SKILL.md`. Do not edit copies in other projects; edit this file and copy it again.
> References: relayer spec v3 (`solana-poc/docs/plans/2026-10-08_program-actions-v3-spec.md`) · v2 (`2026-10-06_multi-project-relayer-v2-spec.md`) · rent research (`2026-10-08_rent-sponsorship-research.md`) · lobby compat (`2026-10-08_lobby-relayer-compat.md`)
> Working examples: program `solana-poc/testkit` (Anchor 1.2.1) · FE `solana-poc/frontend/src/lib/testkit.ts` · policy in `docs/plans/2026-10-09_v3-phase5-staging-runbook.md` step 3
> Deployed ids, URLs and wallets: `solana-poc/relayer/SKILL.md`
> Version: 2026-10-09 (v3) · maintained by the hub maintainers

## 0. Relayer overview
- **1 project = 1 Web3Auth clientId (`aud`) = 1 fee payer wallet = 1 cluster.** Each project has its own quota, budget and wallet, so damage in one project does not spread to another.
- Flow:
  1. The user logs in with Web3Auth (social) and signs the tx.
  2. The FE sends `POST /relay` with the idToken.
  3. The relayer checks the policy and signs as fee payer (slot 0).
  4. When needed, it simulates the tx and checks the result.
  5. It broadcasts the tx.
- The relayer **does not cache program state**. The chain is the only source of truth.
- Every program to be sponsored must be **registered in the project the player logs in with**. A program shared by several projects is registered separately in each one.
- **blockchain-games-hub (H-3, 2026-10-10):** the hub has **one project**, the Web3Auth clientId of hub-web.
  - Every hub program (lobby, rewards-pool, games) is registered as a program action in that project. There is no separate project or clientId per game.
  - To disable one game, remove its program from the policy (never leave the policy empty).
  - Quota, budget, RPM, cooldown, `max_instructions` and the fee payer wallet are shared by all games.

## 1. What is supported

| Capability | Status |
|---|---|
| SPL Memo v2 (prefix per project) | ✅ v2 |
| Per-user daily quota · RPM / budget per project and per cluster · kill switch | ✅ v2 |
| **Program action**: allowlist by program id + discriminator | ✅ v3 (devnet staging, 2026-10-09) |
| **Payer slot** (relayer fronts rent for `init`) · **refund slot** (rent returns to the relayer) | ✅ v3 |
| Simulation + inner-instruction check before sending (can be turned off per program) · program version pin | ✅ v3 |
| ComputeBudget with ceilings · separate fee and rent budgets · per-project cooldown/backoff · retryable custom errors | ✅ v3 |
| User-signed crank · **cron sweep** that closes accounts and returns rent every 5 minutes | ✅ v3 |
| Mainnet | 🔜 needs separate approval |
| Address Lookup Tables · Loader-v4 | ❌ not supported |
| Registering System / ComputeBudget / BPF loaders / Token / Token-2022 / ATA / Memo v1-v2 / ALT program / **precompiles (Ed25519, Secp256k1, Secp256r1)** as program actions | ❌ not supported. Precompiles would make the declared fee wrong, so precompile signature verification cannot be used in txs sent through the relayer |

## 2. Who pays rent: decide before designing accounts

```
Does the program create accounts?
├─ No ───────────────────────────────────────────────► no rent to think about (relayer pays only the fee)
└─ Yes
   ├─ our own program + reviewable + we control upgrades ─► (P) Payer slot ⭐ simplest
   ├─ relayer should trust nothing / someone else's program ─► (V) the program's own rent vault
   └─ user has SOL (normal wallet, not via the relayer) ───► user pays (payer is an ordinary Signer)
```

**Trust rule:** the relayer must trust **every program that receives the fee payer wallet as an account in an instruction**. The fee payer is a tx-level signer, and signer rights pass through CPI.

The relayer defends in 3 layers:
- the registered slot position (W3)
- the version pin (W6)
- simulating the tx and checking where the fee payer's money goes (spec §4.3)

A program that pays rent from its own vault never receives the fee payer, so it does not need to be trusted.

### (P) Payer slot: MUST
The relayer allows the fee payer inside an instruction **only** at the `payer_index` (or `refund_index`) of a registered ix. In addition, **every top-level ix must belong to a registered program** (+ ComputeBudget), which closes Kora #720-style attacks. The program must:
1. **Separate `user: Signer` and `payer: Signer` (`mut`)** in every ix that creates an account. An ix that creates no account **must not take a `payer`**.
2. **Use `payer` only to fund `init` / `init_if_needed`.** That means System `createAccount`, or Anchor's `transfer` → `allocate` → `assign` when the account already holds lamports.
   - Never transfer it anywhere else.
   - **Never pass `payer` through CPI to any program other than System.**
   - The relayer simulates the tx and rejects any inner ix that involves the fee payer in any other way (`inner_ix_rejected`).
3. **Every account funded by `payer` stores `rent_payer: Pubkey`**, and **on close refunds only to `rent_payer`** (`close = rent_payer` + `has_one = rent_payer`). Never let the caller choose the destination; this is what stops rent farming.
4. **Always have a way to return the rent.** Finished or expired accounts must be closable without waiting for the user, because most social users never come back to press a button.
   - Add a **permissionless** close ix that refunds `rent_payer`. The destination is pinned, so whoever presses it cannot steal anything.
   - Then choose who presses it:
     - (a) **the project's own worker**, using its own wallet (e.g. lobby → Lobby Worker, per L14, 2026-10-10)
     - (b) **the relayer's cron sweep** (§2.1)
5. **Account size is fixed at `init` and capped.** Any TTL that keeps rent locked must have a program-level cap (a const).
6. **Upgrades must be announced to the relayer.** The relayer pins the ProgramData `deploy_slot` (or `immutable`).
   - If you upgrade without notice, every ix of that program returns **503 `program_changed`** until a hub admin reviews the new code and updates `deploy_slot`.
   - The upgrade authority should be a multisig, or frozen, before mainnet.
7. `emit!` an event carrying `rent_payer` on create and on close (for later audits).
8. Write a test asserting that `payer`'s lamports drop by **exactly the rent of the created account**. Set the ix's `max_rent_lamports` ≥ the real rent: the relayer charges the rent budget by this declared value, not the actual amount, and rejects the tx if the simulated amount exceeds it.
9. **8-byte discriminator** (Anchor default): an ix with a payer or refund slot must have an 8-byte discriminator.

### Refund slot (an ix the user calls to close an account)
- Register `refund_index` = the position of `rent_payer` (writable). The relayer simulates the tx and requires that the fee payer's balance **does not drop by more than the fee**.
- The program must be **version-pinned** (`deploy_slot` or `immutable`), as with a payer slot.
- It works **only for accounts whose `rent_payer` is the project's fee payer**. Accounts whose rent the user paid must be closed from the user's own wallet; sending them through the relayer fails `has_one`.
- **A tx must not contain both a payer slot and a refund slot.**

### 2.1 If the relayer's cron sweep does the closing (option (b) of §2 P item 4)
Every 5 minutes, the relayer cron finds accounts with `getProgramAccounts` and builds the close tx itself, **with the fee payer as the only signer**. Design it like this (example: testkit `Room` + `sweep_close`):
1. The account has its own discriminator (Anchor `account:<Name>`) and **`rent_payer` at a fixed offset**, e.g. the first field after the discriminator = offset 8.
2. The close ix has data = **the 8-byte discriminator only** (no arguments), and accounts that follow the registered template, e.g. `[account (mut), rent_payer (mut, Signer)]`. Fixed accounts such as a config PDA may be added.
3. `close = rent_payer` + `has_one = rent_payer`, and **`rent_payer` is the only signer the ix requires**.
4. When the account cannot be closed yet (retention not reached, game not finished), **return a custom error** such as `RetentionNotReached`. The relayer simulates the tx, sees the error, and skips it at no fee.
   - Never succeed without paying the rent back. The relayer only sends txs that return at least `rent(0 bytes)` to its wallet.
5. An account that can never be closed costs one simulation every cycle (no money, but RPC load), so always give accounts an expiry.

### (V) Rent vault: MUST
1. The vault is a PDA of the program that is **owned by System and holds no data**. Create accounts with `system_program::create_account` + `invoke_signed`. Anchor `init` requires a Signer payer, so the CPI has to be written by hand; this is not yet verified with anchor 1.2.1.
2. On close, refund into the vault (pinned in code).
3. **Prevent griefing:** limit accounts per user, close inactive ones, cap their size, and monitor the vault balance.
4. The program owner (not the relayer) is responsible for funding the vault.
5. The relayer applies its normal rule (the fee payer may not appear in any ix). Register only the instructions whose fee should be sponsored.

### Both models: SHOULD
- Consider **compressed state** before creating per-user accounts at scale: cNFT/Bubblegum ~0.00001 SOL per mint and Metaplex Core ~0.003 SOL, versus Token Metadata ~0.022 SOL.
- Always ask the cluster for rent (`getMinimumBalanceForRentExemption`), never hardcode it. Measured on devnet 2026-10-09: 73 bytes = 1,021,080 · 81 bytes = 1,061,720 · 0 bytes = 650,240.

## 3. Transaction shape the relayer accepts (program action)
1. **Fee payer = `pubkey` from `GET /relayer?project=<clientId>`**, in slot 0. Do not hard-code it.
2. **Exactly 2 signers: the fee payer and the user** (`num_required_signatures == 2`). Do not use fresh keypairs as signers (e.g. a mint keypair); use PDAs instead.
3. Message format:
   - Legacy or v0, **without ALT**.
   - No duplicate keys in the message's account list. Compilers dedupe these automatically, and one ix may still reference the same key at two positions.
   - Actual size < 1232 bytes.
4. **Every top-level ix must belong to a registered program** (+ ComputeBudget).
   - At least one registered ix is required.
   - Instruction count ≤ `max_instructions` (default 4, **ComputeBudget counts too**).
   - Each ix must match its registered discriminator, `min_accounts..max_accounts` and `max_data_len`.
5. **Do not mix memo and program actions** in one tx. Token / ATA / Token-2022 / Memo / System cannot be top-level ixs; have your program CPI into them instead.
6. **ComputeBudget:** `SetComputeUnitLimit` ≤ `max_cu` (default 400,000) and `SetComputeUnitPrice` ≤ `max_cu_price_micro_lamports` (default 0 = no priority fee), each at most once. **A price requires a limit.**
7. **Crank ix** (e.g. `tick`, callable by anyone): the FE appends **the user as a readonly signer at the end of the crank ix's accounts** (a remaining account), so the relayer can count the user's quota.
   - Register `max_accounts` with +1 for it.
   - **The program must not read the crank ix's `remaining_accounts`**, or must skip the trailing readonly signer. Anchor ignores remaining accounts unless you read them yourself.
8. **Conflicts must fail before reaching the chain.** Check preconditions (phase, counter, pointer) at the start of the ix.
   - The relayer simulates before sending (when `verify_inner_ix` is on). A normal error there → 400 `simulation_failed`, with a per-wallet backoff that keeps growing.
   - A program with `verify_inner_ix` off, or state that changed between simulate and send, fails at the RPC's preflight instead → 502 `rpc_rejected` with the same backoff.
   - Neither path costs a fee.
9. **For expected races (e.g. room full, already closed), use custom errors registered as `retryable_errors`.** These return 409 `retryable` with a short cooldown (`retry_cooldown_ms` ≥ 100 ms) and no accumulated backoff.
   - The relayer attributes the error by **top-level ix index** in `InstructionError[i, {Custom: c}]`, so an error raised by a CPI'd program counts as the registered program's.
   - **Pick retryable codes that do not collide with errors of programs you CPI into.** For example, Anchor `3012 AccountNotInitialized` works when a user races a sweep that already closed the account.
10. **The per-wallet cooldown** is set per project (`cooldown_ms`; the Worker default is 10 s).
    - Games that send txs rapidly should ask for 0–1000 ms and rely on RPM + quota + budget instead.
    - Still combine actions into one ix where possible.
11. **What the relayer charges:**
    - fee = 10,000 (2 signatures) + `ceil(cu_limit × cu_price / 1e6)`, charged to the fee budget
    - rent = the sum of `max_rent_lamports` of every payer-slot ix, charged to the rent budget
    - The two budgets are separate, and rent refunded on close does not go back into the budget.
12. **CU measured on devnet** (testkit, Anchor 1.2.1, 2026-10-09): noop 502 · `init` of a 73-byte account 7,933 · `init` of 81 bytes + Clock 12,667 · close (refund slot) 3,680 · sweep close 2,005. Measure your own program and send the numbers to the relayer (§5).

## 4. Frontend (Web3Auth + @solana/kit)
1. **D18 (placeholder):** the Web3Auth embedded wallet refuses to partially sign (error 5663009).
   - Before calling `solana:signTransaction`, put a **64-byte placeholder (not all zeros) into every slot except the user's**. The relayer overwrites it.
   - See `frontend/src/lib/solana.ts` (`withPlaceholders`, `signWithWallet`).
   - **Verified in a browser (2026-10-09)** for memo, crank (user is only a readonly signer) and payer slot (fee payer is a writable signer in the ix).
2. Build instructions with `@solana/kit`, then `buildTx(relayer, ix)` (fee payer = relayer, fresh `confirmed` blockhash every time before opening the popup). Full examples: `frontend/src/lib/testkit.ts` (PDAs via `getProgramDerivedAddress`, `AccountRole` matching each slot).
3. User account: call `standard:connect`, and sign through the wallet-standard feature directly (`useSignTransaction` swallows errors).
4. The build-time `VITE_SOLANA_CLUSTER` must match `cluster` in `GET /relayer`, and the FE's RPC must be on the same cluster (check the genesis hash). If either differs, disable sending.
5. **Handle errors by code** (see `frontend/src/lib/messages.ts`):

| HTTP / code | What the FE should do |
|---|---|
| 200 | Poll the signature status (`confirmed`) |
| 400 `validation` | FE/policy bug: ix, slot, ComputeBudget or size does not match the registration (`reason` says what) |
| 400 `simulation_failed` | Show the program's error (`reason`) · wait `retry_after_ms` (backoff) |
| 400 `inner_ix_rejected` | The program used the relayer's money against the rules: alert the maintainers, not the user |
| 409 `retryable` | Re-read state from the chain, then **build a new tx** after `retry_after_ms` |
| 409 `in_progress` | This wallet's previous tx has not finished (no `retry_after_ms`): wait briefly, then build a new tx |
| 429 `cooldown` / `project_throttle` / `cluster_throttle` | Wait `retry_after_ms`, then build a new tx |
| 502 `rpc_rejected` | The RPC preflight refused it (e.g. a program error with `verify_inner_ix` off, an expired blockhash): show `reason`, wait `retry_after_ms` (backoff), then build a new tx |
| 429 `user_quota` · 503 `project_budget` / `cluster_budget` (`reason`: `fee` or `rent`) | Show the reset time (00:00 UTC) instead of a countdown |
| 503 `program_changed` | The program was upgraded and not re-approved yet: disable the buttons that use it |
| 503 `low_balance` / `upstream` | The relayer is not ready: try again later |
| 504 `rpc_unknown` | **Not a failure:** poll the returned `signature` or re-read the account on chain · never roll back client state |

6. Show quota from `GET /quota?wallet=` and from the `/relay` response.
7. Never resend the same tx after a 409/429. Build a new one with a fresh blockhash.

## 5. Information to send when registering a project
1. Web3Auth clientId (Sapphire network matching the cluster) + the FE origin (exact, https).
2. Cluster (devnet / mainnet).
3. **Program id + versioned IDL + current deploy slot** (or confirmation that it is immutable), plus the review result for that code version if it has payer/refund slots.
4. For each ix to sponsor:
   - `name`, `discriminator` (hex)
   - `min_accounts` / `max_accounts` (+1 for cranks)
   - `max_data_len`
   - if applicable, `payer_index` + `max_rent_lamports`, or `refund_index`
5. `retryable_errors` (custom error numbers) · measured CU per ix (if ComputeBudget is needed, give the limit/price).
6. Sweep, if any: `account_discriminator` (hex of `account:<Name>`), `rent_payer_offset`, `ix_discriminator`, the account template, and the per-run maximums.
7. **Estimates:** txs per user per day, rent per action, users per day, and tx frequency per wallet. These set quota, RPM, fee budget, **rent budget** (default 0 = payer slots unusable) and cooldown.
   - **Sweeps consume the project's resources too.** Every sweep RPC call (list, blockhash, simulate, send) takes 1 project + cluster RPM slot, and every sent tx takes 5,000 lamports of the fee budget.
   - For example, `max_simulations_per_run` 20 means up to ~25 RPM per 5-minute cycle.
8. Fee payer wallet:
   - A hub admin creates it following the runbook and stores it as a Worker secret. The project owner never holds the key.
   - Agree who funds it with SOL.
   - With payer slots: `min_balance_lamports` ≥ `max_instructions × max(max_rent_lamports) + 1,000,000`.

Example policy (devnet staging, testkit):
```json
{"actions":[
 {"type":"memo","max_bytes":128,"prefix":"gasless-poc:"},
 {"type":"program","program_id":"Bh4GY79ytnEZ4t3ZGf7qUVooaSKYyC8sM1uJpiM2xdCW","deploy_slot":509032443,"retryable_errors":[6000],
  "instructions":[
   {"name":"create","discriminator":"181ec828051c0777","payer_index":1,"max_rent_lamports":1100000,"min_accounts":4,"max_accounts":4,"max_data_len":8},
   {"name":"close_thing","discriminator":"a75149fabad23e79","refund_index":2,"min_accounts":3,"max_accounts":3,"max_data_len":8},
   {"name":"noop","discriminator":"09b20d738123ed66","min_accounts":0,"max_accounts":1,"max_data_len":8},
   {"name":"fail_retryable","discriminator":"2d8f8610d2bf0cd4","min_accounts":0,"max_accounts":1,"max_data_len":8},
   {"name":"open_room","discriminator":"e4fbda56c3dd8a9f","payer_index":1,"max_rent_lamports":1100000,"min_accounts":4,"max_accounts":4,"max_data_len":16}],
  "sweeps":[{"name":"close_room","account_discriminator":"9cc7431bde17b95e","rent_payer_offset":8,"ix_discriminator":"f1ed4afddfe936dc",
   "accounts":[{"kind":"swept","writable":true},{"kind":"fee_payer","writable":true}],"max_sends_per_run":5,"max_simulations_per_run":20}]}],
 "limits":{"max_instructions":4,"max_cu":400000,"max_cu_price_micro_lamports":0}}
```
- `verify_inner_ix` (default `true`) may be turned off only for programs that are pinned and entirely written by us. This lowers latency, but safety then rests solely on the pinned code.
- `open_room` is an ix with an argument (`i64`, 16 bytes of data) that creates the `Room` closed by the `close_room` sweep. `noop` / `fail_retryable` are cranks (`max_accounts` 1 = the appended user).
- A policy that fails to parse makes **the whole project unusable, memo included**:
  - via curl (no Origin), `GET /relayer` returns 404
  - from a browser, you get 403 `origin` / a CORS error, because the project's origin drops out of the allowlist (the FE shows "Relayer unreachable")
  - Always check it against a local relayer before writing it to the real D1.

## 6. Checklist before asking to register
- [ ] Rent model chosen (no accounts / P / V), with the reasoning written down
- [ ] (P) items 1–9 of §2 done, including the item 8 test
- [ ] Every account the relayer funds has a way back: a refund slot and/or a permissionless close ix pressed by the project's worker, or closable by the relayer's cron sweep per §2.1 (`rent_payer` is the only signer, error while not yet closable)
- [ ] (V) griefing is limited, and someone is assigned to fund the vault
- [ ] No ix passes the fee payer / payer through CPI to any program other than System
- [ ] Crank ixs do not read `remaining_accounts` (or skip the trailing signer)
- [ ] Expected races use custom errors registered as retryable, which do not collide with errors of CPI'd programs
- [ ] Main-flow txs: 2 signers, no ALT, no duplicate message keys, CU and size measured
- [ ] FE does the D18 placeholder, checks the cluster, and handles the §4 error table (especially `retryable` and `rpc_unknown`)
- [ ] A plan to notify the relayer before upgrading the program (otherwise 503 `program_changed`)
- [ ] All §5 information sent
