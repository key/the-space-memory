//! Records current-run performance metrics for the CI regression gate and
//! nightly baseline update (`benches/baseline.json`).
//!
//! Unlike `search_latency.rs` (a human-facing criterion exploration tool),
//! this binary is the CI-facing measurement tool: it produces one JSON
//! document on stdout matching `the_space_memory::bench_baseline::Baseline`,
//! covering all 4 metrics from the original design (full-index throughput
//! with stage breakdown, incremental-index latency, hybrid search latency,
//! embedder call counts). Only `embedder_calls` is gated — see
//! `bench_baseline` module docs for why the rest is record-only.
//!
//! Requires the `bench-counters` feature (uses `embedder::counters` and
//! `indexer::index_file_timed`).
//!
//! Prerequisites: `tsm start` with the embedder reachable at the configured
//! socket, and the standard testdata corpus indexed (`tsm init` + `tsm
//! index` against `tests/e2e/testdata`) for the search-latency portion. The
//! indexing-throughput portion reads the corpus files directly and indexes
//! into scratch in-memory databases — it does not touch the already-indexed
//! DB or any tracked file.
//!
//! Usage:
//!   cargo bench --features bench-counters --bench record_metrics

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::Connection;

use the_space_memory::bench_baseline::{
    percentile, trimmed_mean, Baseline, EmbedderCallCounts, FullThroughput, IncrementalLatency,
    IndexingMetrics, LatencyStats, SearchMetrics, StageBreakdown,
};
use the_space_memory::embedder::counters;
use the_space_memory::{config, db, embedder, indexer, searcher};

const SEARCH_QUERIES: &[&str] = &["銀河", "坊っちゃん", "メロス", "夏目", "猫"];
const TOP_K: usize = 5;
// See `bench_baseline::trimmed_mean` docs for the empirical basis: the
// steepest part of the observed warm-up decay lands in the first 2
// iterations on the reference machine.
const WARMUP: usize = 2;
const FULL_INDEX_SAMPLES: usize = 5;
const INCREMENTAL_SAMPLES: usize = 7;
const SEARCH_SAMPLES: usize = 7;

fn testdata_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/e2e/testdata")
}

fn corpus_files() -> Vec<PathBuf> {
    let notes = testdata_dir().join("notes");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&notes)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", notes.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    files.sort();
    files
}

/// Preflight: confirm the embedder is reachable so the recorded numbers
/// actually measure CPU inference, not a silent FTS-only fallback (see the
/// same check in `search_latency.rs`).
///
/// Called both before and after each measurement phase in `main`, not just
/// once at startup: `embed::insert_vectors` and `searcher::search` (with
/// `require_vector=false`) both degrade silently — an empty `Option` or a
/// quiet FTS-only fallback, no error — when the embedder socket disappears
/// mid-run. `tsmd` does not restart a crashed embedder child automatically,
/// so a mid-run death would otherwise show up only as suspiciously fast,
/// suspiciously low-call-count numbers, not a failed CI run. Tagging each
/// call with `stage` pinpoints where a dead embedder was first observed.
fn assert_embedder_reachable(stage: &str) {
    let probe = embedder::embed_via_socket(&[String::from("preflight")]);
    assert!(
        probe.is_some(),
        "Embedder not reachable ({stage}) — record_metrics needs the real \
         embedder for every phase (mocking, or a silent fallback partway \
         through, would defeat the point of a CPU-inference baseline). Run \
         `tsm start` and verify `tsm status` before running this bench."
    );
}

/// One full index of the corpus into a fresh in-memory DB. Returns
/// per-stage totals, the wall-clock total, and the resulting chunk count.
fn run_full_index(files: &[PathBuf], project_root: &Path) -> (f64, f64, f64, f64, i64) {
    let conn = db::get_memory_connection().expect("in-memory DB");
    let mut prepare = 0.0;
    let mut persist = 0.0;
    let mut embed = 0.0;
    let start = Instant::now();
    for f in files {
        let (_, timings) =
            indexer::index_file_timed(&conn, f, project_root).expect("index_file_timed");
        prepare += timings.prepare.as_secs_f64();
        persist += timings.persist.as_secs_f64();
        embed += timings.embed.as_secs_f64();
    }
    let total = start.elapsed().as_secs_f64();
    let chunks: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
        .unwrap_or(0);
    (total, prepare, persist, embed, chunks)
}

fn mean_or_raw(samples: &[f64], warmup: usize) -> f64 {
    trimmed_mean(samples, warmup)
        .unwrap_or_else(|| samples.iter().sum::<f64>() / samples.len().max(1) as f64)
}

fn measure_full_index(files: &[PathBuf], project_root: &Path) -> (FullThroughput, u64) {
    // The embedder-call count is deterministic for a fixed corpus and
    // batching strategy (one `embed::insert_vectors` call per indexed file
    // with pending chunks), so it's captured once rather than repeated
    // across samples — repeating it would only add noise, not information.
    counters::reset_embedder_calls();
    let first = run_full_index(files, project_root);
    let full_index_calls = counters::embedder_call_count();

    let mut totals = vec![first.0];
    let mut prepares = vec![first.1];
    let mut persists = vec![first.2];
    let mut embeds = vec![first.3];
    let mut chunks_last = first.4;
    for _ in 1..FULL_INDEX_SAMPLES {
        let (total, prepare, persist, embed, chunks) = run_full_index(files, project_root);
        totals.push(total);
        prepares.push(prepare);
        persists.push(persist);
        embeds.push(embed);
        chunks_last = chunks;
    }

    let mean_total = mean_or_raw(&totals, WARMUP);
    let mean_prepare = mean_or_raw(&prepares, WARMUP);
    let mean_persist = mean_or_raw(&persists, WARMUP);
    let mean_embed = mean_or_raw(&embeds, WARMUP);

    let files_per_sec = if mean_total > 0.0 {
        files.len() as f64 / mean_total
    } else {
        0.0
    };
    let chunks_per_sec = if mean_total > 0.0 {
        chunks_last as f64 / mean_total
    } else {
        0.0
    };

    (
        FullThroughput {
            total_seconds: mean_total,
            files_per_sec,
            chunks_per_sec,
            breakdown: StageBreakdown {
                prepare_seconds: mean_prepare,
                embed_seconds: mean_embed,
                persist_seconds: mean_persist,
            },
        },
        full_index_calls,
    )
}

