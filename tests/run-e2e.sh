#!/usr/bin/env bash
# Runs the local e2e suites (spec v1 §7 + v2 §8) against wrangler dev + mock upstream.
# No network access needed; never touches devnet, mainnet, or any remote Cloudflare resource.
# Fee payer keys are throwaway keys generated per run (never funded anywhere).
set -euo pipefail
cd "$(dirname "$0")/.."

WORK=$(mktemp -d)
PORT=8788
MOCK_PORT=8899
MOCK="http://127.0.0.1:$MOCK_PORT"
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$WORK"' EXIT

cargo build -q --example mktx --example genkey --example progtx
worker-build --release >/dev/null

MOCK_PORT=$MOCK_PORT node tests/mock-upstream.mjs > "$WORK/mock.log" 2>&1 &
until grep -q ready "$WORK/mock.log"; do sleep 0.2; done

# Projects (spec v2 §8): A, B devnet · M mainnet · P paused · X secret missing · Y key mismatch
read -r SK_A PK_A <<<"$(./target/debug/examples/genkey)"
read -r SK_B PK_B <<<"$(./target/debug/examples/genkey)"
read -r SK_M PK_M <<<"$(./target/debug/examples/genkey)"
read -r SK_P PK_P <<<"$(./target/debug/examples/genkey)"
read -r SK_C PK_C <<<"$(./target/debug/examples/genkey)"
read -r SK_D PK_D <<<"$(./target/debug/examples/genkey)"
read -r SK_G PK_G <<<"$(./target/debug/examples/genkey)"
read -r _ NOVERIFY <<<"$(./target/debug/examples/genkey)"   # program id: verify_inner_ix=false (W7)
read -r _ PINX <<<"$(./target/debug/examples/genkey)"       # program id: pinned slot never matches (W6)
read -r _ PK_X <<<"$(./target/debug/examples/genkey)"
read -r SK_Y _ <<<"$(./target/debug/examples/genkey)"
read -r _ PK_Y <<<"$(./target/debug/examples/genkey)"

COMMON=(
  --var "RPC_URL_DEVNET:$MOCK"
  --var "RPC_URL_MAINNET:$MOCK/mainnet"
  --var "WEB3AUTH_DEVNET_JWKS_URL:$MOCK/jwks"
  --var "WEB3AUTH_DEVNET_ISSUER:https://issuer.test"
  --var "WEB3AUTH_MAINNET_JWKS_URL:$MOCK/jwks"
  --var "WEB3AUTH_MAINNET_ISSUER:https://issuer-mainnet.test"
  --var "RPC_TIMEOUT_MS:2000"
  --var "FEE_PAYER_PA:$SK_A" --var "FEE_PAYER_PB:$SK_B" --var "FEE_PAYER_PM:$SK_M"
  --var "FEE_PAYER_PP:$SK_P" --var "FEE_PAYER_PY:$SK_Y"
  --var "FEE_PAYER_PC:$SK_C" --var "FEE_PAYER_PD:$SK_D" --var "FEE_PAYER_PG:$SK_G"
)

TESTKIT=Bh4GY79ytnEZ4t3ZGf7qUVooaSKYyC8sM1uJpiM2xdCW
BIG_BUDGET=1000000000
NOOP_IX='{"name":"noop","discriminator":"09b20d738123ed66","min_accounts":0,"max_accounts":1,"max_data_len":8}'
PROGRAM_POLICY='{"actions":[{"type":"program","program_id":"'$TESTKIT'","deploy_slot":1,"retryable_errors":[6000],"instructions":[{"name":"create","discriminator":"181ec828051c0777","payer_index":1,"max_rent_lamports":1100000,"min_accounts":4,"max_accounts":4,"max_data_len":8},{"name":"close_thing","discriminator":"a75149fabad23e79","refund_index":2,"min_accounts":3,"max_accounts":3,"max_data_len":8},'$NOOP_IX']},{"type":"program","program_id":"'$NOVERIFY'","deploy_slot":7,"verify_inner_ix":false,"instructions":['$NOOP_IX']},{"type":"program","program_id":"'$PINX'","deploy_slot":5,"instructions":['$NOOP_IX']}],"limits":{"max_instructions":4,"max_cu":400000,"max_cu_price_micro_lamports":1000,"skip_preflight_after_verify":true}}'

# seed <rpm_a> <rpm_b> <mainnet_enabled> <quota_a> <budget_a> <budget_cd> <budget_b>
seed() {
  local memo='{"actions":[{"type":"memo","max_bytes":128,"prefix":"gasless-poc:"}]}'
  local cols="aud, slug, name, status, cluster, web3auth_network, allowed_origins, fee_payer_pubkey, policy, user_daily_quota, project_rpm, project_daily_budget_lamports, min_balance_lamports, created_at, updated_at"
  row() { # aud slug status cluster network origin pubkey rpm [quota] [budget] [policy]
    echo "INSERT INTO projects ($cols) VALUES ('$1','$2','$2','$3','$4','$5','[\"$6\"]','$7','${11:-$memo}',${9:-100},$8,${10:-1000000000},50000000,0,0);"
    echo "INSERT INTO fee_payer_history (pubkey, aud, cluster, created_at) VALUES ('$7','$1','$4',0);"
  }
  {
    row test-client PA active devnet sapphire_devnet http://localhost:5173 "$PK_A" "$1" "$4" "$5"
    row client-b PB active devnet sapphire_devnet http://localhost:5174 "$PK_B" "$2" 100 "$7"
    row client-m PM active mainnet sapphire_mainnet http://localhost:5175 "$PK_M" 1000
    row client-p PP paused devnet sapphire_devnet http://localhost:5176 "$PK_P" 1000
    row client-x PX active devnet sapphire_devnet http://localhost:5177 "$PK_X" 1000
    row client-y PY active devnet sapphire_devnet http://localhost:5178 "$PK_Y" 1000
    row client-c PC active devnet sapphire_devnet http://localhost:5179 "$PK_C" 1000 100 "$6"
    row client-d PD active devnet sapphire_devnet http://localhost:5180 "$PK_D" 1000 100 "$6"
    # Program actions only (spec v3): testkit-shaped program, no memo action.
    row client-g PG active devnet sapphire_devnet http://localhost:5181 "$PK_G" 1000 100 "$BIG_BUDGET" "$PROGRAM_POLICY"
    echo "INSERT INTO settings (key, value) VALUES ('mainnet_enabled', '$3');"
  } > "$WORK/seed.sql"
}

