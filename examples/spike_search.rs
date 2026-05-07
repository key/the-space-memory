//! Spike: in-process search latency CV measurement.
//!
//! Calls `searcher::search()` directly in a loop to isolate
//! search-algorithm variance from CLI/process-spawn overhead.
//! Requires tsmd running (for embedder query embedding).
//!
//! Usage:
//!   cargo run --release --example spike_search
//!
//! Output: JSON summary on stdout (CV, p50, p95, raw values).
//! Throwaway: this file will be deleted once the gate threshold is set.

use std::time::Instant;

use the_space_memory::{config, db, searcher};

const QUERY: &str = "銀河";
const TOP_K: usize = 5;
const N_ITER: usize = 100;
const WARMUP: usize = 10;

fn main() -> anyhow::Result<()> {
    let db_path = config::db_path();
    eprintln!("Using DB: {}", db_path.display());

    let conn = db::get_connection(&db_path)?;

    let mut times = Vec::with_capacity(N_ITER);
    for i in 0..N_ITER {
        let start = Instant::now();
        let _ = searcher::search(&conn, QUERY, TOP_K, None, false, None)?;
        let elapsed = start.elapsed().as_secs_f64();
        times.push(elapsed);
        if !(3..N_ITER - 3).contains(&i) {
            eprintln!("iter {}: {:.6}s", i, elapsed);
        }
    }

    let warm: Vec<f64> = times.iter().skip(WARMUP).copied().collect();
    let mean: f64 = warm.iter().sum::<f64>() / warm.len() as f64;
    let variance: f64 = warm.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / warm.len() as f64;
    let stdev = variance.sqrt();
    let cv = if mean > 0.0 {
        stdev / mean * 100.0
    } else {
        0.0
    };

    let mut sorted = warm.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = sorted[sorted.len() / 2];
    let p95 = sorted[(sorted.len() as f64 * 0.95) as usize];

    let summary = serde_json::json!({
        "iterations_total": times.len(),
        "iterations_used_for_cv": warm.len(),
        "warmup_dropped": WARMUP,
        "values_seconds_all": times,
        "mean_seconds": mean,
        "stdev_seconds": stdev,
        "cv_percent": cv,
        "min_seconds": sorted.first().copied().unwrap_or(0.0),
        "max_seconds": sorted.last().copied().unwrap_or(0.0),
        "p50_seconds": p50,
        "p95_seconds": p95,
    });

    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}
