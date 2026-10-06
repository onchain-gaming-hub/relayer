//! Persistent per-UTC-day counter in a Durable Object's SQLite (spec v2 V6, V8).
//!
//! `SqlStorage::exec` is synchronous (worker 0.8.7 `sql.rs`), so `try_add` is one atomic
//! check-and-increment: no input-gate or microtask-ordering argument is needed, and the value
//! survives eviction. Each DO has its own database, so one table name serves every DO class.
//! Used for: user tx count (cost 1), project and cluster fee budgets (cost in lamports).

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

#[derive(serde::Deserialize)]
struct Spent {
    spent: i64,
}

fn int(v: i64) -> SqlStorageValue {
    SqlStorageValue::Integer(v)
}

pub fn ensure_table(sql: &SqlStorage) -> worker::Result<()> {
    sql.exec(
        "CREATE TABLE IF NOT EXISTS daily_counter (day INTEGER PRIMARY KEY, spent INTEGER NOT NULL)",
        None,
    )
    .map(|_| ())
}

/// Adds `cost` to `day` if the total stays within `limit`. `Ok(Some(total))` = counted,
/// `Ok(None)` = over the limit (nothing changed).
pub fn try_add(sql: &SqlStorage, day: i64, cost: i64, limit: i64) -> worker::Result<Option<i64>> {
    ensure_table(sql)?;
    sql.exec(
        "INSERT INTO daily_counter (day, spent) VALUES (?, 0) ON CONFLICT(day) DO NOTHING",
        vec![int(day)],
    )?;
    // Old days are only history (relay_log keeps the real record).
    sql.exec("DELETE FROM daily_counter WHERE day < ?", vec![int(day)])?;
    let rows: Vec<Spent> = sql
        .exec(
            "UPDATE daily_counter SET spent = spent + ? WHERE day = ? AND spent + ? <= ? RETURNING spent",
            vec![int(cost), int(day), int(cost), int(limit)],
        )?
        .to_array()?;
    Ok(rows.first().map(|r| r.spent))
}

/// Gives `cost` back to `day` (never below zero). A refund for a past day is a no-op.
pub fn refund(sql: &SqlStorage, day: i64, cost: i64) -> worker::Result<()> {
    ensure_table(sql)?;
    sql.exec(
        "UPDATE daily_counter SET spent = MAX(0, spent - ?) WHERE day = ?",
        vec![int(cost), int(day)],
    )
    .map(|_| ())
}

/// What `day` has used so far.
pub fn used(sql: &SqlStorage, day: i64) -> worker::Result<i64> {
    ensure_table(sql)?;
    let rows: Vec<Spent> = sql
        .exec(
            "SELECT spent FROM daily_counter WHERE day = ?",
            vec![int(day)],
        )?
        .to_array()?;
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
