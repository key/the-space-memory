//! Search quality benchmark: Precision@5 / MRR / nDCG@5 against a
//! hand-graded golden corpus (`tests/golden/`).
//!
//! Metric computation (`precision_at_k`, `reciprocal_rank`, `ndcg_at_k`) is
//! pure and exercised by the `#[cfg(test)]` unit tests below, which run
//! under plain `cargo test` like any other test in this crate.
//!
//! The live measurement/gate passes need a running daemon with the golden
//! corpus indexed and a real embedder (no mock), so they cannot run inside
//! plain `cargo test` — each is `#[ignore]`d and invoked explicitly by
//! `tests/quality_bench.sh`, which owns environment setup (daemon
//! lifecycle, corpus indexing, embedder up/down toggling). See that script
//! for the full orchestration and `docs/` / `CLAUDE.md` "Isolated
//! benchmark" section for the general pattern this follows.
//!
//! Modes:
//!   - `measure_hybrid`   — default search path (FTS5 + vector + entity).
//!     This is the mode gated against the committed baseline.
//!   - `measure_fts_only` — embedder stopped, `--fallback fts_only`.
//!     Recorded for comparison; not gated (see README "Benchmarks" for why
//!     an embedder-up FTS-only measurement isn't meaningful: `--fallback
//!     fts_only` only changes error handling, it does not disable vector
//!     retrieval while the embedder is reachable).
//!   - `gate_against_baseline` — reads both mode reports plus
//!     `tests/golden/baseline.json` and fails if hybrid quality regressed
//!     beyond threshold.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use serde::{Deserialize, Serialize};

// ---------- Golden data model ----------

#[derive(Debug, Deserialize)]
struct QuerySet {
    queries: Vec<GoldenQuery>,
}

#[derive(Debug, Deserialize)]
struct GoldenQuery {
    id: String,
    category: String,
    query: String,
    relevant: Vec<Judgment>,
}

#[derive(Debug, Deserialize)]
struct Judgment {
    doc: String,
    grade: u8,
}

impl GoldenQuery {
    fn relevance_map(&self) -> HashMap<String, u8> {
        self.relevant
            .iter()
            .map(|j| (j.doc.clone(), j.grade))
            .collect()
    }
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_query_set() -> QuerySet {
    let path = manifest_dir().join("tests/golden/queries.yaml");
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    serde_yaml::from_str(&raw).unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()))
}

// ---------- Pure metrics ----------

/// Precision@k: fraction of the top-k ranked docs that are relevant
/// (grade > 0). A missing slot (fewer than k results returned) counts as
/// non-relevant, matching the definition in `tests/search-test-patterns.md`.
fn precision_at_k(ranked: &[String], relevant: &HashMap<String, u8>, k: usize) -> f64 {
    let hits = ranked
        .iter()
        .take(k)
        .filter(|doc| relevant.get(doc.as_str()).copied().unwrap_or(0) > 0)
        .count();
    hits as f64 / k as f64
}

/// Reciprocal rank (1-indexed) of the first relevant doc in `ranked`, or
/// 0.0 if none of the ranked docs are relevant.
fn reciprocal_rank(ranked: &[String], relevant: &HashMap<String, u8>) -> f64 {
    ranked
        .iter()
        .position(|doc| relevant.get(doc.as_str()).copied().unwrap_or(0) > 0)
        .map(|idx| 1.0 / (idx as f64 + 1.0))
        .unwrap_or(0.0)
}

/// nDCG@k with graded relevance: DCG = sum (2^grade - 1) / log2(rank + 1)
/// over the top k ranked docs, normalized by the ideal DCG (all judged
/// docs sorted by grade descending). Returns 0.0 when the query has no
/// relevant docs at all (IDCG == 0), rather than dividing by zero.
fn ndcg_at_k(ranked: &[String], relevant: &HashMap<String, u8>, k: usize) -> f64 {
    let dcg = dcg_at_k(
        ranked
            .iter()
            .map(|doc| relevant.get(doc.as_str()).copied().unwrap_or(0)),
        k,
    );

    let mut ideal_grades: Vec<u8> = relevant.values().copied().collect();
    ideal_grades.sort_unstable_by(|a, b| b.cmp(a));
    let idcg = dcg_at_k(ideal_grades.into_iter(), k);

    if idcg == 0.0 {
        0.0
    } else {
        dcg / idcg
    }
}

