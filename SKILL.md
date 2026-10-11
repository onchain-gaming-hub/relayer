---
name: gasless-relayer
description: Operator and onboarding guide for maintainer's gasless Solana relayer (Cloudflare Worker + Durable Objects + D1, devnet staging). Lists everything already deployed on devnet (Worker URL, D1, registered projects, fee payer wallets, test program ids, slots), the HTTP API, how to register a new app or program, how to test it (local e2e, live devnet), and how to operate it safely. Use when a team wants to use the relayer, when registering or changing a project/policy, or when you need an id/address/URL of the relayer's devnet deployment.
---

# Gasless Relayer: User and Operator Guide

> Everything in this file was **verified read-only against the live systems on 2026-10-11**: D1 remote, `wrangler deployments list` and devnet RPC.
> Values that change over time (balances, Worker version, policy) must be re-checked before use. Each part of section 2 gives the command to do that.
> Design rules for programs and frontends (payer slot, crank, sweep, FE error handling) are in **`relayer/skills/gasless-relayer-integration/SKILL.md`**. This file links there and does not repeat them.
> Specs: `docs/plans/2026-10-08_program-actions-v3-spec.md` (v3) · v2: `docs/plans/2026-10-06_multi-project-relayer-v2-spec.md`
> Maintainer: maintainer · approves and runs all remote changes: **maintainer**

## 1. What the relayer is
- A user who logs in with Web3Auth (social login) has a wallet but no SOL.
- The FE builds a tx with **fee payer = the relayer's wallet**. The user signs it, and the FE sends it with `POST /relay` together with the idToken.
- The relayer then:
  1. checks identity, the project's policy, and quota/budget;
  2. signs as fee payer;
  3. when needed, simulates the tx and checks where the relayer's money goes;
  4. sends the tx to the chain.
- **1 project = 1 Web3Auth clientId (`aud`) = 1 fee payer wallet = 1 cluster.** A project's `policy` says what it sponsors:
  - **memo**: Memo v2 with a prefix.
  - **program**: instructions of registered programs, matched by discriminator and account count, with optional payer slot, refund slot and sweeps.
- **blockchain-games-hub uses a single project (H-3, 2026-10-10).** Every hub program is registered in the hub-web project, and the platform funds that wallet itself (H-5).

## 2. What is deployed (devnet only · mainnet is disabled)

### 2.1 Cloudflare (staging)
| Item | Value |
|---|---|
| Worker | `gasless-relayer` · **https://gasless-relayer.meaw-pipat.workers.dev** |
| Running version | `53182635-e546-4a14-bef6-3c80f4a2f8b2` (uploaded 2026-10-09T02:21Z = relayer v3 Phases 1–5). ⚠️ **Not deployed yet:** the audit `cost_lamports` fix (relayer `dadb272`) and later doc-only commits. Check with `npx wrangler deployments list` |
| Cron | `*/5 * * * *` (sweeps for projects that have `sweeps` in their policy) |
| Durable Objects | `UserController` (per `aud:wallet`) · `ProjectThrottle` (per `aud`) · `ClusterThrottle` (per cluster) · `SweepController` (per `aud`) · migration tags `v2` + `v3` |
| D1 | `gasless-relayer-audit` · id `e3bf7ce9-6c44-43a6-b6cb-398c04068d9f` · tables `projects`, `fee_payer_history`, `settings`, `relay_log` · latest migration `0003_v3_program_actions.sql` |
| Secrets (names only) | `FEE_PAYER_STG_POC` · `RPC_URL_DEVNET` (Alchemy devnet) · **no `RPC_URL_MAINNET`** |
| `settings` | `mainnet_enabled = false` |
| Vars | Full list in `relayer/wrangler.toml` [vars]. Main values: `WALLET_BINDING = wallets` (the wallet in the tx must belong to the idToken) · cooldown 10 s · reject backoff 3 s → 60 s · cluster RPM 300 · cluster fee budget 10 SOL/day on devnet and 0.2 SOL/day on mainnet · cluster rent budget 10 SOL/day on both clusters · `RETRY_COOLDOWN_MS` 1000 · registry cache 60 s (a stale snapshot is served for up to `REGISTRY_MAX_STALE_MS` 600 s if D1 is down) |
| Frontend (Pages) | **https://gasless-poc-frontend.pages.dev** · `?v3` = program-action test panel (testkit) |