/// Reindexes a single touched file `INCREMENTAL_SAMPLES` times, appending a
/// marker line before each pass to force a content-hash change. Works on a
/// scratch copy in a tempdir so tracked corpus files are never mutated.
fn measure_incremental(seed_file: &Path) -> IncrementalLatency {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("incremental.md");
    std::fs::copy(seed_file, &target).expect("seed incremental file");

    let conn = db::get_memory_connection().expect("in-memory DB");
    // Establish the file in the index once; not part of the measured samples.
    indexer::index_file_timed(&conn, &target, tmp.path()).expect("initial index");

    let mut samples = Vec::with_capacity(INCREMENTAL_SAMPLES);
    for i in 0..INCREMENTAL_SAMPLES {
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&target)
                .expect("open for append");
            writeln!(f, "<!-- bench touch {i} -->").expect("append marker");
        }
        let (indexed, timings) =
            indexer::index_file_timed(&conn, &target, tmp.path()).expect("reindex");
        assert!(indexed, "touched file must be detected as changed");
        samples.push((timings.prepare + timings.persist + timings.embed).as_secs_f64());
    }

    let mean = mean_or_raw(&samples, WARMUP);
    IncrementalLatency { mean, samples }
}

fn measure_hybrid_search(conn: &Connection) -> (SearchMetrics, u64) {
    // Same determinism argument as `measure_full_index`: one embedder call
    // per hybrid query, captured once on a warm connection.
    counters::reset_embedder_calls();
    searcher::search(conn, SEARCH_QUERIES[0], TOP_K, None, false, None).expect("warmup search");
    let single_query_hybrid_calls = counters::embedder_call_count();

    let mut by_query = BTreeMap::new();
    // Pooled, warmup-trimmed raw per-request latencies across all queries.
    // `hybrid_ms.p50`/`p95` are computed over this pool — a genuine
    // request-latency distribution — rather than over the 5 per-query
    // means, which would understate tail latency and misrepresent what a
    // "p95" is supposed to mean. `by_query` below stays per-query (that's
    // what its name promises).
    let mut pooled_samples: Vec<f64> = Vec::with_capacity(SEARCH_QUERIES.len() * SEARCH_SAMPLES);
    for &q in SEARCH_QUERIES {
        let mut samples = Vec::with_capacity(SEARCH_SAMPLES);
        for _ in 0..SEARCH_SAMPLES {
            let start = Instant::now();
            let out = searcher::search(conn, q, TOP_K, None, false, None).expect("search");
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            assert!(
                out.total_hits > 0,
                "probe query {q:?} returned zero hits — every probe query must \
                 hit the corpus, or its latency measures an early-return path \
                 instead of search work (see the QUERIES doc comment in \
                 search_latency.rs)"
            );
        }
        by_query.insert(q.to_string(), mean_or_raw(&samples, WARMUP));
        let trimmed = if samples.len() > WARMUP {
            &samples[WARMUP..]
        } else {
            &samples[..]
        };
        pooled_samples.extend_from_slice(trimmed);
    }

    pooled_samples.sort_by(|a, b| a.partial_cmp(b).unwrap());

    (
        SearchMetrics {
            // FTS5-only mode needs a `searcher::search` API extension (see
            // search_latency.rs); left zeroed until that lands.
            fts5_only_ms: LatencyStats::default(),
            hybrid_ms: LatencyStats {
                p50: percentile(&pooled_samples, 50.0),
                p95: percentile(&pooled_samples, 95.0),
                by_query,
            },
        },
        single_query_hybrid_calls,
    )
}

fn main() {
    assert_embedder_reachable("preflight");

    let files = corpus_files();
    let project_root = testdata_dir();
    let (full_throughput, full_index_calls) = measure_full_index(&files, &project_root);
    assert_embedder_reachable("after full-index measurement");
    let incremental_latency_seconds = measure_incremental(&files[0]);
    assert_embedder_reachable("after incremental-index measurement");

    let db_path = config::db_path();
    let conn = db::get_connection(&db_path).unwrap_or_else(|e| {
        panic!(
            "DB not available at {}: {e}\n\
             Run `tsm init` and `tsm index` against tests/e2e/testdata before benching.",
            db_path.display()
        );
    });
    let (search, single_query_hybrid_calls) = measure_hybrid_search(&conn);
    assert_embedder_reachable("after search-latency measurement");

    let baseline = Baseline {
        schema_version: "1".to_string(),
        env: std::env::var("TSM_BENCH_ENV").unwrap_or_else(|_| "local".to_string()),
        embedder: "ruri-v3-30m-fp32".to_string(),
        corpus: "tests/e2e/testdata/notes".to_string(),
        indexing: IndexingMetrics {
            full_throughput,
            incremental_latency_seconds,
        },
        search,
        embedder_calls: EmbedderCallCounts {
            full_index: full_index_calls,
            single_query_hybrid: single_query_hybrid_calls,
        },
    };

    println!(
        "{}",
        serde_json::to_string_pretty(&baseline).expect("serialize baseline")
    );
}