fn dcg_at_k(grades: impl Iterator<Item = u8>, k: usize) -> f64 {
    let sum: f64 = grades
        .take(k)
        .enumerate()
        .map(|(i, grade)| (2f64.powi(grade as i32) - 1.0) / (i as f64 + 2.0).log2())
        .sum();
    // `Sum for f64` folds from -0.0 (the IEEE754-correct additive identity,
    // since -0.0 + -0.0 stays -0.0 while +0.0 + -0.0 would flip it) — so an
    // empty `grades` iterator (e.g. a query with zero search results) yields
    // -0.0 here, which is numerically equal to 0.0 but serializes as "-0.0"
    // in JSON. Normalize it: -0.0 + 0.0 == 0.0 per IEEE754.
    sum + 0.0
}

/// The best Precision@k a query can possibly achieve given its gold set,
/// i.e. Precision@k under a perfect (oracle) ranking. On a small golden
/// corpus, most queries have fewer than k relevant docs by design (a query
/// about one narrow topic in a 20-doc corpus), which caps this below 1.0 —
/// so a raw `mean_precision_at_5` is uninterpretable without this ceiling
/// alongside it (see `mean_precision_at_5_ceiling` on `ModeReport`).
fn oracle_precision_at_k(relevant: &HashMap<String, u8>, k: usize) -> f64 {
    let relevant_count = relevant.values().filter(|&&g| g > 0).count();
    relevant_count.min(k) as f64 / k as f64
}

/// Deduplicate a ranked chunk-level result list into a doc-level ranking,
/// keeping each `source_file`'s best (first) rank position. The golden
/// corpus documents are short enough to each produce a single chunk, so
/// this is normally a no-op, but stays correct if chunking ever splits one.
fn dedup_by_source_file(source_files: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for doc in source_files {
        if seen.insert(doc.clone()) {
            out.push(doc);
        }
    }
    out
}

// ---------- Report model ----------