### 2.2 Registered projects (D1 remote)
| slug | `aud` (Web3Auth clientId · Sapphire Devnet) | origin | fee payer | settings |
|---|---|---|---|---|
| `STG_POC` | `BCBRRKiatPMMJBRPEdNaqI-fRZ0w3Z1C0J0Q51ypmz2vEYAkQLbCpRFA7L_cgK2ylavoMkdFIOWcKJCLg6JKns4` | `https://gasless-poc-frontend.pages.dev` | `hp1nnTcTLqrBczEGujPFhvtXzL2HJRxnBumeU216xyr` | active · quota 100/day · 60 RPM · fee budget 1 SOL/day · rent budget 0.05 SOL/day · min balance 0.05 SOL · cooldown = Worker default |

`STG_POC` policy:
- memo with prefix `gasless-poc:` (128 bytes)
- program testkit (pinned to slot `509032443`): `create`, `close_thing`, `noop`, `fail_retryable`, `open_room`
- sweep `close_room`
- limits: `max_instructions` 4, `max_cu` 400,000, no priority fee
- check: `npx wrangler d1 execute gasless-relayer-audit --remote --command "SELECT slug, status, policy FROM projects"`

### 2.3 Wallets (pubkeys only · never put a private key in docs, git or on the command line)
| Role | pubkey | Where the key lives | Balance on 2026-10-11 |
|---|---|---|---|
| `STG_POC` fee payer (staging) | `hp1nnTcTLqrBczEGujPFhvtXzL2HJRxnBumeU216xyr` | Worker secret `FEE_PAYER_STG_POC`, backup in `relayer/.dev.vars` | 1.999865 SOL |
| Local live-test fee payer | `FSrvuAXPpjuSDjZxc24j2dsnFa1K4Au9s5vavqHcN989` | `relayer/.dev.vars` (`FEE_PAYER_LIVE_V3`) | 0.19663 SOL · **3 testkit `Room` accounts** are still open (1-hour retention rooms from earlier live runs, ~1,061,720 lamports of rent each). The sweep in the next `run-live.sh` run closes them. The staging cron does not touch them, because it only searches accounts whose `rent_payer` is that project's own wallet |
| Local dev fee payer (`wrangler dev` + local FE) | `7uA1QxYTnzXzNBoVaE4gbLvXzxtPLjzvWAQqvmcMU5Cc` | `relayer/.dev.vars` (`FEE_PAYER_POC`) | 1.99993 SOL |
| testkit deployer / upgrade authority | `Ej77KMCVLEcDTrSjXHTLHTjMZUAr5F46PFrDw6MVVe7F` | `~/.config/solana/testkit-deployer-devnet.json` (outside the repo) | 1.00756 SOL |

- Every wallet is **devnet only**. A wallet is bound to one project and one cluster for life (`fee_payer_history`).
- `relayer/.dev.vars` (gitignored, mode 0600) also still holds the unused v1 keys `RELAYER_PRIVATE_KEY` and `RPC_URL`.

