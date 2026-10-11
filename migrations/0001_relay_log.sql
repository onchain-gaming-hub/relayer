-- Generic relay audit log (v1 spec §4.6, D13): success / rpc_error only, best-effort writes.
CREATE TABLE IF NOT EXISTS relay_log (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  wallet_address TEXT    NOT NULL,
  tx_signature   TEXT    NOT NULL,   -- slot 0 (relayer) signature = tx id
  status         TEXT    NOT NULL CHECK (status IN ('success', 'rpc_error')),
  reason         TEXT,               -- 'rejected: <msg>' | 'unknown: <msg>' | 'already processed' (status=success, replay)
  created_at     INTEGER NOT NULL    -- unix ms
);
CREATE INDEX IF NOT EXISTS idx_relay_log_wallet ON relay_log (wallet_address, created_at);
