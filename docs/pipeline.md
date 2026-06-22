# Pipeline Stages

`tsm` defines its processing as two logical pipelines — **Index** and
**Search** — each decomposed into stages. A stage boundary is drawn where
parallelism and resource constraints change, and those same boundaries are
the attachment points for plugins.

This document specifies the stages, their responsibilities and I/O, the
invariants that hold across them, and where each plugin kind hooks in. For
the cross-process data flow (which process owns each step), see
[Data Flow](data-flow.md). For the rationale and alternatives behind this
decomposition, see [ADR-0007](../decisions/0007-pipeline-stages.md).

## Why stages

Stage boundaries are cut at the points where **parallelism and resource
constraint change**. The Index pipeline's Prepare (parallel) / Embed (GPU
serial) / Persist (DB-mutex serial) boundaries coincide with real resource
limits; splitting finer only adds inter-stage overhead without sharpening
responsibility separation.

The pipeline is defined **before** the plugin API. A plugin's hook point is
equal to a stage boundary, so once the stages are fixed, the plugin API only
has to follow each stage's input/output type. Defining plugins first would
let plugin concerns distort the stages and break the parallelism design.

## Index pipeline

```mermaid
flowchart LR
    P["Prepare<br/><i>parallel per file</i>"] --> E["Embed<br/><i>serial</i>"]
    E --> W["Persist<br/><i>serial</i>"]
```

| Stage | Nature | Responsibility | Input → Output |
|---|---|---|---|
| Prepare | IO/CPU bound, parallel per file | Load, frontmatter parse, chunking, metadata extraction | file → chunk set (with metadata) |
| Embed | GPU bound, serial (embedder contract) | Embedder invocation | chunk set → chunk set with vectors |
| Persist | DB-mutex bound, serial | FTS5 / vector / metadata writes (1 file = 1 transaction) | chunk set → committed rows |

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
2. **Persist transaction boundary** — 1 file = 1 transaction. *(Breaking it
   forces a per-statement fsync under WAL.)*
3. **Tokenizer consistency** — Prepare and Plan reference the same tokenizer
   implementation; replacing it requires a re-index. *(Breaking it makes
   query tokens diverge from indexed tokens, silently losing recall.)*
4. **Embed serial contract** — the embedder is invoked only from the Embed
   stage; no other stage or plugin calls it directly. *(Breaking it violates
   the embedder's single-threaded accept contract.)*

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

## Failure behavior

Hook failures fall back through a per-kind hierarchy and never halt the
pipeline:

```text
user plugin → default plugin (per kind) → Rust built-in fallback
```

On error, the plugin name, file path, and error are logged and processing
continues; indexing and search as a whole do not stop.

**Visibility is fail-closed.** A visibility plugin that fails withholds the
affected results rather than passing them through; fail-open is prohibited.
`.tsmignore` is applied at a gate *before* Prepare, so excluded files never
enter the DB in the first place.

## Related

- [ADR-0007](../decisions/0007-pipeline-stages.md) — rationale and
  alternatives for the two-pipeline decomposition
- [ADR-0001](../decisions/0001-process-roles-and-responsibilities.md) —
  process roles and responsibilities
- [ADR-0005](../decisions/0005-embedder-binary-consolidation.md) — embedder
  binary consolidation (origin of the Embed serial contract)
- [Data Flow](data-flow.md) — cross-process data flow that these logical
  stages run within