### 2.4 Programs on devnet
| Program | id | ProgramData / deploy slot | Status with the relayer |
|---|---|---|---|
| **testkit** (the relayer's own · Anchor 1.2.1 · repo `solana-poc/testkit`) | `Bh4GY79ytnEZ4t3ZGf7qUVooaSKYyC8sM1uJpiM2xdCW` | `Bx4QouRTySJqMCPDhBStWY5EPzvkk5iFQrkvkU9Nq6YU` · slot **509032443** · authority `Ej77KM…` | Registered in `STG_POC` (pinned) |
| platform-core (platform-core team) | `AJnPSYsn7J2muHfKztRtpZz5kBP94dWkNZLr9aMhu18G` | `4HDMqVpFQQLwrSXrzR4yEPpBTPk5ZZ864gGGsumjdadk` · slot 509128032 · authority `HRstpEJNgPzXUPMQmDFef4HzAfydbqK6KorqFuzMsTvy` | **Not registered.** It is never sponsored, and it must not be a top-level ix in a tx sent through the relayer |
| rewards-pool (rewards-pool team) | `HgApfwjAg1L2FJL3fX7A4WX5yfhrVV8k9Ahii5Pn8HVW` | `CnmWkV6B1uXQ8fSiPtDBodeBWu6CfoqXhvAqgTZmL9Dk` · slot 509454201 · authority `HRstpE…` (same) | **Waiting to be registered** in the hub project once hub-web is deployed. Its policy has passed testing on devnet (`tests/live/rewards-pool-policy.json`) |
| lobby (lobby team) | `5zdBSvep3yV6v1SVD67uw9N5Wb3eT9Hp9D3eg4YSRqRu` | `5uWN3vjanUhyhQzpa4oVhjhp1otjaWq85FeNV9SporKu` · slot **509795184** · authority `HRstpE…` (same) | **Waiting to be registered** in the hub project. Open request (not yet answered) for a per-user rent cap in the relayer: `maintainer/<internal-note>`. Latest registration data: `<internal-note>`. Per L14, lobby closes its accounts with its own Lobby Worker and does not use the relayer's sweep |

testkit instructions (discriminator · accounts):
- `create` `181ec828051c0777` · [user s, payer w s (payer slot 1), thing PDA `["thing", user]` w, system]
- `close_thing` `a75149fabad23e79` · [user s, thing w, rent_payer w (refund slot 2)]
- `noop` `09b20d738123ed66` · [] (crank: append the user as a readonly signer)
- `fail_retryable` `2d8f8610d2bf0cd4` · Custom 6000
- `open_room(i64)` `e4fbda56c3dd8a9f` · [user s, payer w s (payer slot 1), room PDA `["room", user]` w, system]
- `sweep_close` `f1ed4afddfe936dc` · [room w, rent_payer w s] · returns Custom 6001 before the retention time
- `drain` `ef131e786ffcf54a` (negative test, never register it)
- account `Room` = `9cc7431bde17b95e` (`rent_payer` at offset 8) · account `Thing` = `12f42ecb0fc17b7b`

Genesis hashes: devnet `EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG` · mainnet `5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d`. The Worker checks each cluster's RPC against these.

## 3. HTTP API (for frontends)
| Method / path | Requires | Returns |
|---|---|---|
| `GET /relayer?project=<clientId>` | No Origin (e.g. curl), or the Origin of any active project (any other Origin is refused) | `{pubkey, cluster, policy.actions, cooldown_ms, user_daily_quota, wallet_binding}` · project inactive / unknown / broken policy → 404 |
| `POST /relay` | **Origin required, and it must be in the `allowed_origins` of the project named by the token** · `Authorization: Bearer <Web3Auth idToken>` · `Content-Type: application/json` · body `{"transaction": "<base64 wire tx>"}` | `200 {signature, quota}`, or an error from the table in the integration skill §4 |
| `GET /quota?wallet=<pubkey>` | Bearer idToken of that wallet · same Origin rule as `GET /relayer` | `{used, limit, resets_at}` (resets at 00:00 UTC) |
| `OPTIONS` | An allowed Origin | 204 + CORS (`Authorization, Content-Type` · exposes `Retry-After`) |

- Limits:
  - body ≤ 4 KB
  - tx is Legacy or v0 without ALT
  - exactly 2 signers: fee payer in slot 0, plus the user
- FE examples: `frontend/src/lib/solana.ts` (`buildTx`, and `signWithWallet` with the D18 placeholder) and `frontend/src/lib/testkit.ts`.

## 4. Using the relayer for a new app or program
1. **Read the integration skill** and design the program and FE by its rules, especially §2 (payer slot), §3 (tx shape) and §5 (what to send).
2. **Pick the project:**
   - hub players → add the program to the hub project (H-3).
   - any other app with its own Web3Auth clientId → create a new project. Follow steps 2, 4 and 6–8 of `docs/plans/2026-10-07_phase4-staging-runbook.md`:
     1. create the wallet (`cargo run -q --example keygen -- <SLUG>`)
     2. `secret put`
     3. insert the row as paused, then enable it
3. **Write the policy JSON.** Schema: spec v3 §3. Real examples: step 3 of `docs/plans/2026-10-09_v3-phase5-staging-runbook.md`, and `tests/live/rewards-pool-policy.json`.
4. **Always check it locally first.** A policy that fails to parse makes the **whole project unusable** (for the hub, every game).
   - Test on devnet through the real relayer: `LIVE_POLICY=<file> LIVE_SCRIPT=<script> ./tests/run-live.sh` (see section 5).
   - At minimum, load it into `wrangler dev` with a local D1; `GET /relayer` must return 200 with every action.
5. **maintainer registers it on staging.** The Oracle never writes to remote D1 itself.
   1. Save the current policy.
   2. Run `UPDATE projects SET policy = …` via `--file`.
   3. Wait 65 s, then call `GET /relayer`.
   4. Have a rollback SQL ready (example in the Phase 5 runbook).
6. **Test through the real FE** (Web3Auth), then check `relay_log`.

## 5. Test tools (`relayer/`)
| Command | What it does | Network |
|---|---|---|
| `cargo test --lib` | unit tests (75) | none |
| `cargo clippy --lib --tests --examples --target wasm32-unknown-unknown` | lint | none |
| `./tests/run-e2e.sh [A B C D E F S M]` | e2e on `wrangler dev` + mock RPC/JWKS. Suites: A–E = v1/v2 · F = program actions · S = cron sweep · M = daily-counter migration from an older Worker (`OLD_WORKER_DIR=<git worktree of the deployed version>`; the worktree has no `node_modules`, so symlink it from `relayer/` to use the same wrangler instead of fetching one from npm) | none (all mocked) |
| `./tests/run-live.sh` | Real relayer locally + **real devnet** (Alchemy from `.dev.vars`) + mock JWKS instead of Web3Auth. Default: 15 testkit checks (~0.003 SOL, plus 1-hour Room rent held temporarily) | devnet |
| `LIVE_POLICY=tests/live/rewards-pool-policy.json LIVE_SCRIPT=tests/live/rewards-pool.mjs ./tests/run-live.sh` | Example of testing another project's policy (sends nothing, spends no SOL) | devnet |
| `./target/debug/examples/progtx <fee_payer> <blockhash> <seed> <ix>...` | Build test txs (`p:<program>:<hex>:<accts>` · `u:s`, `fp:w`, `new:w`, `pda/<seed>/<program>:w` · `cbl:` / `cbp:` · `PROGTX_V0=1`) | none |
| `$FE/?v3` | testkit test panel through real Web3Auth | staging |

## 6. Operations
- **Deploy order:**
  - Always apply D1 migrations before deploying the Worker (Worker v3 reads the new columns).
  - A new DO migration (added class) blocks `wrangler rollback` across it, so reverts must be forward deploys.
  - Before a migration that drops or rebuilds a table, record `wrangler d1 time-travel info` and run `d1 export`.
- **Runbooks already executed:**
  - `docs/plans/2026-10-07_phase4-staging-runbook.md`: v2 launch
  - `2026-10-08_v3-staging-runbook.md`: v3 upgrade
  - `2026-10-09_v3-phase5-staging-runbook.md`: program actions on `STG_POC`
- **Emergency stop (fastest first):**
  - `UPDATE projects SET status='paused'` takes effect within ≤ 60 s.
  - Remove the program from the policy (never leave the policy empty).
  - Withdraw from the wallet below `min_balance` → 503 `low_balance` within ≤ 30 s.
- **Funding:**
  - Send SOL to the fee payer pubkey (no key needed).
  - The balance must always stay above the project's `min_balance_lamports`.
  - **The relayer has no low-balance alert yet**, so check it yourself: `solana balance <pubkey> --url devnet`.
- **Upgrading a registered program** makes its instructions return 503 `program_changed` until `deploy_slot` is updated in the policy. The program's team must announce upgrades in advance.
- **Audit:** `npx wrangler d1 execute gasless-relayer-audit --remote --command "SELECT id, status, reason, cost_lamports, rent_lamports, aud FROM relay_log ORDER BY id DESC LIMIT 20"`. Statuses:
  - `success`: reason empty, or `already processed`.
  - `rpc_error`, with a reason prefix:
    - from users: `simulation_failed:`, `inner_ix_rejected:`, `retryable:`, `rejected:`, `unknown:`
    - from the cron: `sweep_rejected:`, `sweep rejected:`, `sweep unknown:`, `sweep already processed`
  - `sweep`: reason = the sweep name.
- **Known limitations:**
  - No per-program budget or audit (`relay_log` has no program id column).
  - Cron CPU time on Cloudflare has not been measured.
  - The relayer does not check tx size itself (the RPC rejects txs over 1232 bytes).

## 7. Rules that must not be broken
- **Devnet only** until maintainer separately approves mainnet. Do not set `RPC_URL_MAINNET`, and do not set `mainnet_enabled = true`.
- **Never** put a private key or an RPC URL containing an API key in git, `wrangler.toml`, the command line or docs. Pass secrets via stdin only.
- `wrangler deploy`, remote D1 writes, `secret put` and testkit upgrades are **approved and run by maintainer**.
- Never DELETE rows from `projects` / `fee_payer_history` (use `status='retired'`).
