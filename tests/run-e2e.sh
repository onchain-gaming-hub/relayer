#!/usr/bin/env bash
# Runs the local e2e suites (spec §7) against wrangler dev + mock upstream.
# No network access needed; never touches devnet or any remote Cloudflare resource.
set -euo pipefail
cd "$(dirname "$0")/.."

WORK=$(mktemp -d)
PORT=8788
MOCK_PORT=8899
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$WORK"' EXIT

cargo build -q --example mktx
worker-build --release >/dev/null

MOCK_PORT=$MOCK_PORT node tests/mock-upstream.mjs > "$WORK/mock.log" 2>&1 &
until grep -q ready "$WORK/mock.log"; do sleep 0.2; done

COMMON=(
  --var "RPC_URL:http://127.0.0.1:$MOCK_PORT"
  --var "WEB3AUTH_JWKS_URL:http://127.0.0.1:$MOCK_PORT/jwks"
  --var "WEB3AUTH_CLIENT_ID:test-client"
  --var "WEB3AUTH_ISSUER:https://issuer.test"
  --var "RPC_TIMEOUT_MS:2000"
)

run_suite() {
  local suite=$1 schema=$2; shift 2
  local persist="$WORK/state-$suite"
  if [ "$schema" = yes ]; then
    npx wrangler d1 execute gasless-relayer-audit --local --persist-to "$persist" --file schema.sql >/dev/null 2>&1
  fi
  npx wrangler dev --port $PORT --ip 127.0.0.1 --persist-to "$persist" "${COMMON[@]}" "$@" \
    > "$WORK/wrangler-$suite.log" 2>&1 &
  local pid=$!
  until grep -qE "Ready on|ERROR" "$WORK/wrangler-$suite.log"; do sleep 0.3; done
  local rc=0
  PERSIST_DIR="$persist" RELAYER_URL="http://127.0.0.1:$PORT" MOCK_URL="http://127.0.0.1:$MOCK_PORT" \
    node tests/e2e.mjs "$suite" || rc=$?
  kill $pid 2>/dev/null; wait $pid 2>/dev/null || true
  # Reset mock behavior between suites.
  curl -s -X POST "http://127.0.0.1:$MOCK_PORT/__state" \
    -d '{"genesis":"EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG","balance":10000000000,"balanceMode":"ok","send":"ok","delayMs":0}' >/dev/null
  return $rc
}

status=0
run_suite A yes --var COOLDOWN_MS:2000 --var REJECT_COOLDOWN_MS:1000 --var GLOBAL_RPM:1000 || status=1
run_suite B yes --var COOLDOWN_MS:5000 --var GLOBAL_RPM:3 || status=1
run_suite C no  --var COOLDOWN_MS:2000 || status=1
exit $status
