//! Fixed one-minute window counter kept in memory (spec v2 V8: an eviction resets it, which
//! costs at most one window; money is bounded by the persistent daily budgets). Single-threaded:
//! callers check-and-take synchronously before their first `await`.

use std::cell::Cell;

pub const WINDOW_MS: f64 = 60_000.0;

#[derive(Default)]
pub struct FixedWindow {
    start: Cell<f64>,
    count: Cell<u32>,
}

impl FixedWindow {
    /// Starts a new window when the current one has ended. Call before `take` / `release`.
    pub fn roll(&self, now: f64) {
        if now - self.start.get() >= WINDOW_MS {
            self.start.set(now);
            self.count.set(0);
        }
    }

    /// Takes one slot; returns the window it was counted in (needed to refund it), or the
    /// milliseconds until the window ends when it is full.
    pub fn take(&self, limit: u32, now: f64) -> Result<f64, f64> {
        self.roll(now);
        if self.count.get() >= limit {
            return Err(self.start.get() + WINDOW_MS - now);
        }
        self.count.set(self.count.get() + 1);
        Ok(self.start.get())
    }

    /// Gives back one slot, but only in the window it was taken from and never below zero.
    pub fn release(&self, window: f64) -> bool {
        let ok = window == self.start.get() && self.count.get() > 0;
        if ok {
            self.count.set(self.count.get() - 1);
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_until_full_then_reports_time_left() {
        let w = FixedWindow::default();
        let win = w.take(2, 1_000_000.0).unwrap();
        assert_eq!(w.take(2, 1_000_001.0), Ok(win));
        assert_eq!(w.take(2, 1_010_000.0), Err(50_000.0));
        assert!(w.release(win));
        assert_eq!(w.take(2, 1_010_000.0), Ok(win));
    }

    #[test]
    fn refund_only_within_its_window_and_never_below_zero() {
        let w = FixedWindow::default();
        let old = w.take(5, 1_000_000.0).unwrap();
        assert!(w.release(old));
        assert!(!w.release(old), "count is already 0");
        w.take(5, 1_000_000.0).unwrap();
        w.roll(1_000_000.0 + WINDOW_MS);
        assert!(!w.release(old), "window rolled over");
    }
}
