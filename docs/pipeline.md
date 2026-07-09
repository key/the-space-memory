# Pipeline Stages

`tsm` defines its processing as two logical pipelines — **Index** and
**Search** — each decomposed into stages. A stage boundary is drawn where
parallelism and resource constraints change, and those same boundaries are
the attachment points for plugins.

This document specifies the stages, their responsibilities and I/O, the
invariants that hold across them, and where each plugin kind hooks in. For
the cross-process data flow (which process owns each step), see
[Data Flow](data-flow.md). For the rationale and alternatives behind this
decomposition, see the pipeline-stages decision record.[^adr-0007]

## Why stages

Stage boundaries are cut at the points where **parallelism and resource
constraint change**. The Index pipeline's Prepare (parallel) / Persist
(DB-mutex serial) / Embed (GPU serial, asynchronous) boundaries coincide
with real resource limits; splitting finer only adds inter-stage overhead
without sharpening responsibility separation.

The pipeline is defined **before** the plugin API. A plugin's hook point is
equal to a stage boundary, so once the stages are fixed, the plugin API only
has to follow each stage's input/output type. Defining plugins first would
let plugin concerns distort the stages and break the parallelism design.

## Index pipeline

A synchronous core (Prepare → Persist) makes a document searchable via FTS5
the moment Persist commits. Vector embedding runs asynchronously downstream:
Persist enqueues the chunk texts, and Embed produces and writes the vectors
out of band, so indexing never blocks on the embedder.

```mermaid
flowchart LR
    P["Prepare<br/><i>parallel per file</i>"] --> W["Persist<br/><i>serial, synchronous</i>"]
    W -. "enqueue" .-> E["Embed<br/><i>serial, asynchronous</i>"]
```

| Stage | Nature | Responsibility | Input → Output |
|---|---|---|---|
| Prepare | IO/CPU bound, parallel per file | Load, frontmatter parse, chunking, metadata extraction | file → chunk set (with metadata) |
| Persist | DB-mutex bound, serial, synchronous | documents + chunks + FTS5 + entity + link writes in one transaction; the document is FTS-searchable on commit | chunk set → committed rows (FTS-ready) |
| Embed | GPU bound, serial, asynchronous | After commit, chunk texts are enqueued; the embedder produces vectors out of band and writes the vector rows | enqueued chunks → vector rows |

## Search pipeline

```mermaid
flowchart LR
    PL["Plan<br/><i>serial</i>"] --> RT["Retrieve<br/><i>FTS5 / vector parallel</i>"]
    RT --> RK["Rank<br/><i>serial</i>"]
    RK --> FM["Format<br/><i>serial, pure</i>"]
```

| Stage | Nature | Responsibility | Input → Output |
|---|---|---|---|
| Plan | serial, light | Morphological analysis, query embedding | query → query plan (tokens + query vector) |
| Retrieve | FTS5 and vector in parallel | Obtain the two candidate sets | query plan → candidate sets |
| Rank | serial | RRF fusion, time decay, status penalty, filter | candidate sets → ordered results |
| Format | serial, pure | Render to the output format (text / JSON / future MCP) | ordered results → formatted output |

## Invariants

These hold across the stage decomposition. Each is paired with the
regression it prevents.

1. **Batch granularity** — the smallest unit flowing between stages is a
   chunk set. Do not flatten to per-chunk. *(Breaking it reintroduces
   per-statement overhead and collapses throughput.)*
2. **Persist transaction boundary** — one file's relational and FTS5 writes
   are a single transaction (vector rows are written separately by Embed).
   *(Breaking it forces a per-statement fsync under WAL.)*
3. **Tokenizer consistency** — Prepare and Plan reference the same tokenizer
   implementation; replacing it requires a re-index. *(Breaking it makes
   query tokens diverge from indexed tokens, silently losing recall.)*
4. **Embed serial contract** — the embedder runs as a single serial process.
   In the Index pipeline only the Embed stage calls it; in the Search
   pipeline only the Plan stage calls it, to vectorize the query. No other
   stage or plugin invokes inference directly or in parallel. *(Breaking it
   violates the embedder's single-threaded accept contract.)*
5. **Vectors are always async** — FTS5 is available the moment Persist
   commits; vectors are produced and written afterward (best-effort
   post-commit plus backfill). *(Breaking it couples indexing throughput and
   availability to the embedder: a slow or stopped embedder would stall or
   fail indexing instead of degrading to FTS-only.)*

## Hook insertion points

Each plugin kind attaches at a specific stage boundary. This document
specifies only *where* each kind attaches. The hook contract — per-stage
I/O types, before/around/after semantics, and the error model — is defined
in the separate hook API specification.

| Plugin kind | Stage |
|---|---|
| metadata | Prepare |
| indexer (transformer) | Prepare |
| embedder | Embed |
| filter (exclusion) | Rank |
| mask (redaction) | Format |
| output (added format) | Format |
| tokenizer | Prepare + Plan (cross-cutting) |
| source (external) | outside the pipeline — converted to Markdown, then fed into Prepare |

Session ingestion is the reference implementation of the `source` kind:
`tsm ingest-session` parses Claude session JSONL, serializes it to Markdown
(`session_source::session_to_markdown`), and feeds it into the same Prepare
implementation (`indexer::prepare::prepare_text`) as filesystem documents.
A `source` transform makes content decisions only; chunk boundaries are
owned by the markdown chunker. Per-source participation in the side indexes
(entity graph, doc links, dictionary candidates) is an explicit
`SourcePolicy` carried through Prepare into Persist.

## Failure behavior

**Visibility is fail-closed.** A visibility plugin that fails withholds the
affected results rather than passing them through; fail-open is prohibited.
`.tsmignore` is applied at a gate *before* Prepare, so excluded files never
enter the DB in the first place.

If the embedder is unavailable, Persist still commits and the document is
searchable via FTS5; the missing vectors are filled later by backfill.
Indexing never fails because embedding failed.

The per-hook error model — how a failing hook falls back and what context is
logged — is part of the hook contract and is specified in the separate hook
API specification, not here.

## Related

- Rationale and alternatives for the two-pipeline decomposition.[^adr-0007]
- Process roles and responsibilities.[^adr-0001]
- Embedder binary consolidation — origin of the Embed serial contract.[^adr-0005]
- [Data Flow](data-flow.md) — cross-process data flow that these logical
  stages run within

[^adr-0007]: [ADR-0007: Pipeline stages](../decisions/0007-pipeline-stages.md)
[^adr-0001]: [ADR-0001: Process roles and responsibilities](../decisions/0001-process-roles-and-responsibilities.md)
[^adr-0005]: [ADR-0005: Embedder binary consolidation](../decisions/0005-embedder-binary-consolidation.md)