#[derive(Debug, Serialize, Deserialize, Clone)]
struct QueryMetrics {
    id: String,
    category: String,
    precision_at_5: f64,
    /// Best Precision@5 achievable for this query's gold set (see
    /// `oracle_precision_at_k`). Constant per query regardless of the
    /// search run; included per-query so the aggregate ceiling below is
    /// reproducible from this file alone.
    precision_at_5_ceiling: f64,
    reciprocal_rank: f64,
    ndcg_at_5: f64,
    latency_ms: f64,
    ranked: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ModeReport {
    mode: String,
    mean_precision_at_5: f64,
    /// Mean of `precision_at_5_ceiling` across queries — the best
    /// `mean_precision_at_5` this golden set could ever produce, even under
    /// a perfect ranking. Compare `mean_precision_at_5` against THIS, not
    /// against 1.0 or the aspirational 80% figure in
    /// tests/search-test-patterns.md (that target assumes every query has
    /// >=5 gold-relevant docs, which most queries here don't by design).
    mean_precision_at_5_ceiling: f64,
    mean_mrr: f64,
    mean_ndcg_at_5: f64,
    mean_latency_ms: f64,
    per_query: Vec<QueryMetrics>,
}

fn build_report(mode: &str, per_query: Vec<QueryMetrics>) -> ModeReport {
    let n = per_query.len().max(1) as f64;
    let mean_precision_at_5 = per_query.iter().map(|q| q.precision_at_5).sum::<f64>() / n;
    let mean_precision_at_5_ceiling = per_query
        .iter()
        .map(|q| q.precision_at_5_ceiling)
        .sum::<f64>()
        / n;
    let mean_mrr = per_query.iter().map(|q| q.reciprocal_rank).sum::<f64>() / n;
    let mean_ndcg_at_5 = per_query.iter().map(|q| q.ndcg_at_5).sum::<f64>() / n;
    let mean_latency_ms = per_query.iter().map(|q| q.latency_ms).sum::<f64>() / n;
    ModeReport {
        mode: mode.to_string(),
        mean_precision_at_5,
        mean_precision_at_5_ceiling,
        mean_mrr,
        mean_ndcg_at_5,
        mean_latency_ms,
        per_query,
    }
}

// ---------- Live search invocation (env-gated; see module docs) ----------

/// Run one query against the live `tsm` CLI and return the ranked,
/// doc-deduplicated `source_file` list (relativized to `project_dir`, to
/// match `tests/golden/queries.yaml`'s `doc` paths) plus wall-clock
/// latency. `extra_args` carries mode-specific flags (e.g. `--fallback
/// fts-only`).
fn run_query(project_dir: &Path, query: &str, extra_args: &[&str]) -> (Vec<String>, f64) {
    let start = Instant::now();
    let output = Command::new("tsm")
        .args(["search", "-q", query, "-f", "json", "-k", "5"])
        .args(extra_args)
        .current_dir(project_dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run `tsm search -q {query:?}`: {e}"));
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

    assert!(
        output.status.success(),
        "tsm search -q {query:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("failed to parse search JSON for {query:?}: {e}"));
    let source_files = parsed["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r["source_file"].as_str())
                .map(|s| relativize(s, project_dir))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    (dedup_by_source_file(source_files), latency_ms)
}

/// `tsm search -f json` returns `source_file` as an absolute path (resolved
/// against the indexed project root). Convert it back to a path relative to
/// `project_dir` so it matches the `doc:` paths in `tests/golden/queries.yaml`.
/// Canonicalizes both sides first so this is robust to `/var` vs
/// `/private/var`-style symlink differences (macOS tmp dirs).
fn relativize(source_file: &str, project_dir: &Path) -> String {
    let p = Path::new(source_file);
    let absolute = if p.is_absolute() {
        p.to_path_buf()
    } else {
        project_dir.join(p)
    };
    let canonical_file = absolute.canonicalize().unwrap_or(absolute);
    let canonical_project = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    canonical_file
        .strip_prefix(&canonical_project)
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| source_file.to_string())
}

fn measure(mode: &str, extra_args: &[&str]) -> ModeReport {
    let project_dir = std::env::var("TSM_QB_PROJECT_DIR")
        .unwrap_or_else(|_| panic!("TSM_QB_PROJECT_DIR must be set (see tests/quality_bench.sh)"));
    let project_dir = PathBuf::from(project_dir);

    let query_set = load_query_set();
    let per_query: Vec<QueryMetrics> = query_set
        .queries
        .iter()
        .map(|gq| {
            let relevant = gq.relevance_map();
            let (ranked, latency_ms) = run_query(&project_dir, &gq.query, extra_args);
            QueryMetrics {
                id: gq.id.clone(),
                category: gq.category.clone(),
                precision_at_5: precision_at_k(&ranked, &relevant, 5),
                precision_at_5_ceiling: oracle_precision_at_k(&relevant, 5),
                reciprocal_rank: reciprocal_rank(&ranked, &relevant),
                ndcg_at_5: ndcg_at_k(&ranked, &relevant, 5),
                latency_ms,
                ranked,
            }
        })
        .collect();

    build_report(mode, per_query)
}

fn out_dir() -> PathBuf {
    let dir = std::env::var("TSM_QB_OUT_DIR")
        .unwrap_or_else(|_| panic!("TSM_QB_OUT_DIR must be set (see tests/quality_bench.sh)"));
    PathBuf::from(dir)
}

fn write_report(report: &ModeReport) {
    let dir = out_dir();
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("failed to create {}: {e}", dir.display()));
    let path = dir.join(format!("{}.json", report.mode));
    let json = serde_json::to_string_pretty(report).expect("serialize report");
    fs::write(&path, json).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    println!("wrote {} ({})", path.display(), report.mode);
    println!(
        "  P@5={:.3} (ceiling {:.3}) MRR={:.3} nDCG@5={:.3} latency={:.1}ms (n={})",
        report.mean_precision_at_5,
        report.mean_precision_at_5_ceiling,
        report.mean_mrr,
        report.mean_ndcg_at_5,
        report.mean_latency_ms,
        report.per_query.len()
    );
}

#[test]
#[ignore = "needs a live daemon + real embedder; run via tests/quality_bench.sh"]
fn measure_hybrid() {
    let report = measure("hybrid", &[]);
    write_report(&report);
}

#[test]
#[ignore = "needs a live daemon with the embedder stopped; run via tests/quality_bench.sh"]
fn measure_fts_only() {
    // clap's ValueEnum kebab-cases `FtsOnly` to "fts-only" for the CLI flag
    // (the crate's internal `SearchFallbackArg::Display` impl formats the
    // same value as "fts_only" for daemon-facing messages, but that's not
    // what the argument parser accepts here).
    let report = measure("fts_only", &["--fallback", "fts-only"]);
    write_report(&report);
}

// ---------- Regression gate ----------

/// Absolute-drop thresholds for the aggregate hybrid metrics. Chosen to
/// match the gate formula sketched in the issue that introduced this
/// harness: a single-PR quality dip should fail, but the small (20-doc)
/// golden corpus means per-query metrics are coarse-grained (each query
/// moves by 1/5 or 1/n increments), so thresholds stay above that noise
/// floor rather than at 0.
const MAX_PRECISION_DROP: f64 = 0.05;
const MAX_MRR_DROP: f64 = 0.05;
const MAX_NDCG_DROP: f64 = 0.05;

/// Per-query max-drop thresholds. The aggregate thresholds above are
/// diluted by n=14 queries: a single query's reciprocal rank sliding from
/// rank 1 to rank 3 (RR 1.0 -> 0.333) only moves the AGGREGATE mean MRR by
/// 0.667/14 ~= 0.048, under MAX_MRR_DROP, even though that one query
/// regressed badly — and a top-1 result silently downgrading from a
/// grade-2 to a grade-1 relevant doc doesn't change reciprocal rank at all
/// (still rank 1) but nearly triples nDCG@5's gap from ideal for a
/// single-relevant-doc query. Per-query thresholds close both gaps: nDCG
/// is graded-relevance-aware so it independently catches the grade
/// downgrade, and RR independently catches the rank slide, regardless of
/// what the aggregate does. 0.2 is roughly "a relevant result moved by two
/// or more ranks, or the top hit's grade was meaningfully replaced" — loose
/// enough to tolerate the coarse-grained jitter this 20-doc corpus
/// naturally produces between adjacent low-relevance ranks, tight enough to
/// catch both evasions above.
const MAX_PER_QUERY_RR_DROP: f64 = 0.2;
const MAX_PER_QUERY_NDCG_DROP: f64 = 0.2;

#[derive(Debug, Serialize, Deserialize)]
struct Baseline {
    hybrid: ModeReport,
}

fn load_baseline() -> Baseline {
    let path = manifest_dir().join("tests/golden/baseline.json");
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()))
}

