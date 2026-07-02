# Search Quality

Precision@5 / MRR / nDCG@5 measured against a hand-graded golden corpus,
gated in CI against a committed baseline. This catches quality regressions
from dictionary changes, scoring parameter tuning, Lua hook edits, or
synonym expansion changes — the kind of change that doesn't break any
existing test but silently makes search worse.

## Why this exists

Search quality can regress without breaking a single test. A dictionary
addition can split a compound noun the wrong way, a scoring parameter
tweak can bury a previously-top result, a Lua hook edit can silently zero
out a class of documents — none of these fail `cargo test`, because there
is no ground truth for "is this search result actually right" anywhere
else in the test suite. This harness is that ground truth: a small,
hand-graded corpus with known-correct answers, measured the same way on
every PR that touches ranking-adjacent code.

### Why a purpose-built corpus, not the existing query patterns

`tests/search-test-patterns.md` documents a standard query set used
historically for manual quality spot-checks. Those queries were written
against the live, multi-workspace content this tool is actually used
against day to day — content that has no equivalent in this repository's
test assets, and that is not appropriate to reproduce here even in part:
it includes real, private project and business content that must not
appear in a public repository. Annotating those queries with
relevant-document IDs would require either shipping a slice of that
private corpus into a public repository, or a corpus that doesn't actually
match the queries. Neither is workable for an automated, CI-run gate, so
this harness ships its own small, self-contained corpus and query set
instead, restricted to topics with no product, business, or in-development
project content: this repository's own tech stack, and two personal-hobby
domains of the corpus author. `tests/search-test-patterns.md` remains the
reference for manual, exploratory quality checks against a real workspace;
this harness is the automated regression gate.

### Why graded relevance, not binary

