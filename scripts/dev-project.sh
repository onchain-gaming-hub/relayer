#!/usr/bin/env bash
# LOCAL ONLY: registers (or re-activates) a devnet project in the local D1 used by `wrangler dev`.
# Production projects are added by hand per spec v2 §10 (a hub admin runs remote D1 commands).
# Usage: scripts/dev-project.sh <clientId> <SLUG> <fee_payer_pubkey> <origin> [user_daily_quota]
#   The secret FEE_PAYER_<SLUG> must already be in .dev.vars (cargo run -q --example keygen -- <SLUG>).
set -euo pipefail
cd "$(dirname "$0")/.."
[ $# -ge 4 ] || { sed -n '2,6p' "$0"; exit 2; }
AUD=$1 SLUG=$2 PK=$3 ORIGIN=$4 QUOTA=${5:-100}
for v in "$AUD" "$SLUG" "$PK" "$ORIGIN"; do
  [[ $v =~ ^[A-Za-z0-9_:/.-]+$ ]] || { echo "refusing unexpected characters in: $v"; exit 2; }
done
NOW=$(($(date +%s) * 1000))
POLICY='{"actions":[{"type":"memo","max_bytes":128,"prefix":"gasless-poc:"}]}'
npx wrangler d1 migrations apply gasless-relayer-audit --local
npx wrangler d1 execute gasless-relayer-audit --local --command "
-- Fails loudly (PRIMARY KEY) if this key was ever registered for another project (V3).
INSERT INTO fee_payer_history (pubkey, aud, cluster, created_at)
  SELECT '$PK', '$AUD', 'devnet', $NOW
  WHERE NOT EXISTS (SELECT 1 FROM fee_payer_history WHERE pubkey = '$PK' AND aud = '$AUD');
INSERT INTO projects (aud, slug, name, status, cluster, web3auth_network, allowed_origins, fee_payer_pubkey, policy,
  user_daily_quota, project_rpm, project_daily_budget_lamports, min_balance_lamports, created_at, updated_at)
VALUES ('$AUD', '$SLUG', '$SLUG', 'active', 'devnet', 'sapphire_devnet', '[\"$ORIGIN\"]', '$PK', '$POLICY',
  $QUOTA, 60, 1000000000, 50000000, $NOW, $NOW)
ON CONFLICT(aud) DO UPDATE SET status = 'active', slug = excluded.slug, fee_payer_pubkey = excluded.fee_payer_pubkey,
  allowed_origins = excluded.allowed_origins, updated_at = excluded.updated_at;"
