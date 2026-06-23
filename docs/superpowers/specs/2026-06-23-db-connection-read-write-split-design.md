# Design: tsmd read/write connection split + write fairness

## Problem

Two symptoms while a reindex (`tsm reindex {all|fts|vectors}`) is running:

1. `tsm status` and `tsm doctor` freeze (read blocked by writes).
2. A file/interactive `tsm index` is starved by the reindex (write blocked by writes).

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
- For **write-vs-write** (symptom 2): a watcher/CLI `Index` competes for the same
  writer mutex as reindex. The FTS reindex batch is 1000 chunks
  (`REINDEX_FTS_BATCH_SIZE`), so an `Index` waits up to a whole batch, and the
  unfair mutex lets reindex re-grab ahead of it.

WAL mode is already enabled (`src/db.rs:139`). SQLite in WAL supports multiple
concurrent readers alongside a single writer, so the read-side constraint is
**not** the database but the in-process single-connection mutex. The write side
is inherently serial (SQLite allows one writer per file — see Non-goals), but its
*fairness* can be fixed. Both fixes live in the daemon's connection/scheduling
architecture, not in SQLite configuration.

## Goals

- `Status` / `Doctor` / `Search` respond promptly while a reindex or backfill is
  running.
- Read-only requests run **truly concurrently across threads**, bounded by the
  reader pool size — not serialized through one connection.
- A file/interactive `Index` **preempts an in-progress reindex within one batch**,
  rather than waiting for the reindex to finish.
- Preserve the single-writer invariant and all existing transaction boundaries.

## Non-goals

