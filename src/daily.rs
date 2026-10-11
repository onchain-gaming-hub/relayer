//! Persistent per-UTC-day counter in a Durable Object's SQLite (spec v2 V6, V8).
//!
//! `SqlStorage::exec` is synchronous (worker 0.8.7 `sql.rs`), so `try_add` is one atomic
//! check-and-increment: no input-gate or microtask-ordering argument is needed, and the value
//! survives eviction. Each DO has its own database, so one table name serves every DO class.
//! Used for: user tx count (kind `count`, cost 1), project and cluster fee / rent budgets
//! (kinds `fee` / `rent`, cost in lamports; spec v3 W9).

use worker::{SqlStorage, SqlStorageValue};

pub const DAY_MS: f64 = 86_400_000.0;

/// UTC day number (V6): `floor(now_ms / 86_400_000)`.
pub fn day_of(now_ms: f64) -> i64 {
    (now_ms / DAY_MS).floor() as i64
}

/// Milliseconds from `now_ms` to the next 00:00 UTC (when every daily counter resets).
pub fn ms_until_next_day(now_ms: f64) -> f64 {
    (day_of(now_ms) + 1) as f64 * DAY_MS - now_ms
}

/// Start of the next UTC day, unix ms.
pub fn next_day_start(now_ms: f64) -> f64 {
    (day_of(now_ms) + 1) as f64 * DAY_MS
}

/// Counter kinds (spec v3 W9): the user's tx count, and lamports of fee / rent.
pub const COUNT: &str = "count";
pub const FEE: &str = "fee";
pub const RENT: &str = "rent";

#[derive(serde::Deserialize)]
struct Spent {
    spent: i64,
}

#[derive(serde::Deserialize)]
struct Count {
    n: i64,
}

fn int(v: i64) -> SqlStorageValue {
    SqlStorageValue::Integer(v)
}

fn text(v: &str) -> SqlStorageValue {
    SqlStorageValue::String(v.to_string())
}

fn table_exists(sql: &SqlStorage, name: &str) -> worker::Result<bool> {
    let rows: Vec<Count> = sql
        .exec(
            "SELECT count(*) AS n FROM sqlite_master WHERE type = 'table' AND name = ?",
            vec![text(name)],
        )?
        .to_array()?;
    Ok(rows.first().map(|c| c.n).unwrap_or(0) > 0)
}

/// Creates `daily_counter_v2` on first use. A DO from before v3 (staging) already has
/// `daily_counter (day, spent)` made with CREATE TABLE IF NOT EXISTS, which cannot gain a
/// column: its rows are copied once as `legacy_kind` (UserController: count, throttles: fee)
/// so the deploy day's quota and budget do not reset. The old table is left as it was.
pub fn ensure_table(sql: &SqlStorage, legacy_kind: &str) -> worker::Result<()> {
    if table_exists(sql, "daily_counter_v2")? {
        return Ok(());
    }
    sql.exec(
        "CREATE TABLE daily_counter_v2 (day INTEGER NOT NULL, kind TEXT NOT NULL, spent INTEGER NOT NULL, PRIMARY KEY (day, kind))",
        None,
    )?;
    if table_exists(sql, "daily_counter")? {
        let copied = sql.exec(
            "INSERT OR IGNORE INTO daily_counter_v2 (day, kind, spent) SELECT day, ?, spent FROM daily_counter",
            vec![text(legacy_kind)],
        );
        if let Err(e) = copied {
            // Never leave an empty v2 table behind: its existence would skip the copy forever
            // and reset the deploy day's counters. The next call retries the whole migration.
            if let Err(d) = sql.exec("DROP TABLE daily_counter_v2", None) {
                worker::console_error!("daily_counter_v2 rollback failed: {d}");
            }
            return Err(e);
        }
    }
    Ok(())
}

/// Adds `cost` to (`day`, `kind`) if the total stays within `limit`. `Ok(Some(total))` =
/// counted, `Ok(None)` = over the limit (nothing changed).
pub fn try_add(
    sql: &SqlStorage,
    legacy_kind: &str,
    day: i64,
    kind: &str,
    cost: i64,
    limit: i64,
) -> worker::Result<Option<i64>> {
    ensure_table(sql, legacy_kind)?;
    sql.exec(
        "INSERT INTO daily_counter_v2 (day, kind, spent) VALUES (?, ?, 0) ON CONFLICT(day, kind) DO NOTHING",
        vec![int(day), text(kind)],
    )?;
    // Old days are only history (relay_log keeps the real record).
    sql.exec("DELETE FROM daily_counter_v2 WHERE day < ?", vec![int(day)])?;
    let rows: Vec<Spent> = sql
        .exec(
            "UPDATE daily_counter_v2 SET spent = spent + ? WHERE day = ? AND kind = ? AND spent + ? <= ? RETURNING spent",
            vec![int(cost), int(day), text(kind), int(cost), int(limit)],
        )?
        .to_array()?;
    Ok(rows.first().map(|r| r.spent))
}

/// Gives `cost` back to (`day`, `kind`) (never below zero). A refund for a past day is a no-op.
pub fn refund(
    sql: &SqlStorage,
    legacy_kind: &str,
    day: i64,
    kind: &str,
    cost: i64,
) -> worker::Result<()> {
    if cost == 0 {
        return Ok(());
    }
    ensure_table(sql, legacy_kind)?;
    sql.exec(
        "UPDATE daily_counter_v2 SET spent = MAX(0, spent - ?) WHERE day = ? AND kind = ?",
        vec![int(cost), int(day), text(kind)],
    )
    .map(|_| ())
}

/// What (`day`, `kind`) has used so far. Read-only: a DO that never counted anything stays
/// empty (no CREATE TABLE), so GET /quota never writes state (spec v2 §4.2). A DO not yet
/// migrated to v2 answers from the legacy table for `legacy_kind`.
pub fn used(sql: &SqlStorage, legacy_kind: &str, day: i64, kind: &str) -> worker::Result<i64> {
    let rows: Vec<Spent> = if table_exists(sql, "daily_counter_v2")? {
        sql.exec(
            "SELECT spent FROM daily_counter_v2 WHERE day = ? AND kind = ?",
            vec![int(day), text(kind)],
        )?
        .to_array()?
    } else if kind == legacy_kind && table_exists(sql, "daily_counter")? {
        sql.exec(
            "SELECT spent FROM daily_counter WHERE day = ?",
            vec![int(day)],
        )?
        .to_array()?
    } else {
        Vec::new()
    };
    Ok(rows.first().map(|r| r.spent).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_day_math() {
        // 2026-10-06T00:00:00Z = 1791244800000
        let midnight = 1_791_244_800_000.0;
        assert_eq!(day_of(midnight), 20_732);
        assert_eq!(day_of(midnight - 1.0), 20_731);
        assert_eq!(ms_until_next_day(midnight), DAY_MS);
        assert_eq!(ms_until_next_day(midnight + DAY_MS - 1.0), 1.0);
        assert_eq!(next_day_start(midnight + 5.0), midnight + DAY_MS);
    }
}
