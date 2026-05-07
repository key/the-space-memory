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

use the_space_memory::{config, db, searcher};

const QUERIES: &[&str] = &["銀河", "坊っちゃん", "メロス", "夏目", "走れ"];
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
