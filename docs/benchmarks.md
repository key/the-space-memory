# Benchmarks

Performance benches for the search/index pipeline, plus a CI regression
gate (`.github/workflows/bench.yml`) that runs on every PR touching
`src/`, `benches/`, or `Cargo.toml`.

## Design concept

**Only `embedder_calls` (full-index and single-query-hybrid call counts)
is regression-gated**, on exact equality — it's deterministic for a
fixed corpus and batching strategy, so any deviation (not just a
slowdown) means behavior changed. Indexing throughput and search latency
are recorded into the CI job summary for trend visibility only,
explicitly labeled "recorded, not gated."

This scope is narrower than a naive "fail on >5% slowdown for every
metric" design, based on empirical variance measurement: on the current
5-file testdata corpus, hybrid search latency swung 15-49% between two
back-to-back runs on an idle machine with zero code changes, and
incremental single-file reindex latency showed a ~3.4x warm-up transient
across 5 identical runs. No percentage threshold at this corpus size
would separate a real regression from noise. Revisit gating those
metrics once the corpus is meaningfully larger.

Every probe query in `search_latency.rs` and `record_metrics.rs` must
return at least one hit. A zero-hit query lets `searcher::search`
short-circuit before reaching the embedder or FTS5, so its latency
measures an early-return path, not search work — observed: microseconds
instead of hundreds of milliseconds for a real query.

## File layout

| Path | Role |
|---|---|
| `benches/search_latency.rs` | Human-facing criterion exploration (hybrid search latency, statistical distributions printed to the terminal) |
| `benches/record_metrics.rs` | CI-facing measurement tool (`bench-counters` feature). Prints one JSON document to stdout matching `benches/baseline.json`'s schema |
| `benches/baseline.json` | Committed baseline for the CI environment (`ubuntu-latest`, CPU inference). Bootstrapped by the first CI run, not committed with locally-measured numbers |
| `src/bench_baseline.rs` | Pure baseline schema (serde), bootstrap-aware parsing, embedder-call-count diff, warmup-trimmed mean, percentile helpers. Unit-tested |
| `src/bin/tsm-bench-check/main.rs` | Thin CLI shell over `bench_baseline` — no `bench-counters` dependency, builds by default |
| `src/indexer/mod.rs` (`index_file_timed`, `IndexTimings`) | Prepare/Persist/Embed stage timing, `bench-counters`-gated, sharing `index_file`'s code path via a private `index_file_inner` |
| `.github/workflows/bench.yml` | `gate` job (PR-triggered) and `update-baseline` job (push-to-`main`-triggered) |

## Operations guide

### Prerequisites

- `tsmd` running with embedder ready (`tsm start` and verify via `tsm status`)
- The standard testdata corpus indexed:

  ```bash
  cd tests/e2e/testdata
  tsm init && tsm index
  ```

### Running locally

```bash
# Search latency (hybrid: FTS5 + vector + entity)
cargo bench --bench search_latency

# Full metrics recording (indexing throughput with Prepare/Persist/Embed
# breakdown, incremental-index latency, hybrid search latency, embedder
# call counts)
cargo bench --features bench-counters --bench record_metrics
```

