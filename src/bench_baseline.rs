//! Pure parsing/diff/threshold logic for the performance baseline
//! (`benches/baseline.json`).
//!
//! Only `embedder_calls` is regression-gated (exact equality — the call
//! count is deterministic for a fixed corpus and batching strategy).
//! `indexing` and `search` are recorded for trend visibility only: on the
//! standard e2e testdata corpus (5 files, ~20KB), wall-clock latency swung
//! 15-49% between two consecutive runs on an otherwise idle machine with no
//! code changes, and incremental single-file reindex latency decayed ~3.4x
//! over 5 consecutive identical runs (warm-up transient, not signal) — no
//! percentage threshold on this corpus size would separate real regressions
//! from noise.
//!
//! The I/O shells (reading/writing JSON, running the actual benches, wiring
//! the embedder-socket counters) live in `benches/record_metrics.rs` and
//! `src/bin/tsm-bench-check/main.rs`. This module only turns already-parsed
//! values into decisions, so every branch is exercisable as a pure value.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Baseline {
    pub schema_version: String,
    pub env: String,
    pub embedder: String,
    pub corpus: String,
    pub indexing: IndexingMetrics,
    pub search: SearchMetrics,
    pub embedder_calls: EmbedderCallCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct IndexingMetrics {
    pub full_throughput: FullThroughput,
    pub incremental_latency_seconds: IncrementalLatency,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FullThroughput {
    pub total_seconds: f64,
    pub files_per_sec: f64,
    pub chunks_per_sec: f64,
    pub breakdown: StageBreakdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct StageBreakdown {
    pub prepare_seconds: f64,
    pub embed_seconds: f64,
    pub persist_seconds: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct IncrementalLatency {
    pub mean: f64,
    pub samples: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SearchMetrics {
    pub fts5_only_ms: LatencyStats,
    pub hybrid_ms: LatencyStats,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LatencyStats {
    pub p50: f64,
    pub p95: f64,
    pub by_query: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct EmbedderCallCounts {
    pub full_index: u64,
    pub single_query_hybrid: u64,
}

/// Result of attempting to load `baseline.json`. A missing file is a
/// distinct, explicitly-reported state ("bootstrap") rather than folded
/// into an error or silently treated as "no regression" — the gate shell
/// (`tsm-bench-check`) must print BOOTSTRAP and exit 0, not pass silently
/// with no output.
#[derive(Debug)]
pub enum BaselineState {
    /// No baseline.json yet — first CI run on this environment.
    Bootstrap,
    /// Parsed successfully.
    Loaded(Box<Baseline>),
    /// File exists but isn't valid JSON / doesn't match the schema.
    Invalid(String),
}

/// `raw` is `None` when the file does not exist (the shell's job to
/// determine via a filesystem check); `Some(contents)` otherwise. Keeping
/// file I/O out of this function is what makes it unit-testable without a
/// filesystem.
pub fn parse_baseline(raw: Option<&str>) -> BaselineState {
    match raw {
        None => BaselineState::Bootstrap,
        Some(s) => match serde_json::from_str::<Baseline>(s) {
            Ok(b) => BaselineState::Loaded(Box::new(b)),
            Err(e) => BaselineState::Invalid(e.to_string()),
        },
    }
}

/// One embedder-call-count metric that differs between baseline and current.
#[derive(Debug, Clone, PartialEq)]
pub struct CallCountMismatch {
    pub metric: &'static str,
    pub baseline: u64,
    pub current: u64,
}

/// Exact-equality regression check for embedder call counts — the only
/// gated metric (see module docs for why `indexing`/`search` are
/// record-only). Deterministic for a fixed corpus and batching strategy, so
/// any deviation, not just "more calls", is reported: an unexpected drop
/// means behavior changed too (e.g. a batch silently short-circuiting).
pub fn check_embedder_calls(
    baseline: &EmbedderCallCounts,
    current: &EmbedderCallCounts,
) -> Vec<CallCountMismatch> {
    let mut mismatches = Vec::new();
    if baseline.full_index != current.full_index {
        mismatches.push(CallCountMismatch {
            metric: "embedder_calls.full_index",
            baseline: baseline.full_index,
            current: current.full_index,
        });
    }
    if baseline.single_query_hybrid != current.single_query_hybrid {
        mismatches.push(CallCountMismatch {
            metric: "embedder_calls.single_query_hybrid",
            baseline: baseline.single_query_hybrid,
            current: current.single_query_hybrid,
        });
    }
    mismatches
}

/// Discards the first `warmup` samples (warm-up transient) before averaging
/// the rest. Empirically, incremental single-file reindex latency decayed
/// ~3.4x over 5 consecutive identical runs on the reference machine
/// (1144.7ms -> 340.1ms); `warmup = 2` trims the steepest part of that curve
/// (iteration 1->2: -21.7%, 2->3: -38.8%) while keeping the bench cheap
/// enough for CI. This does not prove convergence by iteration 3 — which is
/// exactly why these metrics stay record-only rather than gated.
pub fn trimmed_mean(samples: &[f64], warmup: usize) -> Option<f64> {
    if samples.len() <= warmup {
        return None;
    }
    let kept = &samples[warmup..];
    Some(kept.iter().sum::<f64>() / kept.len() as f64)
}

/// Nearest-rank percentile over an already-sorted ascending slice. `pct` is
/// in `[0.0, 100.0]`. Returns `0.0` for an empty slice.
pub fn percentile(sorted_samples: &[f64], pct: f64) -> f64 {
    if sorted_samples.is_empty() {
        return 0.0;
    }
    let last = sorted_samples.len() - 1;
    let rank = ((pct / 100.0) * last as f64).round() as usize;
    sorted_samples[rank.min(last)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_counts(full_index: u64, single_query_hybrid: u64) -> EmbedderCallCounts {
        EmbedderCallCounts {
            full_index,
            single_query_hybrid,
        }
    }

    #[test]
    fn parse_baseline_missing_file_is_bootstrap() {
        assert!(matches!(parse_baseline(None), BaselineState::Bootstrap));
    }

    #[test]
    fn parse_baseline_invalid_json_is_invalid() {
        let state = parse_baseline(Some("not json"));
        assert!(matches!(state, BaselineState::Invalid(_)));
    }

    #[test]
    fn parse_baseline_valid_json_loads() {
        let json = r#"{
            "schema_version": "1",
            "env": "ubuntu-latest-cpu",
            "embedder": "ruri-v3-30m-fp32",
            "corpus": "tests/e2e/testdata/notes",
            "indexing": {
                "full_throughput": {
                    "total_seconds": 1.0,
                    "files_per_sec": 5.0,
                    "chunks_per_sec": 28.0,
                    "breakdown": { "prepare_seconds": 0.1, "embed_seconds": 0.8, "persist_seconds": 0.1 }
                },
                "incremental_latency_seconds": { "mean": 0.3, "samples": [0.3, 0.31] }
            },
            "search": {
                "fts5_only_ms": { "p50": 1.0, "p95": 2.0, "by_query": {} },
                "hybrid_ms": { "p50": 100.0, "p95": 200.0, "by_query": {"猫": 150.0} }
            },
            "embedder_calls": { "full_index": 1, "single_query_hybrid": 1 }
        }"#;
        let state = parse_baseline(Some(json));
        match state {
            BaselineState::Loaded(b) => {
                assert_eq!(b.schema_version, "1");
                assert_eq!(b.embedder_calls.full_index, 1);
                assert_eq!(b.search.hybrid_ms.by_query.get("猫"), Some(&150.0));
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    #[test]
    fn check_embedder_calls_no_mismatch_when_equal() {
        let baseline = sample_counts(1, 2);
        let current = sample_counts(1, 2);
        assert_eq!(check_embedder_calls(&baseline, &current), vec![]);
    }

    #[test]
    fn check_embedder_calls_reports_full_index_mismatch() {
        let baseline = sample_counts(1, 2);
        let current = sample_counts(3, 2);
        let mismatches = check_embedder_calls(&baseline, &current);
        assert_eq!(
            mismatches,
            vec![CallCountMismatch {
                metric: "embedder_calls.full_index",
                baseline: 1,
                current: 3,
            }]
        );
    }

    #[test]
    fn check_embedder_calls_reports_single_query_mismatch() {
        let baseline = sample_counts(1, 2);
        let current = sample_counts(1, 5);
        let mismatches = check_embedder_calls(&baseline, &current);
        assert_eq!(
            mismatches,
            vec![CallCountMismatch {
                metric: "embedder_calls.single_query_hybrid",
                baseline: 2,
                current: 5,
            }]
        );
    }

    #[test]
    fn check_embedder_calls_reports_both_mismatches_including_decreases() {
        let baseline = sample_counts(5, 5);
        let current = sample_counts(3, 3);
        let mismatches = check_embedder_calls(&baseline, &current);
        assert_eq!(mismatches.len(), 2);
    }

    #[test]
    fn trimmed_mean_discards_warmup_samples() {
        let samples = [1144.7, 896.3, 548.7, 495.3, 340.1];
        let mean = trimmed_mean(&samples, 2).unwrap();
        let expected = (548.7 + 495.3 + 340.1) / 3.0;
        assert!((mean - expected).abs() < 1e-9);
    }

    #[test]
    fn trimmed_mean_none_when_not_enough_samples() {
        let samples = [1.0, 2.0];
        assert_eq!(trimmed_mean(&samples, 2), None);
    }

    #[test]
    fn trimmed_mean_none_when_exactly_warmup_count() {
        let samples = [1.0, 2.0, 3.0];
        assert_eq!(trimmed_mean(&samples, 3), None);
    }

    #[test]
    fn percentile_empty_slice_is_zero() {
        assert_eq!(percentile(&[], 50.0), 0.0);
    }

    #[test]
    fn percentile_p50_of_single_value() {
        assert_eq!(percentile(&[42.0], 50.0), 42.0);
    }

    #[test]
    fn percentile_p50_and_p95_of_ten_values() {
        let sorted: Vec<f64> = (1..=10).map(|n| n as f64).collect();
        // Nearest-rank on indices 0..9: p50 -> round(0.5*9)=5 -> value 6.0;
        // p95 -> round(0.95*9)=9 (clamped) -> value 10.0.
        assert_eq!(percentile(&sorted, 50.0), 6.0);
        assert_eq!(percentile(&sorted, 95.0), 10.0);
    }

    #[test]
    fn baseline_round_trips_through_serde() {
        let baseline = Baseline {
            schema_version: "1".into(),
            env: "ubuntu-latest-cpu".into(),
            embedder: "ruri-v3-30m-fp32".into(),
            corpus: "tests/e2e/testdata/notes".into(),
            indexing: IndexingMetrics::default(),
            search: SearchMetrics::default(),
            embedder_calls: sample_counts(1, 1),
        };
        let json = serde_json::to_string(&baseline).unwrap();
        let parsed = parse_baseline(Some(&json));
        match parsed {
            BaselineState::Loaded(b) => assert_eq!(*b, baseline),
            other => panic!("expected Loaded, got {other:?}"),
        }
    }
}
