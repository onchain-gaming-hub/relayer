-- Multi-project registry (spec v2 §4.5). Never DELETE rows: retire them (V13).
CREATE TABLE IF NOT EXISTS projects (
  aud                           TEXT PRIMARY KEY,          -- Web3Auth clientId
  slug                          TEXT NOT NULL UNIQUE CHECK (slug NOT GLOB '*[^A-Z0-9_]*' AND length(slug) BETWEEN 1 AND 32),
  name                          TEXT NOT NULL,
  status                        TEXT NOT NULL DEFAULT 'paused' CHECK (status IN ('active','paused','retired')),
  cluster                       TEXT NOT NULL CHECK (cluster IN ('devnet','mainnet')),
  web3auth_network              TEXT NOT NULL CHECK (web3auth_network IN ('sapphire_devnet','sapphire_mainnet')),
  allowed_origins               TEXT NOT NULL,             -- JSON array of exact origins (no '*')
  fee_payer_pubkey              TEXT NOT NULL UNIQUE,      -- secret lives in FEE_PAYER_<slug>
  policy                        TEXT NOT NULL,             -- JSON (V11)
  user_daily_quota              INTEGER NOT NULL CHECK (user_daily_quota >= 1),
  project_rpm                   INTEGER NOT NULL CHECK (project_rpm >= 1),
  project_daily_budget_lamports INTEGER NOT NULL CHECK (project_daily_budget_lamports >= 0),
  min_balance_lamports          INTEGER NOT NULL CHECK (min_balance_lamports >= 0),
  created_at                    INTEGER NOT NULL,
  updated_at                    INTEGER NOT NULL
);

-- Insert-only: a fee payer key can never be reused across projects or clusters (V3).
CREATE TABLE IF NOT EXISTS fee_payer_history (
  pubkey     TEXT PRIMARY KEY,
  aud        TEXT NOT NULL,
  cluster    TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS settings (
  key   TEXT PRIMARY KEY,                                  -- 'mainnet_enabled'
  value TEXT NOT NULL
);

ALTER TABLE relay_log ADD COLUMN aud TEXT;
ALTER TABLE relay_log ADD COLUMN cluster TEXT;
ALTER TABLE relay_log ADD COLUMN cost_lamports INTEGER;
CREATE INDEX IF NOT EXISTS idx_relay_log_aud ON relay_log (aud, created_at);