- Changing indexing semantics, ordering, transaction boundaries, or the
  vectors-always-async contract (preserved from PR #238 / ADR-0007).
- Multi-process or multi-writer access to the DB (preserved from ADR-0001/0002:
  `tsmd` remains the sole DB owner).
- **Parallel writes.** SQLite allows one write transaction per file; the writer
  stays single and serial. This design improves write *fairness*, not write
  throughput. No async write queue or dedicated writer thread (rejected in the
  ADR — they only buy async hand-off, unneeded when synchronous writes are
  responsive enough).
- Tuning embedder throughput.

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
- Pool size is a fixed `N`, defaulting to the CPU core count
  (`std::thread::available_parallelism`, clamped to a sane floor) and overridable
  via config. `N` is the cap on concurrent reads: beyond the core count, extra
  reader threads cannot make real parallel progress anyway, so a core-count
  default maximizes true concurrency without unbounded connection/fd/`-shm`
  growth under a read flood. Implementation is a self-contained pool over
  `Vec<Mutex<Connection>>` with checkout/return semantics — no external crate
  dependency.

### Why a pool (the single-thread-per-connection constraint)

`tsmd` already spawns a thread per client (`daemon_mode.rs:279`), so read
requests already arrive on separate threads. WAL imposes no contention *between*
reader connections — each reads a consistent snapshot in parallel. The one limit
is that a single `Connection` cannot be used by two threads at once (concurrent
use serializes on SQLite's internal mutex). The pool hands each concurrent reader
its own connection, so `N` independent connections give `N`-way real parallel
reads.

### Request routing (`handle_client`)

Routing is driven by a single read/write classification **carried by the request
itself**, not by a per-variant `match`. `DaemonRequest` gains an `is_read_only()`
(equivalently `access_mode()`) method, and `handle_client` selects the connection
purely from it:

- read-only → reader pool
- write → writer mutex

**All read-only requests are treated identically**: the same reader pool, no
per-request special-casing, and no priority ordering between them. A future
read-only request is routed to the reader pool the moment its classification says
read-only — there is no enumeration in `handle_client` to keep in sync. This
structurally prevents the regression where a newly added read request is
forgotten, serializes on the writer, and freezes again.

Classification of the current variants:

| Access | Requests |
|---|---|
| read-only (no DB write on the daemon path) | Status, Doctor, Search, Ping, Shutdown, Rebuild, DictUpdate |
| write | Index, IngestSession, VectorFill, ImportWordnet |
| control (kept as today's special handling, before classification) | Reindex (respond → bg thread), Reload (SIGHUP) |

(`Rebuild` / `DictUpdate` are refused while the daemon is active and touch no DB,
so they are read-only-classed for routing; `Shutdown` only flips a flag.)

Reindex / backfill keep running on the **writer** connection. They are writes
and must remain on the single writer; this is what preserves PR #238's Persist
transaction boundary and Embed serial contract. Only read-only requests move to
the reader pool.

`Reindex` and `Reload` are handled by their existing special-case branches
(immediate response + background thread; SIGHUP to the watcher) *before* the
read/write classification is consulted — they are neither pool reads nor writer
writes in `handle_client`.

`is_read_only` matches **every** `DaemonRequest` variant exhaustively (no
wildcard arm). Adding a new request variant then fails to compile until it is
explicitly classified, so the "all reads treated the same" routing can never
silently miss a new read request.

### Write fairness: `search_active` (reads) → `writes_pending` (writes)

The fairness counter flips sides. Today `search_active` makes reindex/backfill
yield to in-flight **reads**. Once reads move to the reader pool, reindex no
longer contends with them, so that direction is dead. But reindex *does* still
contend with **writes** (symptom 2), which had no yield at all. So:

- **Remove** `search_active`, `SearchActiveGuard`, `yield_to_search` and all call
  sites (reads no longer need a DB-lock yield).
- **Add** the mirror: a `writes_pending: Arc<AtomicUsize>` counter. A write
  request increments it *before* attempting `conn.lock()` and decrements when
  done (`PendingWriteGuard`, mirror of `SearchActiveGuard`). reindex/backfill
  call `yield_to_pending_writes(&writes_pending)` before grabbing each batch: if
  a write is pending, reindex does **not** re-acquire the mutex — it steps back
  so the waiting `Index` gets the lock next.

Why this beats relying on the mutex: `std::sync::Mutex` is unfair, so shrinking
the batch alone is necessary but not sufficient — reindex could re-grab ahead of
the waiter. Because reindex *voluntarily* yields while a write is pending, the
`Index` is serviced within one batch regardless of mutex fairness. Writes stay
synchronous (the caller waits for its ack over the socket); no async queue.

**Batch size is now a config knob.** `REINDEX_FTS_BATCH_SIZE` (currently a
`const` of 1000) becomes `reindex_fts_batch_size`, overridable like
`reader_pool_size`. Smaller batch ⇒ shorter single lock hold ⇒ finer preemption
granularity, at the cost of more per-batch fsync (WAL). Default is a moderate
middle ground (proposed **200**), to be confirmed against ADR-0007's ≤5%
full-reindex throughput gate by measurement. `DELETE FROM chunks_vec` (vectors
reindex) is one statement / brief hold — not batched, unaffected.

Risk to verify during implementation: the old `yield_to_search` was a DB-lock
yield only; it did not pace the embedder (`embedder.sock` is a separate layer).
The new `yield_to_pending_writes` is likewise purely a writer-mutex yield. Keep
it that way — do not couple embedder pacing to it.

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
- `is_read_only` classifies every variant as expected (read-only requests →
  reader, writes → writer). The exhaustive match is enforced by the compiler;
  the test pins the read/write verdict per current variant.
- `PendingWriteGuard` increments/decrements `writes_pending` (RAII, mirror of the
  old `SearchActiveGuard` test).
- `yield_to_pending_writes` returns immediately when `writes_pending == 0` and
  waits while it is > 0 (and on SHUTDOWN).
- E2E: a `tsm index` issued mid-reindex completes promptly (write preemption),
  alongside the status/doctor responsiveness check.

## Deliverables

1. Implementation: reader pool, request routing, `busy_timeout` (+ reader
   pragmas), `search_active` → `writes_pending` flip with `yield_to_pending_writes`,
   and the `reindex_fts_batch_size` config knob.
2. ADR-0015 documenting both decisions (read/write split + write fairness).
   Ships as its own PR, Accepted before the implementation PR merges.
3. CLAUDE.md / README.md / README.ja.md sync (connection model + write-fairness
   description, new config keys `reader_pool_size` and `reindex_fts_batch_size`).

## Sequencing

PR #238 (indexer decomposition) is merged to `origin/main` (`d8b6c5d`), so the
timing gate from the implementation plan is satisfied. Implementation branches
from `origin/main`. This work touches `daemon_mode.rs`, `backfill.rs`, `db.rs`,
`daemon.rs`, and the `run_status`/`run_doctor` call sites — not `indexer/*` — so
overlap with #238 is limited to the stable re-exported `indexer::backfill_*` /
`indexer::rebuild_fts_*` paths.
