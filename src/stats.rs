//! Process-global extract outcome counters (`stats.go`).
//!
//! The design of byblos rests on the premise that a page which is not a single raster is rare.
//! These counters are how that premise is checked against reality. Every attempt increments
//! exactly one of `extracted`, `diverted` and `failed`, so those three always sum to
//! `attempted`. The state is telemetry, not a resource: it is bounded by the divert-reason
//! vocabulary and does not grow with the input.
//!
//! The counters sit behind one mutex, as in Go, so a snapshot is consistent (the sum above
//! holds in every snapshot even while other threads count).

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// A snapshot of `extract_page_raster` and `record_extraction` outcomes since process start, or
/// since the last [`reset_extract_stats`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractCounters {
    pub attempted: u64,
    pub extracted: u64,
    /// Extracted pages whose raster does not fill the page box. A subset of `extracted`, not a
    /// fourth outcome.
    pub partial: u64,
    /// Page understood, but not a single raster.
    pub diverted: u64,
    /// Could not be read at all: damaged file, missing page.
    pub failed: u64,
    /// Divert reason to count; see `classify` in `extract.go`.
    pub reasons: BTreeMap<String, u64>,
}

impl ExtractCounters {
    /// The fraction of attempted pages that diverted. Failures are excluded from the numerator
    /// but not the denominator: a document byblos could not read is a different problem from
    /// one it read and declined. Watch [`ExtractCounters::unhandled_rate`] first.
    pub fn divert_rate(&self) -> f64 {
        if self.attempted == 0 {
            return 0.0;
        }
        self.diverted as f64 / self.attempted as f64
    }

    /// The fraction of attempted pages that produced no raster, whether byblos declined them or
    /// could not read them at all. It has no blind spot by construction.
    pub fn unhandled_rate(&self) -> f64 {
        if self.attempted == 0 {
            return 0.0;
        }
        (self.diverted + self.failed) as f64 / self.attempted as f64
    }
}

static STATS: Mutex<ExtractCounters> = Mutex::new(ExtractCounters {
    attempted: 0,
    extracted: 0,
    partial: 0,
    diverted: 0,
    failed: 0,
    reasons: BTreeMap::new(),
});

fn lock() -> MutexGuard<'static, ExtractCounters> {
    // A panic while counting cannot leave the counters inconsistent: every update is one
    // increment per field.
    STATS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `ExtractStats`: a snapshot. The `reasons` map is a copy.
pub fn extract_stats() -> ExtractCounters {
    lock().clone()
}

/// `ResetExtractStats`: zeroes every counter.
pub fn reset_extract_stats() {
    *lock() = ExtractCounters::default();
}

/// `countAttempt`.
pub fn count_attempt() {
    lock().attempted += 1;
}

/// `countExtracted`: `covers_page` false also counts a partial.
pub fn count_extracted(covers_page: bool) {
    let mut s = lock();
    s.extracted += 1;
    if !covers_page {
        s.partial += 1;
    }
}

/// `countFailure`.
pub fn count_failure() {
    lock().failed += 1;
}

/// `countDivert`.
pub fn count_divert(reason: &str) {
    let mut s = lock();
    s.diverted += 1;
    *s.reasons.entry(reason.to_string()).or_insert(0) += 1;
}

/// Serializes the tests, here and in `extract`, that touch the process-global counters: cargo
/// runs tests in parallel and Go's package tests do not.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    // One test, because the counters are process-global and tests run in parallel.
    #[test]
    fn counters_and_rates() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        reset_extract_stats();
        assert_eq!(extract_stats(), ExtractCounters::default());
        assert_eq!(extract_stats().divert_rate(), 0.0);
        assert_eq!(extract_stats().unhandled_rate(), 0.0);

        for _ in 0..4 {
            count_attempt();
        }
        count_extracted(true);
        count_extracted(false);
        count_divert("vector-paint");
        count_failure();
        let s = extract_stats();
        assert_eq!(s.attempted, 4);
        assert_eq!(s.extracted, 2);
        assert_eq!(s.partial, 1);
        assert_eq!(s.diverted, 1);
        assert_eq!(s.failed, 1);
        assert_eq!(s.extracted + s.diverted + s.failed, s.attempted);
        assert_eq!(s.reasons.get("vector-paint"), Some(&1));
        assert_eq!(s.divert_rate(), 0.25);
        assert_eq!(s.unhandled_rate(), 0.5);

        // The snapshot is a copy.
        count_divert("vector-paint");
        assert_eq!(s.reasons.get("vector-paint"), Some(&1));
        assert_eq!(extract_stats().reasons.get("vector-paint"), Some(&2));

        reset_extract_stats();
        assert_eq!(extract_stats(), ExtractCounters::default());
    }
}
