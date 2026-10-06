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

cargo build -q --example mktx --example genkey
worker-build --release >/dev/null

MOCK_PORT=$MOCK_PORT node tests/mock-upstream.mjs > "$WORK/mock.log" 2>&1 &
until grep -q ready "$WORK/mock.log"; do sleep 0.2; done

# Projects (spec v2 §8): A, B devnet · M mainnet · P paused · X secret missing · Y key mismatch
read -r SK_A PK_A <<<"$(./target/debug/examples/genkey)"
read -r SK_B PK_B <<<"$(./target/debug/examples/genkey)"
read -r SK_M PK_M <<<"$(./target/debug/examples/genkey)"
read -r SK_P PK_P <<<"$(./target/debug/examples/genkey)"
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
)

# seed <rpm_a> <rpm_b> <mainnet_enabled>
seed() {
  local memo='{"actions":[{"type":"memo","max_bytes":128,"prefix":"gasless-poc:"}]}'
  local cols="aud, slug, name, status, cluster, web3auth_network, allowed_origins, fee_payer_pubkey, policy, user_daily_quota, project_rpm, project_daily_budget_lamports, min_balance_lamports, created_at, updated_at"
  row() { # aud slug status cluster network origin pubkey rpm
    echo "INSERT INTO projects ($cols) VALUES ('$1','$2','$2','$3','$4','$5','[\"$6\"]','$7','$memo',100,$8,1000000000,50000000,0,0);"
    echo "INSERT INTO fee_payer_history (pubkey, aud, cluster, created_at) VALUES ('$7','$1','$4',0);"
  }
  {
    row test-client PA active devnet sapphire_devnet http://localhost:5173 "$PK_A" "$1"
    row client-b PB active devnet sapphire_devnet http://localhost:5174 "$PK_B" "$2"
    row client-m PM active mainnet sapphire_mainnet http://localhost:5175 "$PK_M" 1000
    row client-p PP paused devnet sapphire_devnet http://localhost:5176 "$PK_P" 1000
    row client-x PX active devnet sapphire_devnet http://localhost:5177 "$PK_X" 1000
    row client-y PY active devnet sapphire_devnet http://localhost:5178 "$PK_Y" 1000
    echo "INSERT INTO settings (key, value) VALUES ('mainnet_enabled', '$3');"
  } > "$WORK/seed.sql"
}

run_suite() {
  local suite=$1 rpm_a=$2 rpm_b=$3 mainnet=$4; shift 4
  local persist="$WORK/state-$suite"
  npx wrangler d1 migrations apply gasless-relayer-audit --local --persist-to "$persist" >/dev/null 2>&1
  seed "$rpm_a" "$rpm_b" "$mainnet"
  npx wrangler d1 execute gasless-relayer-audit --local --persist-to "$persist" --file "$WORK/seed.sql" >/dev/null 2>&1
  local log="$WORK/wrangler-$suite.log"
  npx wrangler dev --port $PORT --ip 127.0.0.1 --persist-to "$persist" "${COMMON[@]}" "$@" \
    > "$log" 2>&1 &
  local pid=$!
  until grep -qE "Ready on|ERROR" "$log"; do sleep 0.3; done
  local rc=0
  PERSIST_DIR="$persist" WRANGLER_LOG="$log" RELAYER_URL="http://127.0.0.1:$PORT" MOCK_URL="$MOCK" \
    PK_A="$PK_A" PK_B="$PK_B" PK_M="$PK_M" PK_X="$PK_X" PK_Y="$PK_Y" \
    node tests/e2e.mjs "$suite" || rc=$?
  kill $pid 2>/dev/null; wait $pid 2>/dev/null || true
  # Reset mock behavior between suites.
  curl -s -X POST "$MOCK/__state" \
    -d '{"genesis":"EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG","genesisMainnet":"5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d","balances":{},"balance":10000000000,"balanceMode":"ok","balanceDelayMs":0,"jwksMode":"ok","send":"ok","delayMs":0}' >/dev/null
  return $rc
}

# Usage: ./tests/run-e2e.sh [A] [B] [C]   (default: all suites)
SUITES=${*:-A B C}
status=0
for s in $SUITES; do
  case $s in
    A) run_suite A 1000 1000 false --var COOLDOWN_MS:2000 --var REJECT_COOLDOWN_MS:1000 || status=1 ;;
    B) run_suite B 3 3 false --var COOLDOWN_MS:5000 --var CLUSTER_RPM_DEVNET:5 || status=1 ;;
    C) run_suite C 1000 1000 false --var COOLDOWN_MS:2000 \
         --var REGISTRY_CACHE_MS:1000 --var REGISTRY_MAX_STALE_MS:4000 || status=1 ;;
    *) echo "unknown suite $s"; status=1 ;;
  esac
done
exit $status
