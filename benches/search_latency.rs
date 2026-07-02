//! Search latency bench (hybrid: FTS5 + vector + entity).
//!
//! Measures `searcher::search()` latency for the standard query set over
//! the e2e testdata corpus. Hybrid only — FTS5-only mode requires API
//! extension and is deferred.
//!
//! Prerequisites (see README "Running benches"):
//!   1. tsmd running (`tsm start`) with embedder ready
//!   2. testdata indexed (`tsm init` + `tsm index` against `tests/e2e/testdata`)
//!
//! Usage:
//!   cargo bench --bench search_latency

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use the_space_memory::{config, db, embedder, searcher};

// Every probe query must return at least one hit. A zero-hit query lets
// `searcher::search` short-circuit before it ever reaches the embedder or
// FTS5, so its latency measures an early-return path, not search work — on
// this corpus, a zero-hit query ran in single-digit microseconds versus
// 120-370ms for a real hybrid search, a ~40,000x gap unrelated to any code
// change. `"走れ"` (an earlier probe, since replaced) was such a query.
const QUERIES: &[&str] = &["銀河", "坊っちゃん", "メロス", "夏目", "猫"];
const TOP_K: usize = 5;

fn bench_search_hybrid(c: &mut Criterion) {
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path).unwrap_or_else(|e| {
        panic!(
            "DB not available at {}: {e}\n\
             Run `tsm init` and `tsm index` against tests/e2e/testdata before benching.",
            db_path.display()
        );
    });

    // Preflight: confirm the embedder is reachable so the bench actually
    // measures the hybrid path. `searcher::search` is called with
    // `require_vector=false`, which silently falls back to FTS5/entity-only
    // when the embedder socket is down — that would mislabel FTS-only numbers
    // as "search_hybrid". Fail loudly instead.
    let probe = embedder::embed_via_socket(&[String::from("preflight")]);
    assert!(
        probe.is_some(),
        "Embedder not reachable — the hybrid bench would silently measure \
         FTS-only latency. Run `tsm start` and verify `tsm status` before benching."
    );

    let mut group = c.benchmark_group("search_hybrid");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(50);

    for query in QUERIES {
        group.bench_with_input(BenchmarkId::from_parameter(query), query, |b, q| {
            b.iter(|| searcher::search(&conn, q, TOP_K, None, false, None).unwrap());
        });
    }

    group.finish();
}

criterion_group!(benches, bench_search_hybrid);
criterion_main!(benches);
