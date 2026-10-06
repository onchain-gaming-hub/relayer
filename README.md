# gasless-relayer

Multi-project gasless relayer for Solana — Cloudflare Worker in Rust/Wasm with Durable Objects and D1.
One project = one Web3Auth clientId (`aud`) = one fee payer wallet = one cluster.
Spec: `../docs/plans/2026-10-06_multi-project-relayer-v2-spec.md` (extends `2026-10-05_gasless-relayer-poc-spec.md`).
**Phase 1** (registry, per-project fee payers, project/cluster throttles) is done; daily quotas and budgets are
Phase 2. Devnet only until Phase 5 is approved.

```
POST /relay                    { "transaction": "<base64 wire tx>" }   Authorization: Bearer <Web3Auth idToken>
GET  /relayer?project=<aud>  → { pubkey, cluster, policy, cooldown_ms, user_daily_quota, wallet_binding }
```

## Layout

| File | Role |
|------|------|
| `src/lib.rs` | Worker router, CORS (union of active origins), idToken auth (JWKS per network), cluster guard, co-sign |
| `src/registry.rs` | D1 `projects` + `settings` snapshot per isolate (stale-while-revalidate), project parsing |
| `src/policy.rs` | Tx policy: structural (before auth) + per project (fee payer, memo limit/prefix, cost) |
| `src/jwt.rs` | ES256 JWT verify (`alg`/`kid`/`aud`/`iss`/`exp`) + wallet binding |
| `src/user_controller.rs` | DO per `<aud>:<wallet>`: sync in-flight lock → cooldown → ProjectThrottle → RPC → audit |
| `src/project_throttle.rs` | DO per project: RPM + cached fee payer balance → ClusterThrottle |
| `src/cluster_throttle.rs` | DO per cluster: RPM across all projects (protects the RPC plan) |
| `src/window.rs` | Fixed one-minute window counter |
| `src/rpc.rs` | JSON-RPC with timeout; send outcome = accepted / rejected / unknown |
| `migrations/` | D1: `relay_log` (0001), `projects` / `fee_payer_history` / `settings` (0002) |

## Setup (local, devnet)

Requires the toolchain pinned in `rust-toolchain.toml` (rustup installs it) and `cargo install worker-build --version 0.8.7`.

```bash
npm install                                      # local wrangler 4.147
cargo run -q --example keygen -- POC             # appends FEE_PAYER_POC to .dev.vars, prints the pubkey
# fund the pubkey at https://faucet.solana.com (devnet)
echo 'RPC_URL_DEVNET=https://<keyed devnet RPC>' >> .dev.vars
scripts/dev-project.sh <clientId> POC <pubkey> http://localhost:5173   # migrations + local registry row
```

`scripts/dev-project.sh` only touches the **local** D1. Production projects are added by hand (spec v2 §10).

**RPC: the public `https://api.devnet.solana.com` rejects Workers** (HTTP 403 "Your IP or provider is blocked" for any
request carrying the `CF-Worker` header, which Cloudflare — and `wrangler dev` — adds to every subrequest). Use a keyed
RPC (Alchemy, Helius, QuickNode, …) in `RPC_URL_DEVNET` / `RPC_URL_MAINNET`: `.dev.vars` locally,
`npx wrangler secret put` when deployed. The cluster guard checks each RPC's genesis hash. If the provider reports rate
limits as JSON-RPC errors, add its code to `NODE_SIDE_ERROR_CODES` (spec v1 R6).

Web3Auth: Sapphire Devnet defaults are built in (`iss` `web3auth.io`, citadel JWKS — what real tokens use, not the
`api-auth.web3auth.io` values in Web3Auth's docs). Sapphire Mainnet has no default until verified (spec v2 spike 0.2):
mainnet projects answer 500 until `WEB3AUTH_MAINNET_ISSUER` and `WEB3AUTH_MAINNET_JWKS_URL` are both set.

Mainnet is also behind a kill switch: the D1 row `settings('mainnet_enabled')` must be `'true'` (≤ 60 s to take
effect, no deploy). If D1 cannot be read for `REGISTRY_MAX_STALE_MS`, mainnet projects fail closed.

Have a `solana-keygen` JSON keypair instead? Convert the 64-byte array to Base58, e.g.
`node -e "const b=require('fs').readFileSync(process.argv[1]);console.log(require('bs58').default.encode(Buffer.from(JSON.parse(b))))" key.json`.

## Run & test

```bash
npm run dev                 # wrangler dev on :8787 (secrets from .dev.vars, registry from local D1)
cargo test --lib            # policy / registry / jwt / rpc / window / tx unit tests
./tests/run-e2e.sh [A B C]  # local e2e vs wrangler dev + mock RPC/JWKS — no network
```

`tests/run-e2e.sh` seeds six projects (two devnet, one mainnet, one paused, one with a missing secret, one with a
mismatched key) with throwaway keys, and covers v1 T1–T18, T20, T22–T28 plus v2 T-v2-1/2/3/7/8/11/12, the kill switch,
retire, and the D1-outage path (stale snapshot, then mainnet fail-closed). Not covered here: T19 (storage `put` failing
after a broadcast — not simulable locally), T21 (Memo Count — frontend), the "throttle DO itself fails" half of T18
(only the `getBalance` failure path is simulated), a refund that arrives after the window rolled over
(`refunded:false`; needs > 60 s between reject and refund), and an unparseable `Content-Length` (→ 400; clients and
proxies normally reject it before the Worker). The mock JWKS includes an RSA key so every suite also exercises mixed
key sets.

## Deploy

See spec v2 §7 (Phase 4) and §10. Never deploy or run remote D1 migrations without approval.

⚠️ **One fee payer per project, one cluster per fee payer, for life** (`fee_payer_history` enforces it in D1). A 504
`rpc_unknown` returns the fee payer's slot-0 signature, so a user holds a fully signed transaction; the relayer cannot
tell which cluster its blockhash came from. Mainnet keys never go on a machine the Oracle can read.
