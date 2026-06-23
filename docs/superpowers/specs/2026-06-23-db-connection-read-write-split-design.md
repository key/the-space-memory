# Design: tsmd read/write DB connection split

## Problem

`tsm status` and `tsm doctor` freeze while a reindex (`tsm reindex {all|fts|vectors}`)
is running.

### Root cause

`tsmd` holds a single `Arc<Mutex<Connection>>` (`src/bin/tsmd/daemon_mode.rs:96`).
`handle_client` locks this mutex for *every* request that is not specially handled
(Reindex / Reload), including read-only `Status`, `Doctor`, and `Search`
(`daemon_mode.rs:420-423`). All DB access in the daemon is therefore serialized
onto one connection behind one mutex.

Reindex runs in a background thread and releases the lock between batches, but:

- `yield_to_search` only yields to `Search` requests (the `search_active`
  counter). `Status` / `Doctor` never increment that counter, so reindex never
  yields to them.
- `std::sync::Mutex` is unfair, so a tight batch-relock loop can starve
  `Status` / `Doctor` even in the gaps between batches.
- Some reindex steps hold the lock for a long single operation —
  `DELETE FROM chunks_vec; DELETE FROM chunks_vec_skip;` in one lock
  (`backfill.rs`), and the first FTS batch (`is_first`) drop/rebuild.

WAL mode is already enabled (`src/db.rs:139`). SQLite in WAL supports multiple
concurrent readers alongside a single writer, so the database layer is **not**
the constraint — the in-process single-connection mutex is. The fix lives in
the daemon's connection architecture, not in SQLite configuration.

## Goals

- `Status` / `Doctor` / `Search` respond promptly while a reindex or backfill is
  running.
- Preserve the single-writer invariant and all existing transaction boundaries.

## Non-goals

- Changing indexing semantics, ordering, transaction boundaries, or the
  vectors-always-async contract (preserved from PR #238 / ADR-0007).
- Multi-process or multi-writer access to the DB (preserved from ADR-0001/0002:
  `tsmd` remains the sole DB owner).
- Tuning embedder throughput or backfill scheduling beyond removing the now-dead
  DB-lock yield mechanism.

## Design

### Connection model

```text
tsmd daemon (sole DB owner — unchanged)
├── writer: Arc<Mutex<Connection>>           existing; ALL writes serialize here
└── readers: fixed read-only pool of N conns  new; read-only requests only
```

- The reader pool is **read-only**. Each reader connection is opened
  `READ_WRITE` and then set `PRAGMA query_only=ON`.
  - Rationale: a `SQLITE_OPEN_READ_ONLY` handle on a WAL database must still
    write `-shm`/`-wal` and can fail to open a hot WAL with
    `SQLITE_READONLY_RECOVERY`, falsely blocking startup. Opening `READ_WRITE`
    avoids that; `query_only` enforces the read-only contract at the handle.
- `busy_timeout` is set on the writer **and** all reader connections. Today
  `apply_pragmas` (`src/db.rs:137`) sets only `journal_mode=WAL` and
  `foreign_keys=ON`. Adding `busy_timeout` turns the rare `SQLITE_BUSY` (WAL
  checkpoint, writer-vs-writer) into a bounded retry instead of an error.
- Pool size is a fixed `N` (default 4), configurable. Implementation is a
  self-contained pool over `Vec<Mutex<Connection>>` with checkout/return
  semantics — no external crate dependency.

### Request routing (`handle_client`)

| Class | Requests | Connection |
|---|---|---|
| Write | Index, IngestSession, VectorFill, ImportWordnet, Rebuild | writer mutex |
| Control (unchanged special handling) | Reindex (respond → bg thread), Reload (SIGHUP) | — |
| Read | Status, Doctor, Search, Ping | reader pool |

Reindex / backfill keep running on the **writer** connection. They are writes
and must remain on the single writer; this is what preserves PR #238's Persist
transaction boundary and Embed serial contract. Only read-only requests move to
the reader pool.

### `search_active` retirement

Once `Search` reads from the reader pool, `search` vs `reindex`/`backfill` no
longer contend for a DB lock (WAL gives the reader a consistent snapshot). The
`search_active` counter, `SearchActiveGuard`, and `yield_to_search` exist solely
to mitigate that now-removed contention, so they are **removed** entirely along
with all call sites.

Risk to verify during implementation: `search_active`/`yield_to_search` is a
DB-lock yield only; it does not pace the embedder (query embedding queued behind
backfill batch embedding goes through `embedder.sock`, a separate layer). Confirm
no embedder-pacing behavior is silently coupled to it before removing. If a
load-throttling need surfaces later, reintroduce it deliberately as an explicit
mechanism — do not keep it half-meaning here.

### Error handling

- Reader pool exhaustion (all N checked out): block briefly until a reader is
  returned (checkout/return). A slow `Search` cannot starve `Status`/`Doctor`
  indefinitely as long as `N > 1`.
- `SQLITE_BUSY` on any connection: retried via `busy_timeout`.
- A write attempted on a `query_only` reader fails fast (guards against
  misrouting a write request to the read pool); covered by a test.

## Layer-separation constraints (must not break)

1. **Inter-process (ADR-0001/0002):** `tsmd` is the sole DB owner; embedder and
   watcher never touch the DB. The reader pool lives *inside* tsmd, so this holds.
2. **Indexer pipeline (ADR-0007 / PR #238):** Prepare (no DB) → Persist (one
   transaction) → Embed (async vector writes). No indexer write path is routed
   through readers; `backfill_*` / `rebuild_fts*` public paths (preserved via
   re-export in #238) are unchanged.

The new ADR frames this as an **extension** of the tsmd DB-ownership layer:
within the single owner, split connections into one writer + a read-only pool.
It does not contradict ADR-0001/0002 or ADR-0007.

## Testing

- Reindex in progress → `Status` and `Doctor` respond promptly (the core
  regression test for this work).
- A reader's `SELECT COUNT(*) FROM chunks_vec` during
  `DELETE FROM chunks_vec` returns a snapshot value, not an error (verifies WAL
  isolation across the vec0 virtual table's shadow tables — the one non-obvious
  interaction).
- A `query_only` reader rejects a write.
- Pool checkout/return and the size bound (`N` honored; exhaustion blocks then
  recovers).

## Deliverables

1. Implementation: reader pool, request routing, `search_active` removal,
   `busy_timeout` (+ reader pragmas).
2. ADR documenting the reader/writer split policy (ADR-0001/0002 lineage; number
   assigned from `main` at decision time).
3. CLAUDE.md / README.md / README.ja.md sync (connection model description, new
   config key for pool size).

## Sequencing

PR #238 (indexer decomposition) is merged to `origin/main` (`d8b6c5d`), so the
timing gate from the implementation plan is satisfied. Implementation branches
from `origin/main`. This work touches `daemon_mode.rs`, `backfill.rs`, `db.rs`,
`daemon.rs`, and the `run_status`/`run_doctor` call sites — not `indexer/*` — so
overlap with #238 is limited to the stable re-exported `indexer::backfill_*` /
`indexer::rebuild_fts_*` paths.
