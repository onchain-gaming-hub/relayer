-- Program actions (spec v3 §3, W9, W10, V14). Run on remote while the affected projects are
-- paused: relay_log is rebuilt (audit inserts are best-effort, so only a few rows may be lost).

ALTER TABLE projects ADD COLUMN project_daily_rent_budget_lamports INTEGER NOT NULL DEFAULT 0
  CHECK (project_daily_rent_budget_lamports >= 0);
ALTER TABLE projects ADD COLUMN cooldown_ms INTEGER CHECK (cooldown_ms IS NULL OR cooldown_ms >= 0);
ALTER TABLE projects ADD COLUMN reject_cooldown_ms INTEGER CHECK (reject_cooldown_ms IS NULL OR reject_cooldown_ms >= 0);
ALTER TABLE projects ADD COLUMN reject_cooldown_max_ms INTEGER CHECK (reject_cooldown_max_ms IS NULL OR reject_cooldown_max_ms >= 0);
ALTER TABLE projects ADD COLUMN retry_cooldown_ms INTEGER CHECK (retry_cooldown_ms IS NULL OR retry_cooldown_ms >= 0);

-- relay_log: allow status 'sweep' (cron sweep, W13) and record simulated rent (W8).
-- SQLite cannot change a CHECK in place: copy into a new table, keep every row and id.
CREATE TABLE relay_log_v3 (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  wallet_address TEXT    NOT NULL,
  tx_signature   TEXT    NOT NULL,
  status         TEXT    NOT NULL CHECK (status IN ('success', 'rpc_error', 'sweep')),
  reason         TEXT,
  created_at     INTEGER NOT NULL,
  aud            TEXT,
  cluster        TEXT,
  cost_lamports  INTEGER,
  rent_lamports  INTEGER
);
INSERT INTO relay_log_v3 (id, wallet_address, tx_signature, status, reason, created_at, aud, cluster, cost_lamports)
  SELECT id, wallet_address, tx_signature, status, reason, created_at, aud, cluster, cost_lamports FROM relay_log;
DROP TABLE relay_log;
ALTER TABLE relay_log_v3 RENAME TO relay_log;
CREATE INDEX IF NOT EXISTS idx_relay_log_wallet ON relay_log (wallet_address, created_at);
CREATE INDEX IF NOT EXISTS idx_relay_log_aud ON relay_log (aud, created_at);
