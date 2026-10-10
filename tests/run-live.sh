#!/usr/bin/env bash
# Live devnet run of v3 program actions + cron sweep through the real relayer code (spec v3 Phase 5).
# - RPC: the real RPC_URL_DEVNET from .dev.vars (never printed). Devnet only: the Worker's genesis
#   check refuses any other cluster.
# - Fee payer: FEE_PAYER_LIVE_V3 from .dev.vars (create it with `cargo run -q --example keygen -- LIVE_V3`
#   and fund it with devnet SOL). One run spends ≈ 0.003 SOL in fees. Rent comes back on close/sweep,
#   except the 1-hour Room (≈ 0.00106 SOL), which stays locked until a later run's sweep closes it.
# - Identity: the mock JWKS stands in for Web3Auth (identity is not under test here; the browser
#   flow is tested on staging). Local D1 / DO state lives in a temp dir.
# Other projects: LIVE_POLICY=<policy JSON file> LIVE_SCRIPT=<test script> ./tests/run-live.sh
#   (default: the testkit policy below + tests/live-v3.mjs).
set -euo pipefail
cd "$(dirname "$0")/.."

grep -q '^FEE_PAYER_LIVE_V3=' .dev.vars && grep -q '^RPC_URL_DEVNET=' .dev.vars \
  || { echo "need FEE_PAYER_LIVE_V3 and RPC_URL_DEVNET in .dev.vars"; exit 2; }

WORK=$(mktemp -d)
PORT=8790
MOCK_PORT=8901
MOCK="http://127.0.0.1:$MOCK_PORT"
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$WORK"' EXIT

cargo build -q --example progtx
worker-build --release >/dev/null

MOCK_PORT=$MOCK_PORT node tests/mock-upstream.mjs > "$WORK/mock.log" 2>&1 &
until grep -q ready "$WORK/mock.log"; do sleep 0.2; done

# Pubkey of FEE_PAYER_LIVE_V3 (the secret stays inside this node process).
PK_LIVE=$(node -e '
  const line = require("fs").readFileSync(".dev.vars", "utf8").split("\n").find((l) => l.startsWith("FEE_PAYER_LIVE_V3="));
  const A = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
  let n = 0n; for (const c of line.split("=")[1].trim()) n = n * 58n + BigInt(A.indexOf(c));
  const bytes = Buffer.from(n.toString(16).padStart(128, "0"), "hex").subarray(32);
  let m = BigInt("0x" + bytes.toString("hex")), out = "";
  while (m > 0n) { out = A[Number(m % 58n)] + out; m /= 58n; }
  for (const b of bytes) { if (b) break; out = "1" + out; }
  console.log(out);')

TESTKIT=Bh4GY79ytnEZ4t3ZGf7qUVooaSKYyC8sM1uJpiM2xdCW
TESTKIT_SLOT=509032443   # devnet upgrade 2026-10-09 (open_room / sweep_close)
ix() { echo '{"name":"'$1'","discriminator":"'$2'","min_accounts":'$3',"max_accounts":'$4',"max_data_len":'$5"${6:+,$6}"'}'; }
PAYER='"payer_index":1,"max_rent_lamports":1100000'
POLICY='{"actions":[{"type":"program","program_id":"'$TESTKIT'","deploy_slot":'$TESTKIT_SLOT',"retryable_errors":[6000],"instructions":['\
"$(ix create 181ec828051c0777 4 4 8 "$PAYER")"','\
"$(ix close_thing a75149fabad23e79 3 3 8 '"refund_index":2')"','\
"$(ix noop 09b20d738123ed66 0 1 8)"','\
"$(ix fail_retryable 2d8f8610d2bf0cd4 0 1 8)"','\
"$(ix open_room e4fbda56c3dd8a9f 4 4 16 "$PAYER")"','\
"$(ix drain ef131e786ffcf54a 4 4 16 "$PAYER")"'],'\
'"sweeps":[{"name":"close_room","account_discriminator":"9cc7431bde17b95e","rent_payer_offset":8,"ix_discriminator":"f1ed4afddfe936dc","accounts":[{"kind":"swept","writable":true},{"kind":"fee_payer","writable":true}],"max_sends_per_run":5,"max_simulations_per_run":20}]}],'\
'"limits":{"max_instructions":4,"max_cu":400000,"max_cu_price_micro_lamports":0}}'

if [ -n "${LIVE_POLICY:-}" ]; then
  POLICY=$(node -e 'process.stdout.write(JSON.stringify(JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"))))' "$LIVE_POLICY")
fi
NOW=$(($(date +%s) * 1000))
cat > "$WORK/seed.sql" <<SQL
INSERT INTO fee_payer_history (pubkey, aud, cluster, created_at) VALUES ('$PK_LIVE', 'live-v3', 'devnet', $NOW);
INSERT INTO projects (aud, slug, name, status, cluster, web3auth_network, allowed_origins, fee_payer_pubkey, policy,
  user_daily_quota, project_rpm, project_daily_budget_lamports, min_balance_lamports, project_daily_rent_budget_lamports,
  cooldown_ms, created_at, updated_at)
VALUES ('live-v3', 'LIVE_V3', 'live v3', 'active', 'devnet', 'sapphire_devnet', '["http://localhost:5190"]', '$PK_LIVE',
  '$POLICY', 100, 60, 100000000, 10000000, 50000000, 0, $NOW, $NOW);
INSERT INTO settings (key, value) VALUES ('mainnet_enabled', 'false');
SQL
npx wrangler d1 migrations apply gasless-relayer-audit --local --persist-to "$WORK/state" >/dev/null 2>&1
npx wrangler d1 execute gasless-relayer-audit --local --persist-to "$WORK/state" --file "$WORK/seed.sql" >/dev/null

npx wrangler dev --port $PORT --ip 127.0.0.1 --persist-to "$WORK/state" --test-scheduled \
  --var "WEB3AUTH_DEVNET_JWKS_URL:$MOCK/jwks" --var "WEB3AUTH_DEVNET_ISSUER:https://issuer.test" \
  > "$WORK/wrangler.log" 2>&1 &
until grep -qE "Ready on|ERROR" "$WORK/wrangler.log"; do sleep 0.3; done

rc=0
PERSIST_DIR="$WORK/state" RELAYER_LOG="$WORK/wrangler.log" RELAYER_URL="http://127.0.0.1:$PORT" MOCK_URL="$MOCK" \
  PK_LIVE="$PK_LIVE" TESTKIT="$TESTKIT" node "${LIVE_SCRIPT:-tests/live-v3.mjs}" || rc=$?
if [ $rc -ne 0 ]; then grep -E "sweep|error|Error" "$WORK/wrangler.log" | sed -E 's#https?://[^ "]+#<url>#g' | tail -20 || true; fi
exit $rc