# run_suite <suite> <state dir> <reseed yes|no> <rpm_a> <rpm_b> <mainnet> <quota_a> <budget_a> <budget_cd> <budget_b> [wrangler args]
run_suite() {
  local suite=$1 state=$2 reseed=$3; shift 3
  local persist="$WORK/state-$state"
  if [ "$reseed" = yes ]; then
    npx wrangler d1 migrations apply gasless-relayer-audit --local --persist-to "$persist" >/dev/null 2>&1
    seed "$1" "$2" "$3" "$4" "$5" "$6" "$7"
    npx wrangler d1 execute gasless-relayer-audit --local --persist-to "$persist" --file "$WORK/seed.sql" >/dev/null 2>&1
  fi
  shift 7
  local log="$WORK/wrangler-$suite.log"
  npx wrangler dev --port $PORT --ip 127.0.0.1 --persist-to "$persist" "${COMMON[@]}" "$@" \
    > "$log" 2>&1 &
  local pid=$!
  until grep -qE "Ready on|ERROR" "$log"; do sleep 0.3; done
  local rc=0
  PERSIST_DIR="$persist" WRANGLER_LOG="$log" RELAYER_URL="http://127.0.0.1:$PORT" MOCK_URL="$MOCK" \
    PK_A="$PK_A" PK_B="$PK_B" PK_M="$PK_M" PK_X="$PK_X" PK_Y="$PK_Y" PK_C="$PK_C" PK_D="$PK_D" PK_G="$PK_G" TESTKIT="$TESTKIT" NOVERIFY="$NOVERIFY" PINX="$PINX" \
    node tests/e2e.mjs "$suite" || rc=$?
  kill $pid 2>/dev/null; wait $pid 2>/dev/null || true
  # Reset mock behavior between suites.
  curl -s -X POST "$MOCK/__state" \
    -d '{"genesis":"EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG","genesisMainnet":"5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d","balances":{},"balance":10000000000,"balanceMode":"ok","balanceDelayMs":0,"jwksMode":"ok","send":"ok","delayMs":0,"simulate":null,"simOwner":"11111111111111111111111111111111","programs":{}}' >/dev/null
  return $rc
}

# Usage: ./tests/run-e2e.sh [A] [B] [C] [D] [E] [F]   (default: all; D also runs D2 = restart check)
SUITES=${*:-A B C D E F}
BIG=1000000000
status=0
for s in $SUITES; do
  case $s in
    A) run_suite A A yes 1000 1000 false 100 $BIG $BIG $BIG --var COOLDOWN_MS:2000 --var REJECT_COOLDOWN_MS:1000 || status=1 ;;
    B) run_suite B B yes 3 3 false 100 $BIG $BIG $BIG --var COOLDOWN_MS:5000 --var CLUSTER_RPM_DEVNET:5 || status=1 ;;
    C) run_suite C C yes 1000 1000 false 100 $BIG $BIG $BIG --var COOLDOWN_MS:2000 \
         --var REGISTRY_CACHE_MS:1000 --var REGISTRY_MAX_STALE_MS:4000 || status=1 ;;
    # Phase 2: user quota 2 (A), project budget 3 tx (A) / 4 tx (B), cluster budget 6 tx;
    # D2 restarts on the same state. Daily counters reset at 00:00 UTC: don't straddle it.
    D) left=$(( 86400 - $(date -u +%s) % 86400 ))
       if [ $left -lt 120 ]; then echo "… waiting ${left}s for UTC midnight (daily counters reset)"; sleep $((left + 2)); fi
       D_ARGS=(--var COOLDOWN_MS:300 --var REJECT_COOLDOWN_MS:300 --var CLUSTER_DAILY_BUDGET_DEVNET:60000)
       run_suite D D yes 1000 1000 false 2 30000 $BIG 40000 "${D_ARGS[@]}" || status=1
       run_suite D2 D no 0 0 0 0 0 0 0 "${D_ARGS[@]}" || status=1 ;;
    E) run_suite E E yes 1000 1000 false 100 $BIG 30000 $BIG --var COOLDOWN_MS:300 || status=1 ;;
    F) run_suite F F yes 1000 1000 false 100 $BIG $BIG $BIG --var COOLDOWN_MS:300 || status=1 ;;
    *) echo "unknown suite $s"; status=1 ;;
  esac
done
exit $status