Both benches must be run with the current working directory inside the
indexed project (e.g. `tests/e2e/testdata`) — see [CWD-relative
resolution](#cwd-relative-resolution-a-cargo-bench-gotcha) below for why.

### Regression gate

`tsm-bench-check` is the pure diff/threshold logic (unit-tested in
`src/bench_baseline.rs`) behind a thin CLI shell:

```bash
cargo run --bin tsm-bench-check -- benches/baseline.json current.json
```

Exit codes: `0` (no regression, or `benches/baseline.json` doesn't exist
yet — reported as `BOOTSTRAP`, not a silent pass), `1` (an embedder call
count regressed), `2` (usage error or a file that fails to parse).

### `bench-baseline-bump` label

A PR label, `bench-baseline-bump`, skips the `gate` job entirely for a
change that legitimately alters call counts or recorded numbers on
purpose (e.g. a deliberate perf trade-off for a new feature). The next
`update-baseline` run (post-merge) then adopts the new numbers as the
baseline. The label is auto-created (idempotently) by `update-baseline`
if it doesn't already exist in the repo.

### Embedder call counter

For benches that need to verify embedder call counts, build with the
`bench-counters` feature. Off by default; release builds compile the
counters out entirely.

```bash
cargo build --features bench-counters
```

```rust
use the_space_memory::embedder::counters;

counters::reset_embedder_calls();
// ... run code that calls embed_via_socket_at ...
println!("calls: {}", counters::embedder_call_count());
```

## Internals

### CI workflow structure

`gate` (PR-triggered, or `workflow_dispatch`) builds the release
binaries, pre-builds `record_metrics` ahead of starting the daemon,
indexes the testdata corpus, waits for a genuine (non-trivial) vector
backfill, records current-run metrics, and diffs against
`benches/baseline.json` via `tsm-bench-check`. It does not run on push
to `main` — nothing there needs comparing against a baseline that
`update-baseline` doesn't already handle, and running both would double
the CI cost per merge.

`update-baseline` (push-to-`main`-triggered rather than a separate cron
schedule — `main` only accepts squash-merged PRs, so this already fires
at most once per landed change) re-measures the same way and, if the
numbers changed, opens a PR updating `benches/baseline.json` with the
`bench-baseline-bump` label. It has a `concurrency` group so overlapping
merges queue their baseline-update PRs instead of racing.

### Warm-up handling

Wall-clock measurements (full-index throughput, incremental-index
latency, search latency) are averaged after discarding the first 2 of
each sample set (`WARMUP` in `record_metrics.rs`). This trims the
steepest part of an observed warm-up decay (incremental single-file
reindex on the reference machine: 1144.7ms → 896.3ms → 548.7ms → 495.3ms
→ 340.1ms over 5 consecutive identical runs — a ~3.4x transient, steepest
between iterations 1-3) without proving full convergence by iteration 3.
That's exactly why these metrics stay record-only rather than gated.

### CWD-relative resolution (a `cargo bench` gotcha)

`tsm`'s config resolution (state dir, socket paths) is CWD-relative by
design: `state_dir` defaults to the bare relative string `.tsm`, never
joined against the discovered project root, on the assumption that the
process's CWD already *is* the project root — the same assumption
`tsmd` relies on when launched from inside a project directory.

`cargo bench --manifest-path <elsewhere>` breaks this: it runs the
*compiled binary* with CWD set to the manifest's directory, not the
invoking shell's CWD. `record_metrics`'s embedder-reachability preflight
then reports "not reachable," because it's checking for a socket file
relative to the wrong directory — not because the daemon is actually
down. `TSM_CONFIG` alone doesn't fix it either: project-root resolution
succeeds, but the derived socket path stays a relative string, still
evaluated against the wrong CWD. Absolute `TSM_STATE_DIR`/
`TSM_EMBEDDER_SOCKET` overrides work, but risk the UNIX socket path
length limit (`SUN_LEN`, ~104 chars) on a long CI runner path.

`.github/workflows/bench.yml` avoids all of this by building
`record_metrics` once (`cargo bench --no-run`) and then invoking the
resulting binary directly by path
(`target/release/deps/record_metrics-*`), with `working-directory`
pinned to the indexed project — cargo never touches CWD for the actual
measurement run.

### Mid-run embedder liveness

`embed::insert_vectors` and `searcher::search` (with
`require_vector=false`) both degrade silently on a dead embedder — an
empty `Option` or a quiet FTS-only fallback, no error. `tsmd` does not
restart a crashed embedder child automatically, so a mid-run death would
otherwise show up only as suspiciously fast, suspiciously low-call-count
numbers, not a failed CI run. `record_metrics.rs` re-checks embedder
reachability after each of its three measurement phases (not just once
at startup), tagging the panic message with which phase saw a dead
embedder.

## Implementation reference

- `src/bench_baseline.rs` — `Baseline` schema, `BaselineState`
  (`Bootstrap` / `Loaded` / `Invalid`), `parse_baseline`,
  `check_embedder_calls`, `trimmed_mean`, `percentile`.
- `src/bin/tsm-bench-check/main.rs` — CLI entry point; reads the
  baseline and current-run JSON files, prints the recorded-only summary,
  then the gate verdict.
- `benches/record_metrics.rs` — measurement orchestration
  (`measure_full_index`, `measure_incremental`, `measure_hybrid_search`)
  and JSON serialization.
- `src/indexer/mod.rs` — `index_file_inner` (shared by `index_file` and
  `index_file_timed`), `IndexTimings`.
- `src/embedder.rs` (`counters` module) — `bench-counters`-gated
  atomic call counters.
