//! Per-key log-message cooldown.
//!
//! A repeated warning about the same underlying cause (e.g. a directory tree
//! of policy-rejected files) should log once, not once per event. `LogDedup`
//! is a generic gate for that: the first occurrence of a key always reports;
//! further occurrences within a cooldown window are suppressed and counted,
//! and the count is surfaced the next time that key logs.
//!
//! This is plain library code, not tied to any one call site or process — it
//! is not `pub(crate)` inside a single binary because more than one caller
//! across the `tsm`/`tsmd` process tree needs it (a daemon-side policy
//! rejection warning and, potentially, a watcher-side one). Each caller picks
//! its own key granularity (e.g. a directory prefix rather than a full file
//! path) to keep the noise reduction meaningful; see the doc comment at each
//! call site for that choice.
//!
//! The internal map grows one entry per distinct key ever seen and is never
//! pruned. This is an accepted bound, matching the fs-watcher's `Debounce`
//! map: callers are expected to key by something coarse (a directory, not a
//! file), so the map stays small relative to event volume in practice.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A per-key cooldown gate for log messages.
pub struct LogDedup {
    window: Duration,
    /// key -> (last time this key was allowed to log, occurrences
    /// suppressed since that log).
    seen: HashMap<String, (Instant, u64)>,
}

impl LogDedup {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            seen: HashMap::new(),
        }
    }

    /// Record an occurrence of `key` at `now`.
    ///
    /// Returns `Some(suppressed)` if this occurrence should be logged now —
    /// `suppressed` is the number of prior occurrences swallowed since this
    /// key last logged (`0` for a key's first occurrence, or for one that
    /// logs again after a gap with nothing suppressed in between). Returns
    /// `None` if `key` last logged less than `window` ago, in which case the
    /// occurrence is counted but nothing should be printed.
    pub fn gate(&mut self, key: &str, now: Instant) -> Option<u64> {
        match self.seen.get_mut(key) {
            Some((last, suppressed)) if now.duration_since(*last) < self.window => {
                *suppressed += 1;
                None
            }
            Some((last, suppressed)) => {
                let carried = *suppressed;
                *last = now;
                *suppressed = 0;
                Some(carried)
            }
            None => {
                self.seen.insert(key.to_string(), (now, 0));
                Some(0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_occurrence_logs_with_zero_suppressed() {
        let mut d = LogDedup::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert_eq!(d.gate("target/debug", t0), Some(0));
    }

    #[test]
    fn test_second_occurrence_within_window_is_suppressed() {
        let mut d = LogDedup::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert_eq!(d.gate("target/debug", t0), Some(0));
        assert_eq!(d.gate("target/debug", t0 + Duration::from_secs(1)), None);
        assert_eq!(d.gate("target/debug", t0 + Duration::from_secs(2)), None);
    }

    #[test]
    fn test_occurrence_after_window_logs_with_carried_suppressed_count() {
        let mut d = LogDedup::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert_eq!(d.gate("target/debug", t0), Some(0));
        // Two suppressed while still within the window.
        assert_eq!(d.gate("target/debug", t0 + Duration::from_secs(10)), None);
        assert_eq!(d.gate("target/debug", t0 + Duration::from_secs(20)), None);
        // Past the window: logs again, reporting the two swallowed in between.
        assert_eq!(
            d.gate("target/debug", t0 + Duration::from_secs(61)),
            Some(2)
        );
    }

    #[test]
    fn test_cooldown_restarts_after_logging() {
        let mut d = LogDedup::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert_eq!(d.gate("target/debug", t0), Some(0));
        assert_eq!(
            d.gate("target/debug", t0 + Duration::from_secs(61)),
            Some(0)
        );
        // Immediately after logging again, the cooldown is back in effect.
        assert_eq!(d.gate("target/debug", t0 + Duration::from_secs(62)), None);
    }

    #[test]
    fn test_different_keys_are_independent() {
        let mut d = LogDedup::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert_eq!(d.gate("target/debug", t0), Some(0));
        // A different key is unaffected by the first key's cooldown.
        assert_eq!(
            d.gate("node_modules", t0 + Duration::from_millis(1)),
            Some(0)
        );
        assert_eq!(d.gate("target/debug", t0 + Duration::from_secs(1)), None);
    }
}
