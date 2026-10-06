# gasless-relayer

Generic gasless relayer for Solana **devnet** — Cloudflare Worker in Rust/Wasm with Durable Objects and D1.
Spec: `../docs/plans/2026-10-05_gasless-relayer-poc-spec.md`

```
POST /relay   { "transaction": "<base64 wire tx>" }   Authorization: Bearer <Web3Auth idToken>
GET  /relayer → { pubkey, cooldown_ms, allowlist, max_memo_bytes, wallet_binding }
```

## Layout

| File | Role |
|------|------|
| `src/lib.rs` | Worker router, HTTP hardening, idToken auth (JWKS cache), devnet guard, co-sign + verify |
| `src/policy.rs` | Stateless tx policy: Legacy/V0 only, no ALT, keys `[relayer, user, Memo v2]`, 2 sigs, 1 ix |
| `src/jwt.rs` | ES256 JWT verify (`alg`/`kid`/`aud`/`iss`/`exp`) + wallet binding |
| `src/relay_controller.rs` | Per-wallet DO: sync in-flight lock → cooldown → GlobalThrottle → RPC → audit |
| `src/global_throttle.rs` | Singleton DO: requests/min + cached relayer balance (circuit breaker) |
| `src/rpc.rs` | JSON-RPC with timeout; send outcome = accepted / rejected / unknown |
| `schema.sql` | D1 audit log |

## Setup

Requires the toolchain pinned in `rust-toolchain.toml` (rustup installs it) and `cargo install worker-build --version 0.8.7`.

```bash
npm install                          # local wrangler 4.147
cargo run -q --example keygen        # creates .dev.vars with a new devnet relayer key, prints the pubkey
# fund the pubkey at https://faucet.solana.com (devnet)
npx wrangler d1 execute gasless-relayer-audit --local --file schema.sql
```

Set `WEB3AUTH_CLIENT_ID` in `wrangler.toml` (or `--var`) before relaying real tokens. `WEB3AUTH_ISSUER` (`web3auth.io`)
and `WEB3AUTH_JWKS_URL` (`https://api.web3auth.io/citadel-service/.well-known/jwks.json`) default to what real
Sapphire Devnet social-login tokens use — not the `api-auth.web3auth.io` values in Web3Auth's docs.

**RPC: the public `https://api.devnet.solana.com` rejects Workers** (HTTP 403 "Your IP or provider is blocked" for any
request carrying the `CF-Worker` header, which Cloudflare — and `wrangler dev` — adds to every subrequest). Use a keyed
devnet RPC (Alchemy, Helius, QuickNode, …) and keep it out of git: add `RPC_URL=https://…` to `.dev.vars` locally,
`npx wrangler secret put RPC_URL` when deployed. The devnet guard still checks the genesis hash. If the provider reports
rate limits as JSON-RPC errors, add its code to `NODE_SIDE_ERROR_CODES` (spec R6).

Have a `solana-keygen` JSON keypair instead? Convert the 64-byte array to Base58, e.g.
`node -e "const b=require('fs').readFileSync(process.argv[1]);console.log(require('bs58').default.encode(Buffer.from(JSON.parse(b))))" key.json`.

## Run & test

```bash
npm run dev                 # wrangler dev on :8787 (RPC_URL from .dev.vars)
cargo test --lib            # policy / jwt / rpc / tx unit tests
./tests/run-e2e.sh          # local e2e vs wrangler dev + mock RPC/JWKS (spec §7) — no network
```

`tests/run-e2e.sh` covers T1–T18, T20, T22–T28 (T7: HTTP spot-checks here, shape cases as unit tests in `cargo test`).
Not covered here: T19 (storage `put` failing after a broadcast — not simulable locally), T21 (Memo Count — frontend),
the "GlobalThrottle DO itself fails" half of T18 (only the `getBalance` failure path is simulated), a refund that
arrives after the global window rolled over (`refunded:false`; needs >60 s between reject and refund), and an
unparseable `Content-Length` (→ 400; clients/proxies normally reject it before the Worker). The mock JWKS includes an
RSA key so every suite also exercises mixed key sets.

## Deploy

See the runbook in spec §8. Never deploy or run remote D1 migrations without approval.

⚠️ **The relayer keypair is devnet-only — never fund it on mainnet or any other cluster.** A 504 `rpc_unknown`
returns the relayer's slot-0 signature, so a user holds a fully signed transaction; the relayer cannot tell which
cluster its blockhash came from. Use a fresh, dedicated keypair per cluster.
