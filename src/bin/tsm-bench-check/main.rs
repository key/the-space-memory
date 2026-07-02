//! Thin CLI shell for the embedder-call-count regression gate.
//!
//! All parsing/diff logic lives in `the_space_memory::bench_baseline`
//! (unit-tested there); this binary only does file I/O, formatting, and
//! process exit codes — it has no dependency on the `bench-counters`
//! feature and builds as part of the default `cargo build`.
//!
//! Usage:
//!   tsm-bench-check <baseline.json> <current.json>
//!
//! Exit codes:
//!   0 — no regression, or a genuine bootstrap (baseline.json doesn't exist yet)
//!   1 — one or more embedder call counts regressed
//!   2 — usage error, or a baseline/current file that exists but fails to parse

use std::path::Path;
use std::process::ExitCode;

use the_space_memory::bench_baseline::{
    check_embedder_calls, parse_baseline, Baseline, BaselineState,
};

fn read_if_exists(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("error: failed to read {}: {e}", path.display());
            std::process::exit(2);
        }
    }
}

fn report_recorded_only(baseline: &Baseline, current: &Baseline) {
    println!("--- recorded, not gated (trend visibility only) ---");
    println!(
        "indexing.full_throughput.total_seconds: baseline={:.3} current={:.3}",
        baseline.indexing.full_throughput.total_seconds,
        current.indexing.full_throughput.total_seconds
    );
    println!(
        "indexing.incremental_latency_seconds.mean: baseline={:.3} current={:.3}",
        baseline.indexing.incremental_latency_seconds.mean,
        current.indexing.incremental_latency_seconds.mean
    );
    println!(
        "search.hybrid_ms.p50: baseline={:.1} current={:.1}",
        baseline.search.hybrid_ms.p50, current.search.hybrid_ms.p50
    );
    println!(
        "search.hybrid_ms.p95: baseline={:.1} current={:.1}",
        baseline.search.hybrid_ms.p95, current.search.hybrid_ms.p95
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (baseline_path, current_path) = match args.as_slice() {
        [_, b, c] => (b.clone(), c.clone()),
        _ => {
            eprintln!("usage: tsm-bench-check <baseline.json> <current.json>");
            return ExitCode::from(2);
        }
    };

    let current_raw = read_if_exists(Path::new(&current_path));
    let current = match current_raw {
        None => {
            eprintln!("error: current run file not found: {current_path}");
            return ExitCode::from(2);
        }
        Some(s) => match parse_baseline(Some(&s)) {
            BaselineState::Loaded(b) => *b,
            BaselineState::Invalid(e) => {
                eprintln!("error: {current_path} failed to parse: {e}");
                return ExitCode::from(2);
            }
            BaselineState::Bootstrap => unreachable!("Some(_) input never yields Bootstrap"),
        },
    };

    let baseline_raw = read_if_exists(Path::new(&baseline_path));
    match parse_baseline(baseline_raw.as_deref()) {
        BaselineState::Bootstrap => {
            println!(
                "BOOTSTRAP: {baseline_path} does not exist yet — nothing to gate against. \
                 This run's numbers become the baseline once the nightly job commits them."
            );
            ExitCode::SUCCESS
        }
        BaselineState::Invalid(e) => {
            eprintln!("error: {baseline_path} exists but failed to parse: {e}");
            ExitCode::from(2)
        }
        BaselineState::Loaded(baseline) => {
            report_recorded_only(&baseline, &current);

            let mismatches =
                check_embedder_calls(&baseline.embedder_calls, &current.embedder_calls);
            if mismatches.is_empty() {
                println!("PASS: embedder call counts match baseline (the only gated metric).");
                ExitCode::SUCCESS
            } else {
                println!("FAIL: embedder call count regression detected:");
                for m in &mismatches {
                    println!(
                        "  {} — baseline: {}, current: {}",
                        m.metric, m.baseline, m.current
                    );
                }
                ExitCode::FAILURE
            }
        }
    }
}