Relevance judgments use a 0–2 scale (0 = irrelevant / not judged, 1 =
relevant, 2 = highly relevant — the query's primary topic match) rather
than binary relevant/irrelevant. nDCG@5 only carries information beyond
Precision@5 when relevance is graded: under binary judgments, nDCG@5
degenerates into a rank-position-weighted restatement of P@5. Graded
judgments let nDCG@5 independently detect a regression that P@5 and MRR
cannot see — e.g. a query's top result silently downgraded from a
grade-2 to a grade-1 relevant document. Reciprocal rank doesn't change
(the result is still rank 1), so a rank-only metric is blind to it; nDCG
is not.

### Why document-level judgments, not chunk-level

Judgments key on `source_file` (the document path), not chunk ID. Chunk
IDs are not stable across a reindex — chunking boundaries can shift when
a document is edited, when `MAX_CHUNK_CHARS` changes, or when the
chunker's heading-detection logic changes — so pinning judgments to a
chunk ID would make the golden set fragile to unrelated changes.
Empirically, this corpus's documents are short enough that each produces
close to one chunk each (13 documents → 17 chunks), so the practical
difference is small, but the judgment format doesn't depend on that
staying true.

### Why baseline-relative regression detection, not absolute targets

`tests/search-test-patterns.md` states aspirational absolute targets
(Precision@5 ≥ 80%, MRR ≥ 0.7). This harness does not gate on those
targets. It gates on regression relative to a committed baseline instead,
for two reasons:

1. **The targets are corpus-dependent, and this corpus makes some of them
   structurally unreachable.** Most queries in this 13-document corpus
   have fewer than 5 gold-relevant documents by design — a query about
   one narrow topic in a small, multi-cluster corpus naturally has 1–4
   relevant documents, not 5+. Even a perfect ranking can only put those
   documents in the top 5; the rest of the top-5 slots are unavoidably
   non-relevant. This caps the best-achievable mean Precision@5 well below
   1.0 — call this the P@5 **ceiling**. It's computed per query as
   `oracle_precision_at_k`: `min(relevant_count, k) / k`, i.e. Precision@5
   under an ideal ranking, and reported alongside the measured value as
   `mean_precision_at_5_ceiling` in `baseline.json`. On the current
   corpus the ceiling is 0.385 — already below the 80% target — so a
   measured Precision@5 should be read against the ceiling (currently
   measuring at 88% of it), not against 1.0 or the aspirational figure.
2. **The point of this harness is regression detection for ongoing
   tuning work, not a one-time bar to clear.** A PR that changes scoring
   parameters, dictionary entries, or hooks should be judged on whether
   it made *this* corpus's measured quality worse, not on whether the
   corpus happens to clear an unrelated absolute target.

### Why per-query gates, not just aggregate thresholds

The gate checks aggregate mean drops (Precision@5 / MRR / nDCG@5, each
capped at a 0.05 absolute drop) *and* per-query drops, for a reason
discovered during review: aggregate-only thresholds are evadable by
dilution. A single query's relevant result sliding from rank 1 to rank 2
(reciprocal rank 1.0 → 0.5) only moves a 13-query aggregate mean MRR by
about 0.038 — under the aggregate threshold — even though that one
query regressed. Per-query gates close this: each query's own
reciprocal rank and nDCG@5 are checked against baseline independently,
with a 0.2 max-drop threshold (loose enough to tolerate this corpus's
naturally coarse-grained jitter between adjacent low-relevance ranks,
tight enough to catch a multi-rank slide or a relevant-doc grade
downgrade). A separate, stricter rule fires whenever a query's top
relevant document falls out of the top 5 entirely (reciprocal rank drops
to exactly 0), regardless of the 0.2 threshold — this catches queries
whose baseline reciprocal rank was already below 0.2, where the
percentage-drop rule alone wouldn't trigger.

**Known limitation**: a query whose *baseline* reciprocal rank is already
0.0 (see the `T2` finding below) is not protected by either per-query
rule — both require a positive baseline value to compute a drop against.
A further regression specific to that query would not be caught by its
own gate, though it could still surface through the aggregate thresholds
if it also affects other queries.

## File layout

| Path | Contents |
|---|---|
| `tests/golden/corpus/` | 13 Markdown documents across 3 topic clusters, deliberately restricted to content with no product, business, or in-development project overlap: `search-engine/` (this repo's own stack — rust, lindera, candle, SQLite), `hunting/` (射撃/ハンドロード/狩猟, a personal hobby domain of the corpus author, not a product or business), and `garden/` (a distractor cluster with no topical overlap to the other two, used as a negative control). Two documents (`hunting/season-report-{recent,old}.md`) exist solely to exercise temporal filtering. |
| `tests/golden/queries.yaml` | 13 queries. Each has an `id`, a `category` (entity / fts-basic / vector-semantic / mixed / temporal / distractor), the `query` text, and a `relevant` list of `{doc, grade}` judgments (`doc` is a corpus-relative path, `grade` is 0–2). |
| `tests/golden/baseline.json` | The committed baseline: a `ModeReport` for the `hybrid` mode — `mean_precision_at_5`, `mean_precision_at_5_ceiling`, `mean_mrr`, `mean_ndcg_at_5`, `mean_latency_ms`, and a `per_query` array of the same metrics plus `id`, `category`, `latency_ms`, and `ranked` (the actual result list at measurement time). |
| `tests/quality_bench.rs` | Pure metric functions (`precision_at_k`, `reciprocal_rank`, `ndcg_at_k`, `oracle_precision_at_k`) and the regression gate (`check_regressions`), all unit-tested under plain `cargo test`; plus `#[ignore]`d live integration tests (`measure_hybrid`, `measure_fts_only`, `gate_against_baseline`) that drive the real `tsm` CLI. |
| `tests/quality_bench.sh` | Orchestration: builds an isolated environment, indexes the corpus, runs the two measurement passes, and runs the gate. |
| `.github/workflows/quality.yml` | CI job invoking `tests/quality_bench.sh` on PRs touching quality-relevant paths. |

## Operations guide

### Running locally

```bash
bash tests/quality_bench.sh
```

Prerequisites: `cargo build --release` (or binaries already on `PATH`), the
`ruri-v3-30m` model available (`tsm setup`, or a pre-warmed
`HF_HUB_CACHE`), and `jq`.

The script builds an isolated, gitignored environment (`.qbench/`, the
same isolation pattern used by the performance benches), copies
`tests/golden/corpus/` into it, substitutes date
placeholders (`__TODAY__` / `__1Y_AGO__`, the same mechanism as
`tests/e2e.sh`), indexes it with a real embedder, and measures two passes:

- **hybrid** — the default search path (FTS5 + vector + entity). This is
  the mode gated against the baseline.
- **fts_only** — the embedder is physically stopped (`SIGTERM`, reusing
  `tests/e2e.sh`'s `embedder.pid` crash-simulation pattern) partway
  through the run, then `--fallback fts-only` is used for a genuine
  FTS5-only measurement. Recorded for comparison, **not gated**. An
  embedder-up `--fallback fts-only` run would not be a true FTS-only
  measurement: that flag only changes error-handling behavior (whether to
  bail when the embedder is unreachable), not whether vector retrieval
  runs while the embedder is up — see the `src/searcher/retrieve.rs`
  gating logic and the `tests/quality_bench.rs` module docs.

Metric computation is pure and unit-tested under plain `cargo test`; the
live measurement and gate passes are `#[ignore]`d integration tests
invoked by the script, since they need a live daemon and a real embedder
and so cannot run in plain `cargo test`.

### Updating the baseline

When a change intentionally improves search quality, regenerate the
baseline and commit it alongside the change:

```bash
bash tests/quality_bench.sh --update-baseline
```

Do not update the baseline to make a regression pass — if the gate fails,
fix the change under review, not the baseline. `--update-baseline`
enforces this mechanically: it refuses to overwrite an existing baseline
when this run failed the gate against it (i.e. this run is itself a
regression), unless `--force` is also passed. It is not refused on a
first-time bootstrap (no prior baseline exists to protect, and the gate
necessarily fails there only because the file is missing).

### Extending the golden set

Add a document under `tests/golden/corpus/<cluster>/` (or a new cluster
directory) and, if it introduces a new query, an entry in
`tests/golden/queries.yaml` with relevance judgments against the existing
and/or new corpus. Re-run `bash tests/quality_bench.sh --update-baseline`
to establish the new baseline, and commit both the corpus/query changes
and the updated `baseline.json` together — a query set change without a
baseline update makes the next unrelated PR's gate run fail on old,
now-invalid judgments.

## Internals

### The regression gate

`check_regressions(baseline, current)` in `tests/quality_bench.rs` runs,
in order:

1. **Aggregate drop checks** — `mean_precision_at_5`, `mean_mrr`, and
   `mean_ndcg_at_5` each fail if they dropped more than 0.05 absolute
   versus baseline.
2. **Per-query "fell out of top 5"** — any query whose baseline
   reciprocal rank was positive and whose current reciprocal rank is
   exactly 0 fails, unconditionally.
3. **Per-query max-drop** — for queries that didn't fall out entirely,
   reciprocal rank and nDCG@5 are each checked against a 0.2 max-drop
   threshold from baseline, independently.
4. **Missing query** — a query present in baseline but absent from the
   current run (e.g. a query ID typo, or the query set was edited without
   updating the baseline) fails.

### Determinism

Two things make repeated measurements of an unchanged system reproducible
byte-for-byte (verified: two consecutive runs against the same baseline
report identical `P@5`/`MRR`/`nDCG@5`, differing only in latency):

- **Corpus dates**: all corpus documents share `updated: __TODAY__`
  (uniform recency means the time-decay scoring factor is a constant
  multiplier across all candidates, so it cannot perturb relative
  ranking) except two temporal-test documents, which intentionally differ
  (`__TODAY__` vs `__1Y_AGO__`) to exercise the natural-language temporal
  filter.
- **Ranking ties**: `src/searcher/rank.rs` collects candidate chunk IDs
  into a `BTreeSet` (not a `HashSet`, whose iteration order is randomized
  per process by Rust's default hasher) and breaks exact score ties
  deterministically by `(source_file, section_path)`. Without this, two
  runs of an identical query could rank an exact-score tie differently,
  which a gate comparing "did this document fall out of the top 5" cannot
  tolerate.

### Known finding: query `T2`

Query `T2` (`去年の調査`, "last year's research") measures reciprocal
rank 0.0 and Precision@5 0.0 at baseline — the query returns no results
at all, even though its gold-relevant document
(`hunting/season-report-old.md`) contains the residual keyword after
temporal-expression stripping (`調査`) both in its title and body.

This was investigated during golden-set authoring:

- The document is genuinely indexed with the expected content — verified
  directly against the SQLite `chunks_fts` table.
- Direct SQL `MATCH` against `chunks_fts` finds the document and ranks it
  **first** by `bm25()`.
- `wakachi("調査")` tokenizes identically (a single token) in both this
  document and the sibling document whose equivalent query succeeds,
  ruling out a tokenization difference.
- The exclusion reproduces in a genuine FTS-only run (embedder physically
  stopped), ruling out vector-search interference.
- Query-side token-boundary splitting (the two-character query segmenting
  into separate single-character FTS terms) was tested directly via SQL
  and ruled out — no document has isolated single-character tokens for
  this to match against regardless.

The exclusion happens somewhere between the FTS5 SQL match (which finds
and top-ranks the chunk) and the final CLI-returned ranked list — i.e.
within `rank.rs`'s metadata JOIN, RRF fusion, or per-document
diversification — but the exact mechanism was not isolated. This is
recorded as-measured (not worked around) because it's real, useful
signal: a literally-matching, correctly-tokenized, top-`bm25`-ranked
document being invisible to the CLI search pipeline is exactly the kind
of behavior this harness exists to surface. Per the "why per-query gates"
section above, this query's own gate provides no protection against
further regression (baseline reciprocal rank is already 0), so a fix or a
further regression here would currently only be visible by inspecting
`T2`'s measured values directly, not via a gate failure.

## Implementation reference

| File | Role |
|---|---|
| `tests/golden/corpus/**/*.md` | The graded document corpus |
| `tests/golden/queries.yaml` | Query set and relevance judgments |
| `tests/golden/baseline.json` | Committed measurement baseline |
| `tests/quality_bench.rs` | Metric functions, gate logic, live integration tests |
| `tests/quality_bench.sh` | Environment setup, measurement orchestration, gate invocation |
| `.github/workflows/quality.yml` | CI job |
| `src/searcher/rank.rs` | `compare_results_desc` — the deterministic tie-break this harness depends on |
| `tests/search-test-patterns.md` | Manual, exploratory query patterns against a real workspace (not this harness's corpus) |