fn load_mode_report(mode: &str) -> ModeReport {
    let path = out_dir().join(format!("{mode}.json"));
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "failed to read {}: {e}. Run measure_{mode} first.",
            path.display()
        )
    });
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()))
}

/// Compare `current` against `baseline` and return a list of human-readable
/// regression descriptions. Empty means the gate passes. Pure function
/// (no I/O) so it is unit-tested directly below.
fn check_regressions(baseline: &ModeReport, current: &ModeReport) -> Vec<String> {
    let mut failures = Vec::new();

    let precision_drop = baseline.mean_precision_at_5 - current.mean_precision_at_5;
    if precision_drop > MAX_PRECISION_DROP {
        failures.push(format!(
            "mean Precision@5 dropped {precision_drop:.3} (baseline {:.3} -> current {:.3}, max allowed drop {MAX_PRECISION_DROP})",
            baseline.mean_precision_at_5, current.mean_precision_at_5
        ));
    }

    let mrr_drop = baseline.mean_mrr - current.mean_mrr;
    if mrr_drop > MAX_MRR_DROP {
        failures.push(format!(
            "mean MRR dropped {mrr_drop:.3} (baseline {:.3} -> current {:.3}, max allowed drop {MAX_MRR_DROP})",
            baseline.mean_mrr, current.mean_mrr
        ));
    }

    let ndcg_drop = baseline.mean_ndcg_at_5 - current.mean_ndcg_at_5;
    if ndcg_drop > MAX_NDCG_DROP {
        failures.push(format!(
            "mean nDCG@5 dropped {ndcg_drop:.3} (baseline {:.3} -> current {:.3}, max allowed drop {MAX_NDCG_DROP})",
            baseline.mean_ndcg_at_5, current.mean_ndcg_at_5
        ));
    }

    // Per-query hard gates. Aggregate thresholds alone are evadable: a
    // single query's regression gets diluted by averaging over n queries
    // (see MAX_PER_QUERY_*_DROP docs above for the two concrete evasions
    // this closes). These run per query so neither evasion can hide behind
    // the other 13 queries staying flat.
    let current_by_id: HashMap<&str, &QueryMetrics> = current
        .per_query
        .iter()
        .map(|q| (q.id.as_str(), q))
        .collect();
    for base_q in &baseline.per_query {
        if base_q.reciprocal_rank > 0.0 {
            match current_by_id.get(base_q.id.as_str()) {
                Some(cur_q) if cur_q.reciprocal_rank == 0.0 => {
                    failures.push(format!(
                        "query {} ({}): top relevant doc fell out of top 5 (was rank {:.0})",
                        base_q.id,
                        base_q.category,
                        1.0 / base_q.reciprocal_rank
                    ));
                }
                Some(cur_q) => {
                    let rr_drop = base_q.reciprocal_rank - cur_q.reciprocal_rank;
                    if rr_drop > MAX_PER_QUERY_RR_DROP {
                        failures.push(format!(
                            "query {} ({}): reciprocal rank dropped {rr_drop:.3} (baseline {:.3} -> current {:.3}, max allowed drop {MAX_PER_QUERY_RR_DROP})",
                            base_q.id, base_q.category, base_q.reciprocal_rank, cur_q.reciprocal_rank
                        ));
                    }
                }
                None => failures.push(format!(
                    "query {} present in baseline but missing from current run",
                    base_q.id
                )),
            }
        }

        // nDCG is graded-relevance-aware, so it catches a top-ranked doc
        // being replaced by a *lower-graded* relevant doc even when RR
        // (rank-only) sees no change at all. Checked independent of the RR
        // branch above (and independent of `base_q.reciprocal_rank > 0.0`)
        // since a query can gain nDCG signal even where RR was already 0.
        if base_q.ndcg_at_5 > 0.0 {
            if let Some(cur_q) = current_by_id.get(base_q.id.as_str()) {
                let ndcg_drop = base_q.ndcg_at_5 - cur_q.ndcg_at_5;
                if ndcg_drop > MAX_PER_QUERY_NDCG_DROP {
                    failures.push(format!(
                        "query {} ({}): nDCG@5 dropped {ndcg_drop:.3} (baseline {:.3} -> current {:.3}, max allowed drop {MAX_PER_QUERY_NDCG_DROP})",
                        base_q.id, base_q.category, base_q.ndcg_at_5, cur_q.ndcg_at_5
                    ));
                }
            }
            // A query missing from `current` entirely is already reported
            // by the reciprocal_rank branch above when reciprocal_rank > 0;
            // ndcg_at_5 > 0.0 implies reciprocal_rank > 0.0 (a query can't
            // have graded gain without at least one relevant hit), so this
            // never needs its own "missing" branch.
        }
    }

    failures
}

#[test]
#[ignore = "needs measure_hybrid to have run first; run via tests/quality_bench.sh"]
fn gate_against_baseline() {
    let baseline = load_baseline();
    let current = load_mode_report("hybrid");

    println!(
        "baseline: P@5={:.3} MRR={:.3} nDCG@5={:.3}",
        baseline.hybrid.mean_precision_at_5,
        baseline.hybrid.mean_mrr,
        baseline.hybrid.mean_ndcg_at_5
    );
    println!(
        "current:  P@5={:.3} MRR={:.3} nDCG@5={:.3}",
        current.mean_precision_at_5, current.mean_mrr, current.mean_ndcg_at_5
    );

    let failures = check_regressions(&baseline.hybrid, &current);
    assert!(
        failures.is_empty(),
        "search quality regression detected:\n{}",
        failures.join("\n")
    );
}

// ---------- Unit tests (pure functions; run under plain `cargo test`) ----------

#[cfg(test)]
mod tests {
    use super::*;

    fn relmap(pairs: &[(&str, u8)]) -> HashMap<String, u8> {
        pairs.iter().map(|(d, g)| (d.to_string(), *g)).collect()
    }

    fn ranked(docs: &[&str]) -> Vec<String> {
        docs.iter().map(|d| d.to_string()).collect()
    }

    // ---- precision_at_k ----

    #[test]
    fn precision_at_5_all_top5_relevant() {
        let r = ranked(&["a", "b", "c", "d", "e"]);
        let rel = relmap(&[("a", 1), ("b", 1), ("c", 1), ("d", 1), ("e", 1)]);

        let p = precision_at_k(&r, &rel, 5);

        assert_eq!(p, 1.0);
    }

    #[test]
    fn precision_at_5_partial_hits() {
        let r = ranked(&["a", "x", "b", "y", "z"]);
        let rel = relmap(&[("a", 2), ("b", 1)]);

        let p = precision_at_k(&r, &rel, 5);

        assert_eq!(p, 2.0 / 5.0);
    }

    #[test]
    fn precision_at_5_fewer_than_5_results_counts_missing_as_non_relevant() {
        let r = ranked(&["a", "b"]);
        let rel = relmap(&[("a", 1), ("b", 1)]);

        let p = precision_at_k(&r, &rel, 5);

        assert_eq!(p, 2.0 / 5.0);
    }

    #[test]
    fn precision_at_5_no_relevant_docs_is_zero() {
        let r = ranked(&["x", "y", "z"]);
        let rel: HashMap<String, u8> = HashMap::new();

        let p = precision_at_k(&r, &rel, 5);

        assert_eq!(p, 0.0);
    }

    // ---- oracle_precision_at_k ----

    #[test]
    fn oracle_precision_at_5_capped_below_one_when_fewer_than_5_relevant() {
        // Only 2 gold-relevant docs exist for this query, so even a perfect
        // ranking cannot beat 2/5.
        let rel = relmap(&[("a", 2), ("b", 1)]);

        let ceiling = oracle_precision_at_k(&rel, 5);

        assert_eq!(ceiling, 2.0 / 5.0);
    }

    #[test]
    fn oracle_precision_at_5_is_one_when_at_least_5_relevant() {
        let rel = relmap(&[("a", 1), ("b", 1), ("c", 1), ("d", 1), ("e", 1), ("f", 1)]);

        let ceiling = oracle_precision_at_k(&rel, 5);

        assert_eq!(ceiling, 1.0);
    }

    #[test]
    fn oracle_precision_at_5_ignores_grade_zero_judgments() {
        // A judged-but-irrelevant doc (grade 0) doesn't raise the ceiling.
        let rel = relmap(&[("a", 1), ("b", 0), ("c", 0)]);

        let ceiling = oracle_precision_at_k(&rel, 5);

        assert_eq!(ceiling, 1.0 / 5.0);
    }

    // ---- reciprocal_rank ----

    #[test]
    fn mrr_relevant_doc_at_first_rank() {
        let r = ranked(&["a", "b", "c"]);
        let rel = relmap(&[("a", 1)]);

        assert_eq!(reciprocal_rank(&r, &rel), 1.0);
    }

    #[test]
    fn mrr_relevant_doc_at_third_rank() {
        let r = ranked(&["x", "y", "a"]);
        let rel = relmap(&[("a", 1)]);

        assert_eq!(reciprocal_rank(&r, &rel), 1.0 / 3.0);
    }

    #[test]
    fn mrr_no_relevant_doc_present_is_zero() {
        let r = ranked(&["x", "y", "z"]);
        let rel = relmap(&[("a", 1)]);

        assert_eq!(reciprocal_rank(&r, &rel), 0.0);
    }

    // ---- ndcg_at_k ----

    #[test]
    fn ndcg_at_5_perfect_ranking_is_one() {
        let r = ranked(&["a", "b", "c"]);
        let rel = relmap(&[("a", 2), ("b", 1), ("c", 1)]);

        let n = ndcg_at_k(&r, &rel, 5);

        assert!((n - 1.0).abs() < 1e-9, "expected 1.0, got {n}");
    }

    #[test]
    fn ndcg_at_5_reversed_ranking_scores_below_perfect() {
        let perfect = ranked(&["a", "b"]);
        let reversed = ranked(&["b", "a"]);
        let rel = relmap(&[("a", 2), ("b", 1)]);

        let n_perfect = ndcg_at_k(&perfect, &rel, 5);
        let n_reversed = ndcg_at_k(&reversed, &rel, 5);

        assert_eq!(n_perfect, 1.0);
        assert!(n_reversed < n_perfect);
    }

    #[test]
    fn ndcg_at_5_no_relevant_docs_is_zero_not_nan() {
        let r = ranked(&["x", "y"]);
        let rel: HashMap<String, u8> = HashMap::new();

        let n = ndcg_at_k(&r, &rel, 5);

        assert_eq!(n, 0.0);
    }

    #[test]
    fn ndcg_at_5_ignores_docs_beyond_k() {
        // A relevant doc at rank 6 should not affect nDCG@5 at all.
        let r = ranked(&["x", "y", "z", "w", "v", "a"]);
        let rel = relmap(&[("a", 2)]);

        let n = ndcg_at_k(&r, &rel, 5);

        assert_eq!(n, 0.0);
    }

    #[test]
    fn ndcg_at_5_zero_search_results_is_positive_zero_not_negative_zero() {
        // A query with relevant docs (idcg > 0) but zero search results
        // (e.g. a filtered-out temporal query): DCG sums over an empty
        // iterator, which `Sum for f64` folds from -0.0. -0.0 == 0.0
        // numerically (this assert would pass either way), but a JSON
        // report showing "-0.0" reads as a bug to anyone skimming
        // baseline.json, so also assert the sign bit explicitly.
        let r: Vec<String> = vec![];
        let rel = relmap(&[("a", 2)]);

        let n = ndcg_at_k(&r, &rel, 5);

        assert_eq!(n, 0.0);
        assert!(n.is_sign_positive(), "expected +0.0, got -0.0");
    }

    // ---- dedup_by_source_file ----

    #[test]
    fn dedup_keeps_first_occurrence_order() {
        let chunks = vec![
            "a.md".to_string(),
            "b.md".to_string(),
            "a.md".to_string(),
            "c.md".to_string(),
        ];

        let docs = dedup_by_source_file(chunks);

        assert_eq!(docs, vec!["a.md", "b.md", "c.md"]);
    }

    // ---- relativize ----

    #[test]
    fn relativize_strips_absolute_project_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project_dir = tmp.path().canonicalize().expect("canonicalize project dir");
        let sub = project_dir.join("notes");
        fs::create_dir_all(&sub).expect("create subdir");
        let file = sub.join("a.md");
        fs::write(&file, "x").expect("write file");

        let rel = relativize(file.to_str().unwrap(), &project_dir);

        assert_eq!(rel, "notes/a.md");
    }

    #[test]
    fn relativize_falls_back_to_original_when_outside_project() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project_dir = tmp.path().canonicalize().expect("canonicalize project dir");
        let outside = "/nonexistent-for-this-test/outside.md";

        let rel = relativize(outside, &project_dir);

        assert_eq!(rel, outside);
    }

    // ---- golden query set loads and is internally consistent ----

    #[test]
    fn golden_query_set_parses_and_has_no_empty_queries() {
        let qs = load_query_set();

        assert!(!qs.queries.is_empty());
        for q in &qs.queries {
            assert!(!q.id.is_empty());
            assert!(!q.query.trim().is_empty(), "query {} has empty text", q.id);
            assert!(
                !q.relevant.is_empty(),
                "query {} has no relevance judgments",
                q.id
            );
        }
    }

    #[test]
    fn golden_query_set_has_unique_ids() {
        let qs = load_query_set();
        let mut ids: Vec<&str> = qs.queries.iter().map(|q| q.id.as_str()).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();

        assert_eq!(
            ids.len(),
            before,
            "duplicate query IDs in tests/golden/queries.yaml"
        );
    }

    // ---- check_regressions (pure) ----

    fn mode_report(
        mean_p5: f64,
        mean_mrr: f64,
        mean_ndcg: f64,
        per_query: Vec<QueryMetrics>,
    ) -> ModeReport {
        ModeReport {
            mode: "hybrid".to_string(),
            mean_precision_at_5: mean_p5,
            mean_precision_at_5_ceiling: 1.0,
            mean_mrr,
            mean_ndcg_at_5: mean_ndcg,
            mean_latency_ms: 0.0,
            per_query,
        }
    }

    fn qm(id: &str, reciprocal_rank: f64) -> QueryMetrics {
        qm_ndcg(id, reciprocal_rank, 0.0)
    }

    fn qm_ndcg(id: &str, reciprocal_rank: f64, ndcg_at_5: f64) -> QueryMetrics {
        QueryMetrics {
            id: id.to_string(),
            category: "test".to_string(),
            precision_at_5: 0.0,
            precision_at_5_ceiling: 1.0,
            reciprocal_rank,
            ndcg_at_5,
            latency_ms: 0.0,
            ranked: vec![],
        }
    }

    #[test]
    fn check_regressions_no_drop_passes() {
        let baseline = mode_report(0.8, 0.7, 0.75, vec![qm("E1", 1.0)]);
        let current = mode_report(0.8, 0.7, 0.75, vec![qm("E1", 1.0)]);

        assert!(check_regressions(&baseline, &current).is_empty());
    }

    #[test]
    fn check_regressions_small_drop_within_threshold_passes() {
        let baseline = mode_report(0.80, 0.70, 0.75, vec![]);
        let current = mode_report(0.76, 0.68, 0.72, vec![]);

        assert!(check_regressions(&baseline, &current).is_empty());
    }

    #[test]
    fn check_regressions_precision_drop_beyond_threshold_fails() {
        let baseline = mode_report(0.80, 0.70, 0.75, vec![]);
        let current = mode_report(0.70, 0.70, 0.75, vec![]);

        let failures = check_regressions(&baseline, &current);

        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("Precision@5"));
    }

    #[test]
    fn check_regressions_query_falling_out_of_top5_fails_even_with_aggregate_ok() {
        // Aggregate MRR barely moves (one query among many), but the
        // per-query hard gate must still catch the individual regression.
        let baseline = mode_report(0.80, 0.70, 0.75, vec![qm("E1", 1.0), qm("E2", 0.5)]);
        let current = mode_report(0.80, 0.70, 0.75, vec![qm("E1", 0.0), qm("E2", 0.5)]);

        let failures = check_regressions(&baseline, &current);

        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("E1"));
        assert!(failures[0].contains("top 5"));
    }

    #[test]
    fn check_regressions_query_missing_from_baseline_side_is_ignored() {
        // A newly-added query in `current` that wasn't in baseline is not
        // a regression by itself.
        let baseline = mode_report(0.80, 0.70, 0.75, vec![qm("E1", 1.0)]);
        let current = mode_report(0.80, 0.70, 0.75, vec![qm("E1", 1.0), qm("E9", 1.0)]);

        assert!(check_regressions(&baseline, &current).is_empty());
    }

    #[test]
    fn check_regressions_query_removed_from_current_fails() {
        let baseline = mode_report(0.80, 0.70, 0.75, vec![qm("E1", 1.0)]);
        let current = mode_report(0.80, 0.70, 0.75, vec![]);

        let failures = check_regressions(&baseline, &current);

        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("missing from current run"));
    }

    #[test]
    fn check_regressions_per_query_rank_slide_fails_even_though_aggregate_hides_it() {
        // A single query's RR sliding from rank 1 (RR 1.0) to rank 3
        // (RR 0.333) only moves the 14-query aggregate mean MRR by
        // ~0.048 (0.667 / 14) — under the 0.05 aggregate threshold — but
        // the per-query drop (0.667) is far past MAX_PER_QUERY_RR_DROP and
        // must fail on its own.
        let base_queries: Vec<QueryMetrics> = (0..14)
            .map(|i| qm(&format!("Q{i}"), if i == 0 { 1.0 } else { 0.7 }))
            .collect();
        let mut cur_queries = base_queries.clone();
        cur_queries[0] = qm("Q0", 1.0 / 3.0);

        let baseline_mean_mrr = base_queries.iter().map(|q| q.reciprocal_rank).sum::<f64>() / 14.0;
        let current_mean_mrr = cur_queries.iter().map(|q| q.reciprocal_rank).sum::<f64>() / 14.0;
        assert!(
            baseline_mean_mrr - current_mean_mrr < MAX_MRR_DROP,
            "test setup invariant broken: aggregate drop should hide under the aggregate threshold"
        );

        let baseline = mode_report(0.80, baseline_mean_mrr, 0.75, base_queries);
        let current = mode_report(0.80, current_mean_mrr, 0.75, cur_queries);

        let failures = check_regressions(&baseline, &current);

        assert!(
            failures
                .iter()
                .any(|f| f.contains("Q0") && f.contains("reciprocal rank")),
            "expected a per-query reciprocal rank failure for Q0, got: {failures:?}"
        );
    }

    #[test]
    fn check_regressions_per_query_grade_downgrade_fails_via_ndcg_even_though_rr_unchanged() {
        // Top-1 result silently replaced by a lower-graded relevant doc:
        // reciprocal rank is unchanged (still rank 1 -> RR 1.0 either way),
        // so the RR-based checks see nothing. nDCG@5 is graded-relevance-
        // aware and must catch this on its own.
        let baseline = mode_report(0.80, 0.70, 0.75, vec![qm_ndcg("E1", 1.0, 1.0)]);
        let current = mode_report(0.80, 0.70, 0.75, vec![qm_ndcg("E1", 1.0, 0.333)]);

        let failures = check_regressions(&baseline, &current);

        assert!(
            failures
                .iter()
                .any(|f| f.contains("E1") && f.contains("nDCG@5")),
            "expected a per-query nDCG@5 failure for E1, got: {failures:?}"
        );
    }

    #[test]
    fn check_regressions_per_query_small_jitter_within_threshold_passes() {
        let baseline = mode_report(0.80, 0.70, 0.75, vec![qm_ndcg("E1", 1.0, 1.0)]);
        let current = mode_report(0.80, 0.70, 0.75, vec![qm_ndcg("E1", 0.9, 0.9)]);

        assert!(check_regressions(&baseline, &current).is_empty());
    }
}
