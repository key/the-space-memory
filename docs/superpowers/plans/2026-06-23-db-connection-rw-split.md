# tsmd read/write DB connection split — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** During a reindex, stop `tsm status`/`doctor`/`search` from freezing (read side) and let a file/interactive `tsm index` preempt the reindex within one batch (write side).

**Architecture:** `tsmd` keeps one writer `Arc<Mutex<Connection>>` (all writes serialize here, unchanged — SQLite is single-writer). A new fixed-size pool of `query_only` read connections serves every read-only request; `handle_client` routes by a single `DaemonRequest::is_read_only()` classification. WAL (already enabled) gives readers consistent snapshots concurrent with the writer. The `search_active`/`yield_to_search` read-yield is **flipped** into a `writes_pending`/`yield_to_pending_writes` write-yield: reindex/backfill steps back when an `Index` is waiting. The FTS reindex batch size becomes a config knob.

**Tech Stack:** Rust, rusqlite (bundled SQLite, WAL), std threads + `Mutex`/`Condvar` + `AtomicUsize`. No new external crates.

## Global Constraints

- License: MIT. No new dependencies (use std only for the pool). Exact version pinning for any Cargo change (none expected).
- Documentation/comments in English; ADR body in Japanese (terms/paths/code in English) per `decisions/README.md`.
- Coverage ≥ 90% on covered modules (`cargo llvm-cov --fail-under-lines 90`); new pub functions need unit tests.
- `cargo clippy -- -D warnings` and `cargo fmt --check` clean; `npx jscpd` ≤ 5%; `lizard ... -Tcyclomatic_complexity=15 -w` no new warnings.
- Preserve PR #238 invariants: indexer Persist single-transaction boundary, Embed serial contract, and the `indexer::backfill_*` / `indexer::rebuild_fts_*` public paths. No indexer write path may run on a reader connection.
- Preserve ADR-0001/0002: `tsmd` is the sole DB owner. The reader pool lives inside tsmd.
- TDD: Red → Green → Refactor. DB tests use a tempfile DB (WAL needs a real file; `:memory:` cannot be shared across connections).
- prek pre-commit/pre-push gates must pass; never bypass (`PREK_ALLOW_NO_CONFIG=1` is forbidden).
- Branch: `feat/db-connection-rw-split` (already created off `origin/main`, which includes #238).

---

## File Structure

- `decisions/0015-read-write-connection-split.md` — **Create.** ADR documenting the policy.
- `src/db.rs` — **Modify.** Add `busy_timeout` to `apply_pragmas`; add `get_read_connection()` (READ_WRITE + `query_only`).
- `src/read_pool.rs` — **Create.** `ReadPool` (fixed-size pool of read connections, checkout/return). Library module so it is unit-testable.
- `src/lib.rs` — **Modify.** `pub mod read_pool;`.
- `src/daemon_protocol.rs` — **Modify.** Add `DaemonRequest::is_read_only()` (exhaustive match) + tests.
- `src/config.rs` — **Modify.** Add `reader_pool_size` (default CPU cores) and `reindex_fts_batch_size` (default 200) — both field/env/file/merge/accessor.
- `src/bin/tsmd/daemon_mode.rs` — **Modify.** Build the pool; route reads to it in `handle_client`; flip `search_active`→`writes_pending` (increment for write requests).
- `src/bin/tsmd/backfill.rs` — **Modify.** Replace `SearchActiveGuard`/`yield_to_search` with `PendingWriteGuard`/`yield_to_pending_writes`; use `config::reindex_fts_batch_size()` (Task 7).
- `tests/e2e.sh` — **Modify.** Regression checks: status responsive during reindex; `index` preempts reindex.
- `CLAUDE.md`, `README.md`, `README.ja.md` — **Modify.** Document the connection model, write fairness, and new config keys.

Task order: ADR (separate PR) → db pragmas/reader conn → pool → classification → config → wire routing → flip `search_active`→`writes_pending` → e2e → docs. Lib pieces land before the bin wiring that consumes them.

---

### Task 1: ADR-0015 — read/write connection split

> **Execution note (decided 2026-06-23):** This task ships as its **own PR**,
> separate from the feature branch, and must be **Accepted (merged) before** the
> feature PR (Tasks 2–9) merges. Subagents executing Tasks 2–9 must NOT create
> the ADR — it is handled out of band on branch
> `docs/adr-0015-read-write-connection-split`.

**Files:**
- Create: `decisions/0015-read-write-connection-split.md`
- Modify: `decisions/README.md` (index table — add the row)

**Interfaces:**
- Consumes: nothing.
- Produces: the decision record the rest of the plan implements. No code symbols.

Get the next ADR number from `main` before writing (renumbering may be in flight). At plan time the latest is 0014, so this is **0015**; if `main` already has a 0015, bump to the next free number and rename the file.

- [ ] **Step 1: Write the ADR file**

Create `decisions/0015-read-write-connection-split.md` (Japanese body, English terms):

```markdown
---
status: proposed
created: 2026-06-23
updated: 2026-06-23
---

# ADR-0015: 読み取り / 書き込み DB 接続の分離

- **Deciders**: key
- **Related**: ADR-0001（プロセス役割、deprecated）, ADR-0002（watcher 統合）, ADR-0007（パイプライン段）

## Context

`tsmd` は単一の `Arc<Mutex<Connection>>` で DB へアクセスし、`handle_client` は
Reindex / Reload 以外の全リクエスト（読み取りの `Status` / `Doctor` / `Search` を含む）で
この mutex を取得する。reindex / backfill は同じ mutex をバッチごとに取り直すため、
`Status` / `Doctor` が長時間応答できなくなる（mutex は unfair、かつ
`yield_to_search` は `Search` にしか譲らない）。

WAL は既に有効で、SQLite は「複数リーダー + 単一ライター」を並行で扱える。
ボトルネックは DB ではなく in-process の単一接続 mutex である。

## Decision

`tsmd`（単一 DB オーナーであることは不変）の内部接続を 2 種に分ける。

| 接続 | 用途 |
|---|---|
| writer: `Arc<Mutex<Connection>>` | 全書き込み（Index/IngestSession/VectorFill/ImportWordnet, reindex, backfill）を直列化 |
| reader pool: `query_only` 接続 N 本 | 読み取りリクエスト専用。WAL スナップショットを並行読み |

- 振り分けは `DaemonRequest::is_read_only()`（全変種を網羅する match）で決定する。
  読み取りは全て同一の reader pool へ流し、種別ごとの特別扱いはしない。
- reader 接続は `READ_WRITE` で開き `PRAGMA query_only=ON` を適用する
  （`SQLITE_OPEN_READ_ONLY` は hot WAL で `SQLITE_READONLY_RECOVERY` になり起動を阻む）。
- writer / reader 双方に `busy_timeout` を設定する。
- pool サイズ N は既定で CPU コア数（config で上書き可）。N が同時並行読みの上限。

書き込みは依然 SQLite の単一ライター制約により直列。reader pool は書き込みを一切担わない
（ADR-0007 の Persist トランザクション境界・Embed serial contract を侵さない）。

## Rationale

- **代替案: `yield_to_search` を汎用カウンタに拡張** — 単一接続のまま。
  `DELETE FROM chunks_vec` や FTS 初回 rebuild の長時間ロックを解消できず、却下。
- **代替案: リクエストごとに接続を open** — 毎回 shm-map + PRAGMA コスト。固定 pool を採用。
- **代替案: 書き込みの並列化（DB 分割 / 別エンジン）** — SQLite は単一ファイルで
  並行書き込み不可。DB 分割はファイル跨ぎ原子性を失う。組込み方針に反するため却下。
- reader を read-only にするのは `query_only`。これで書き込みリクエストの誤ルートを即検出できる。

## Consequences

### Positive

- reindex / backfill 実行中も `Status` / `Doctor` / `Search` が即応答する。
- 読み取りが N 本で真に並行する。

### Negative

- 接続が writer 1 + reader N 本に増え、各 reader が WAL の `-shm` をマップする。
- 重い `Search` が pool を占有すると軽い `Status` が短時間待つ（N > 1 で緩和）。
```

- [ ] **Step 2: Add the index row to `decisions/README.md`**

Insert after the 0014 row (keep table alignment):

```markdown
| [0015](./0015-read-write-connection-split.md) | 読み取り / 書き込み DB 接続を分離し reader pool で並行読みする | Proposed | 2026-06-23 |
```

- [ ] **Step 3: Lint the Markdown**

Run: `rumdl check decisions/0015-read-write-connection-split.md decisions/README.md`
Expected: no errors (fix any reported).

- [ ] **Step 4: Commit**

```bash
git add decisions/0015-read-write-connection-split.md decisions/README.md
git commit -m "docs(adr): propose ADR-0015 read/write connection split"
```

> Status stays `Proposed` until merge; flip to `Accepted` (+ `updated` + index row) in the final pre-merge commit, per `decisions/README.md` and the team convention.

---

### Task 2: `busy_timeout` + read-only connection opener (`db.rs`)

**Files:**
- Modify: `src/db.rs` (`apply_pragmas` ~line 137; add `get_read_connection` near `get_connection` ~line 252)
- Test: `src/db.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: existing `apply_pragmas`, `Connection`, `OpenFlags`, `ensure_vec_extension`.
- Produces:
  - `pub const BUSY_TIMEOUT_MS: u64 = 5000;`
  - `pub fn get_read_connection(db_path: &Path) -> anyhow::Result<Connection>` — opens READ_WRITE, applies pragmas + `query_only=ON`, runs the idempotent column migrations (so a reader opened before the writer still sees the columns), returns a connection that rejects writes.

- [ ] **Step 1: Write the failing tests**

Add to `src/db.rs` tests:

```rust
#[test]
fn test_busy_timeout_is_set() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    init_db(&path).unwrap();
    let conn = get_connection(&path).unwrap();
    let ms: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ms, BUSY_TIMEOUT_MS as i64);
}

#[test]
fn test_read_connection_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    init_db(&path).unwrap();
    let reader = get_read_connection(&path).unwrap();
    let err = reader
        .execute("INSERT INTO documents (path, content_hash) VALUES ('x', 'y')", [])
        .unwrap_err();
    assert!(
        err.to_string().contains("readonly") || err.to_string().to_lowercase().contains("read"),
        "expected a read-only rejection, got: {err}"
    );
}

#[test]
fn test_read_connection_sees_snapshot_during_writer_delete() {
    // Reader must read chunks_vec without error while the writer holds a
    // transaction that has DELETEd from it (WAL snapshot isolation across the
    // vec0 virtual table's shadow tables).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    init_db(&path).unwrap();

    let writer = get_connection(&path).unwrap();
    writer
        .execute_batch("BEGIN; DELETE FROM chunks_vec; ")
        .unwrap(); // open write txn, not yet committed

    let reader = get_read_connection(&path).unwrap();
    let n: i64 = reader
        .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
        .expect("reader should read a snapshot, not error");
    assert_eq!(n, 0); // empty DB; the point is it returns, not the value

    writer.execute_batch("ROLLBACK;").unwrap();
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib db::tests::test_busy_timeout_is_set db::tests::test_read_connection_rejects_writes db::tests::test_read_connection_sees_snapshot_during_writer_delete`
Expected: FAIL — `get_read_connection` and `BUSY_TIMEOUT_MS` do not exist; busy_timeout assert fails.

- [ ] **Step 3: Implement**

In `src/db.rs`, change `apply_pragmas`:

```rust
/// Busy timeout (ms) applied to every connection. Turns the rare SQLITE_BUSY
/// (WAL checkpoint, writer-vs-writer) into a bounded retry instead of an error.
pub const BUSY_TIMEOUT_MS: u64 = 5000;

fn apply_pragmas(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(&format!(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout={BUSY_TIMEOUT_MS};",
    ))?;
    Ok(())
}
```

Add after `get_connection`:

```rust
/// Open a read-only connection for the daemon's reader pool.
///
/// Opened READ_WRITE (not READ_ONLY: a read-only handle on a WAL DB can fail to
/// recover a hot WAL with SQLITE_READONLY_RECOVERY), then constrained with
/// `PRAGMA query_only=ON` so writes are rejected. Runs the same idempotent
/// column migrations as `get_connection` so a reader opened before the writer
/// still sees `chunk_hash` / `metadata`.
pub fn get_read_connection(db_path: &Path) -> anyhow::Result<Connection> {
    ensure_vec_extension();
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    apply_pragmas(&conn)?;
    ensure_chunk_hash_column(&conn)?;
    ensure_metadata_column(&conn)?;
    conn.execute_batch("PRAGMA query_only=ON;")?;
    Ok(conn)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib db::tests::test_busy_timeout_is_set db::tests::test_read_connection_rejects_writes db::tests::test_read_connection_sees_snapshot_during_writer_delete`
Expected: PASS (3 passed).

- [ ] **Step 5: Commit**

```bash
git add src/db.rs
git commit -m "feat(db): add busy_timeout and query_only read-connection opener"
```

---

### Task 3: `ReadPool` library module (`src/read_pool.rs`)

**Files:**
- Create: `src/read_pool.rs`
- Modify: `src/lib.rs` (add `pub mod read_pool;`)
- Test: `src/read_pool.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `crate::db::get_read_connection`.
- Produces:
  - `pub struct ReadPool` with:
    - `pub fn new(db_path: &Path, size: usize) -> anyhow::Result<ReadPool>` — opens `max(1, size)` read connections.
    - `pub fn size(&self) -> usize`
    - `pub fn checkout(&self) -> PooledConn` — blocks until a connection is free.
  - `pub struct PooledConn<'a>` derefs to `rusqlite::Connection`; returns the connection to the pool on drop.

- [ ] **Step 1: Write the failing tests**

Create `src/read_pool.rs`:

```rust
//! Fixed-size pool of read-only SQLite connections for the daemon.
//!
//! `tsmd` already runs one thread per client, so reads arrive concurrently.
//! WAL imposes no contention between reader connections, but a single
//! `Connection` cannot be shared across threads at once. The pool hands each
//! concurrent reader its own connection, so `N` connections give `N`-way real
//! parallel reads. Writes never use this pool (see ADR-0015).

use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use rusqlite::Connection;

use crate::db;

struct Inner {
    conns: Mutex<Vec<Connection>>,
    available: Condvar,
}

pub struct ReadPool {
    inner: Arc<Inner>,
    size: usize,
}

impl ReadPool {
    pub fn new(db_path: &Path, size: usize) -> anyhow::Result<ReadPool> {
        let size = size.max(1);
        let mut conns = Vec::with_capacity(size);
        for _ in 0..size {
            conns.push(db::get_read_connection(db_path)?);
        }
        Ok(ReadPool {
            inner: Arc::new(Inner {
                conns: Mutex::new(conns),
                available: Condvar::new(),
            }),
            size,
        })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Check out a connection, blocking until one is free.
    pub fn checkout(&self) -> PooledConn {
        let mut guard = self.inner.conns.lock().expect("read pool poisoned");
        while guard.is_empty() {
            guard = self
                .inner
                .available
                .wait(guard)
                .expect("read pool poisoned");
        }
        let conn = guard.pop().expect("non-empty after wait");
        PooledConn {
            inner: Arc::clone(&self.inner),
            conn: Some(conn),
        }
    }
}

pub struct PooledConn {
    inner: Arc<Inner>,
    conn: Option<Connection>,
}

impl Deref for PooledConn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("connection present until drop")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            if let Ok(mut guard) = self.inner.conns.lock() {
                guard.push(conn);
                self.inner.available.notify_one();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        db::init_db(&path).unwrap();
        (dir, path)
    }

    #[test]
    fn test_new_opens_size_connections() {
        let (_dir, path) = temp_db();
        let pool = ReadPool::new(&path, 3).unwrap();
        assert_eq!(pool.size(), 3);
    }

    #[test]
    fn test_size_floor_is_one() {
        let (_dir, path) = temp_db();
        let pool = ReadPool::new(&path, 0).unwrap();
        assert_eq!(pool.size(), 1);
    }

    #[test]
    fn test_checkout_returns_usable_read_connection() {
        let (_dir, path) = temp_db();
        let pool = ReadPool::new(&path, 2).unwrap();
        let conn = pool.checkout();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_connection_returns_to_pool_on_drop() {
        let (_dir, path) = temp_db();
        let pool = ReadPool::new(&path, 1).unwrap();
        {
            let _c = pool.checkout(); // single connection checked out
        } // dropped, returned
        // If it was not returned, this second checkout would block forever.
        let _c2 = pool.checkout();
    }

    #[test]
    fn test_concurrent_checkouts_up_to_size() {
        let (_dir, path) = temp_db();
        let pool = Arc::new(ReadPool::new(&path, 4).unwrap());
        let mut handles = vec![];
        for _ in 0..4 {
            let p = Arc::clone(&pool);
            handles.push(std::thread::spawn(move || {
                let c = p.checkout();
                let _n: i64 = c
                    .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
                    .unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }
}
```

- [ ] **Step 2: Register the module + run tests to verify they fail**

Add to `src/lib.rs` (with the other `pub mod` lines):

```rust
pub mod read_pool;
```

Run: `cargo test --lib read_pool::`
Expected: PASS actually — this module is self-contained and the tests target the code you just wrote. If you want a true red step, comment out the body of `checkout`'s `while` loop first, watch `test_connection_returns_to_pool_on_drop`/`test_concurrent_checkouts_up_to_size` hang or fail, then restore. (TDD note: the failing-first signal here is the compile error before `read_pool.rs` exists.)

- [ ] **Step 3: (implementation already written in Step 1)**

The module body above is the implementation. No further code needed.

- [ ] **Step 4: Run tests + clippy**

Run: `cargo test --lib read_pool:: && cargo clippy --lib -- -D warnings`
Expected: tests PASS (5 passed); clippy clean.

- [ ] **Step 5: Commit**

```bash
git add src/read_pool.rs src/lib.rs
git commit -m "feat(db): add fixed-size read-only connection pool"
```

---

### Task 4: `DaemonRequest::is_read_only()` (`daemon_protocol.rs`)

**Files:**
- Modify: `src/daemon_protocol.rs` (add an `impl DaemonRequest` block after the enum, ~line 80)
- Test: `src/daemon_protocol.rs` `#[cfg(test)] mod tests` (create if absent)

**Interfaces:**
- Consumes: the `DaemonRequest` enum.
- Produces: `pub fn is_read_only(&self) -> bool` on `DaemonRequest`.

Classification rule: returns `true` when the daemon's handling performs **no DB write**. `Reindex`/`Reload` are intercepted in `handle_client` before this is consulted; they are classified `false` (write/control class) so the routing fallback is safe, but the value is never used for them in the live daemon.

- [ ] **Step 1: Write the failing tests**

Add to `src/daemon_protocol.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_only_requests() {
        assert!(DaemonRequest::Status.is_read_only());
        assert!(DaemonRequest::Ping.is_read_only());
        assert!(DaemonRequest::Shutdown.is_read_only());
        assert!(DaemonRequest::Doctor { format: "text".into() }.is_read_only());
        assert!(DaemonRequest::Search {
            query: "q".into(),
            top_k: 5,
            format: "text".into(),
            include_content: None,
            after: None,
            before: None,
            recent: None,
            year: None,
            fallback: None,
            paths: None,
        }
        .is_read_only());
        // Refused-while-active control requests touch no DB on the daemon path.
        assert!(DaemonRequest::Rebuild.is_read_only());
        assert!(DaemonRequest::DictUpdate { threshold: 0, apply: true }.is_read_only());
    }

    #[test]
    fn test_write_requests() {
        assert!(!DaemonRequest::Index { files: vec![] }.is_read_only());
        assert!(!DaemonRequest::IngestSession { session_file: "s".into() }.is_read_only());
        assert!(!DaemonRequest::VectorFill { batch_size: 1 }.is_read_only());
        assert!(!DaemonRequest::ImportWordnet { wordnet_db: "w".into() }.is_read_only());
        // Intercepted before classification; classified write/control.
        assert!(!DaemonRequest::Reindex { kind: ReindexKind::All }.is_read_only());
        assert!(!DaemonRequest::Reload.is_read_only());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib daemon_protocol::tests`
Expected: FAIL — `is_read_only` not defined.

- [ ] **Step 3: Implement**

Add after the `DaemonRequest` enum in `src/daemon_protocol.rs`:

```rust
impl DaemonRequest {
    /// Whether the daemon serves this request without writing to the DB.
    ///
    /// Drives connection routing in `tsmd`: `true` → read-only pool, `false` →
    /// writer. The match is exhaustive (no wildcard) on purpose: adding a new
    /// variant fails to compile until it is explicitly classified, so a new
    /// read request can never silently route onto the writer and freeze again.
    pub fn is_read_only(&self) -> bool {
        match self {
            // Genuine reads.
            DaemonRequest::Search { .. }
            | DaemonRequest::Doctor { .. }
            | DaemonRequest::Status
            | DaemonRequest::Ping => true,
            // No DB access on the daemon path (flag flip / refused-while-active).
            DaemonRequest::Shutdown
            | DaemonRequest::Rebuild
            | DaemonRequest::DictUpdate { .. } => true,
            // Writes.
            DaemonRequest::Index { .. }
            | DaemonRequest::IngestSession { .. }
            | DaemonRequest::VectorFill { .. }
            | DaemonRequest::ImportWordnet { .. } => false,
            // Intercepted by handle_client before classification; write/control class.
            DaemonRequest::Reindex { .. } | DaemonRequest::Reload => false,
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib daemon_protocol::tests`
Expected: PASS (2 passed).

- [ ] **Step 5: Commit**

```bash
git add src/daemon_protocol.rs
git commit -m "feat(protocol): classify DaemonRequest read vs write"
```

---

### Task 5: config keys `reader_pool_size` + `reindex_fts_batch_size` (`config.rs`)

**Files:**
- Modify: `src/config.rs` (ConfigFile ~line 191; ResolvedConfig field ~line 245; `from_env` ~line 394; struct literal ~line 490; reload merge ~line 770; accessor ~line 904; `REINDEX_FTS_BATCH_SIZE` const at line 18)
- Modify: `tsm.toml.example` (document the keys)
- Test: `src/config.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: existing `env_parse_u64`, `ResolvedConfig`, `ConfigFile`.
- Produces:
  - `pub fn reader_pool_size() -> usize` (env `TSM_READER_POOL_SIZE` > tsm.toml `reader_pool_size` > CPU core count).
  - `pub fn reindex_fts_batch_size() -> usize` (env `TSM_REINDEX_FTS_BATCH_SIZE` > tsm.toml `reindex_fts_batch_size` > `DEFAULT_REINDEX_FTS_BATCH_SIZE = 200`). Replaces the `REINDEX_FTS_BATCH_SIZE` const for the batch-loop call site.

> Default 200 (not the old 1000, not 10): smaller bounds preemption latency, larger protects full-reindex throughput. 200 is the proposed middle; confirm against ADR-0007's ≤5% gate by measurement and adjust the default if needed.

- [ ] **Step 1: Write the failing tests**

Add to `src/config.rs` tests (the file already has a `resolved_from_toml` helper):

```rust
#[test]
fn test_reader_pool_size_from_file() {
    let cfg = resolved_from_toml("reader_pool_size = 8\n");
    assert_eq!(cfg.reader_pool_size, 8);
}

#[test]
fn test_reader_pool_size_default_is_positive() {
    // No key set → defaults to CPU core count, which is always ≥ 1.
    let cfg = resolved_from_toml("");
    assert!(cfg.reader_pool_size >= 1);
}

#[test]
fn test_reindex_fts_batch_size_from_file() {
    let cfg = resolved_from_toml("reindex_fts_batch_size = 10\n");
    assert_eq!(cfg.reindex_fts_batch_size, 10);
}

#[test]
fn test_reindex_fts_batch_size_default() {
    let cfg = resolved_from_toml("");
    assert_eq!(cfg.reindex_fts_batch_size, DEFAULT_REINDEX_FTS_BATCH_SIZE);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::tests::test_reader_pool_size_from_file config::tests::test_reindex_fts_batch_size_from_file`
Expected: FAIL — fields `reader_pool_size` / `reindex_fts_batch_size` do not exist.

- [ ] **Step 3: Implement — six edits per key**

Apply edits (a)–(f) below for **both** `reader_pool_size` and `reindex_fts_batch_size` (same pattern, listed once for the pool size; mirror it for the batch size with its own env var `TSM_REINDEX_FTS_BATCH_SIZE`, default `DEFAULT_REINDEX_FTS_BATCH_SIZE`, and the doc comment "FTS reindex batch size; smaller = finer preemption, more fsync (ADR-0015)").

(a) `ConfigFile` (after `embedder_backfill_interval_secs: Option<u64>,`):

```rust
    reader_pool_size: Option<u64>,
```

(b) `ResolvedConfig` field (after `embedder_backfill_interval_secs: u64,`):

```rust
    /// Number of read-only connections in the daemon's reader pool.
    /// Default: CPU core count. Env: `TSM_READER_POOL_SIZE`. Config: `reader_pool_size`.
    /// Caps concurrent reads (see ADR-0015).
    pub reader_pool_size: usize,
```

(c) `from_env`, after the `embedder_backfill_interval_secs` block (~line 394):

```rust
        let reader_pool_size = env_parse_u64("TSM_READER_POOL_SIZE", file_cfg.reader_pool_size)
            .map(|n| n as usize)
            .filter(|&n| n > 0)
            .unwrap_or_else(default_reader_pool_size);
```

Add the default helper near the other module-level helpers:

```rust
/// Default reader pool size: the machine's parallelism, floored at 1.
fn default_reader_pool_size() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}
```

(d) Struct literal returned by `from_env` (~line 490, alongside `embedder_backfill_interval_secs,`):

```rust
            reader_pool_size,
```

(e) `reload` merge (~line 770, after the `embedder_backfill_interval_secs` merge):

```rust
        merged.reader_pool_size = merged.reader_pool_size.or(file.reader_pool_size);
```

> Note: confirm the merge operates on the `ConfigFile`-shaped optional (`merged: ConfigFile`). Mirror exactly what the adjacent `embedder_backfill_interval_secs` line does — same receiver type, same `.or(file...)` shape.

(f) Accessor (~line 904, after `embedder_backfill_interval_secs()`):

```rust
pub fn reader_pool_size() -> usize {
    resolved().reader_pool_size
}

pub fn reindex_fts_batch_size() -> usize {
    resolved().reindex_fts_batch_size
}
```

(g) Batch-size default const — replace the existing `pub const REINDEX_FTS_BATCH_SIZE: usize = 1000;` (line 18) with:

```rust
/// Default FTS reindex batch size when `reindex_fts_batch_size` is unset.
/// Smaller = finer write preemption (ADR-0015), larger = better full-reindex
/// throughput. 200 is the middle ground; tune via config + measurement.
pub const DEFAULT_REINDEX_FTS_BATCH_SIZE: usize = 200;
```

(The batch-loop call site in `backfill.rs` switches from the const to the
`config::reindex_fts_batch_size()` accessor in Task 7.)

- [ ] **Step 4: Update `tsm.toml.example`**

Add near `embedder_backfill_interval_secs`:

```toml
# Number of read-only DB connections the daemon keeps for status/doctor/search.
# Caps concurrent reads. Default: CPU core count.
# reader_pool_size = 4

# FTS reindex batch size. Smaller = a file/interactive `index` preempts an
# in-progress reindex sooner; larger = better full-reindex throughput. Default: 200.
# reindex_fts_batch_size = 200
```

- [ ] **Step 5: Run tests + verify**

Run: `cargo test --lib config:: && cargo clippy --lib -- -D warnings`
Expected: PASS (existing config tests + 4 new); clippy clean.

- [ ] **Step 6: Commit**

```bash
git add src/config.rs tsm.toml.example
git commit -m "feat(config): add reader_pool_size and reindex_fts_batch_size knobs"
```

---

### Task 6: Route reads through the pool in `handle_client` (`daemon_mode.rs`)

**Files:**
- Modify: `src/bin/tsmd/daemon_mode.rs` (`run` ~line 232/272-291; `handle_client` ~line 328-426)

**Interfaces:**
- Consumes: `the_space_memory::read_pool::ReadPool`, `config::reader_pool_size()`, `config::db_path()`, `DaemonRequest::is_read_only()`.
- Produces: a `handle_client` that selects the writer mutex or a pooled reader by classification. `search_active` is still threaded through (removed in Task 7).

This task keeps `search_active` in place (still passed to backfill) — it only adds the pool and routing. Removing the now-dead counter is Task 7, so each commit stays coherent.

- [ ] **Step 1: Build the pool in `run` and pass it to `handle_client`**

After the writer `let conn = Arc::new(Mutex::new(conn));` (line 96), add:

```rust
    // Read-only connection pool: serves Status/Doctor/Search/Ping concurrently
    // with the writer (WAL snapshots). See ADR-0015.
    let read_pool = Arc::new(
        the_space_memory::read_pool::ReadPool::new(&db_path, config::reader_pool_size())
            .context("Failed to open reader pool")?,
    );
```

In the accept loop (the `std::thread::spawn` block ~line 272-291), clone and pass it:

```rust
                let read_pool = Arc::clone(&read_pool);
```

and add `&read_pool` to the `handle_client(...)` call arguments.

- [ ] **Step 2: Add the parameter + route by classification in `handle_client`**

Add `read_pool: &Arc<the_space_memory::read_pool::ReadPool>,` to `handle_client`'s signature. Replace the final generic dispatch (currently lines 413-424: the `_guard`, `conn.lock()`, `handle_request`, `write_response`) with:

```rust
    // Track active search requests so backfill can yield (removed in a follow-up
    // once the reader pool makes this redundant).
    let _guard = if matches!(req, DaemonRequest::Search { .. }) {
        Some(backfill::SearchActiveGuard::new(search_active))
    } else {
        None
    };

    let resp = if req.is_read_only() {
        let conn = read_pool.checkout();
        daemon::handle_request(&conn, req, project_root, &SHUTDOWN)
    } else {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;
        daemon::handle_request(&conn, req, project_root, &SHUTDOWN)
    };
    write_response(stream, &resp)?;
    Ok(())
```

> `handle_request` takes `&Connection`; `&conn` works for both the `PooledConn` (via `Deref`) and the writer `MutexGuard` (via auto-deref). No change to `daemon.rs`.

- [ ] **Step 3: Build + run the full test suite**

Run: `cargo build && cargo test`
Expected: builds; all tests pass (lib + bin). The bin compiles with the new param wired through.

> Hidden-write check: after this task, `Search` runs on a `query_only` connection. If any search-time code path writes to the DB it will now fail. The e2e suite's existing search assertions (Task 8 / `bash tests/e2e.sh`) exercise the real search path through the pool and are the empirical guard for this — run e2e before declaring Task 6 done if you skipped the Step 4 smoke.

- [ ] **Step 4: Manual smoke (optional but recommended)**

Run (release): `cargo build --release` then in an indexed project: start `tsmd`, run `tsm reindex all` and immediately `tsm status` / `tsm doctor` in another shell.
Expected: status/doctor return immediately while reindex runs.

- [ ] **Step 5: Commit**

```bash
git add src/bin/tsmd/daemon_mode.rs
git commit -m "feat(tsmd): serve read-only requests from the reader pool"
```

---

### Task 7: Flip `search_active` (reads) → `writes_pending` (writes)

The fairness counter changes sides. Reads no longer need a DB-lock yield (they're
on the pool), but reindex/backfill must now yield to pending **writes** so a
file/interactive `Index` preempts reindex within one batch. This is the mirror of
the mechanism Task 6 left in place — rename + invert the polarity, then wire the
write path and switch the FTS batch to the config knob.

**Files:**
- Modify: `src/bin/tsmd/backfill.rs` (rename guard/yield, invert to `writes_pending`, use `config::reindex_fts_batch_size()`)
- Modify: `src/bin/tsmd/daemon_mode.rs` (rename atomic to `writes_pending`; in `handle_client` increment it for **write** requests, not Search)

**Interfaces:**
- Consumes: `config::reindex_fts_batch_size()`.
- Produces:
  - `pub struct PendingWriteGuard(Arc<AtomicUsize>)` with `new(&Arc<AtomicUsize>) -> Self` (RAII inc/dec) — replaces `SearchActiveGuard`.
  - `fn yield_to_pending_writes(writes_pending: &Arc<AtomicUsize>) -> bool` — replaces `yield_to_search`.
  - Pass signatures gain `writes_pending` (same position the old `search_active` had):
    - `run_backfill_pass(conn: &Arc<Mutex<Connection>>, writes_pending: &Arc<AtomicUsize>)`
    - `periodic_backfill(conn: &Arc<Mutex<Connection>>, writes_pending: &Arc<AtomicUsize>, interval_secs: u64)`
    - `run_reindex_fts_pass(conn: &Arc<Mutex<Connection>>, writes_pending: &Arc<AtomicUsize>, state_dir: &Path)`
    - `run_reindex_vectors_pass(conn: &Arc<Mutex<Connection>>, writes_pending: &Arc<AtomicUsize>, state_dir: &Path)`

Pre-check (Global Constraints): confirm the old `yield_to_search` coupled no embedder pacing (it only spin-waits on a counter + sleeps; the embedder is reached separately via `embedder::embed_via_socket`). Keep `yield_to_pending_writes` equally pure.

- [ ] **Step 1: Edit `backfill.rs` — rename + invert**

- Rename `SearchActiveGuard` → `PendingWriteGuard` (struct + `impl`s + `new`; logic identical — it just guards a different counter).
- Rename `yield_to_search` → `yield_to_pending_writes`; body unchanged (spin while the counter > 0, checking `SHUTDOWN`). Update its doc comment to "yield while a write request is pending".
- In all four pass fns, rename the `search_active` param to `writes_pending` and the `yield_to_search(search_active)` calls to `yield_to_pending_writes(writes_pending)` (call sites ~63-65, 198, 262-264; and the inner `run_backfill_pass(conn, search_active)` calls at ~133, 294 → `run_backfill_pass(conn, writes_pending)`).
- In `run_reindex_fts_pass`, replace `let batch_size = config::REINDEX_FTS_BATCH_SIZE;` with `let batch_size = config::reindex_fts_batch_size();`.
- Rename the `test_search_active_guard_raii` test → `test_pending_write_guard_raii` (same assertions, new type name).

- [ ] **Step 2: Edit `daemon_mode.rs` — rename atomic + wire write path**

- Rename `let search_active = Arc::new(AtomicUsize::new(0));` → `let writes_pending = Arc::new(AtomicUsize::new(0));` and every `Arc::clone(&search_active)` / `&search_active` → `writes_pending` (startup backfill thread, periodic backfill, accept-loop clone, reindex thread, `handle_client` signature). The pass calls keep passing it in the same position.
- In `handle_client`, replace the Task 6 read-only Search guard block + dispatch with the write-side guard:

```rust
    let resp = if req.is_read_only() {
        let conn = read_pool.checkout();
        daemon::handle_request(&conn, req, project_root, &SHUTDOWN)
    } else {
        // Mark a write pending BEFORE locking so reindex/backfill yields the
        // writer to us within one batch (mirror of the retired search yield).
        let _pending = backfill::PendingWriteGuard::new(writes_pending);
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;
        daemon::handle_request(&conn, req, project_root, &SHUTDOWN)
    };
    write_response(stream, &resp)?;
    Ok(())
```

- `AtomicUsize` import stays (still used by `writes_pending`).

- [ ] **Step 3: Build + test**

Run: `cargo build && cargo test && cargo clippy -- -D warnings`
Expected: builds clean (no unused-import / dead-code warnings); all tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/bin/tsmd/backfill.rs src/bin/tsmd/daemon_mode.rs
git commit -m "feat(tsmd): yield reindex to pending writes (flip search_active)"
```

---

### Task 8: E2E regression — reads responsive AND writes preempt during reindex

**Files:**
- Modify: `tests/e2e.sh`

**Interfaces:**
- Consumes: the built `tsm`/`tsmd` binaries and the e2e harness's existing daemon setup.
- Produces: two timed assertions in the e2e suite (read responsiveness + write preemption).

Read `tests/e2e.sh` first to match its existing helpers (daemon start/stop, project setup, assertion style, the date-placeholder convention for testdata).

- [ ] **Step 1: Add the regression check**

After the corpus is indexed and the daemon is running, add a block that kicks a reindex and times a status call (use the suite's existing assert helpers; adapt names to the file):

```bash
# --- Regression: status/doctor stay responsive during reindex (ADR-0015) ---
tsm reindex all >/dev/null 2>&1 &   # background; daemon responds immediately anyway
reindex_kick=$!
start_ns=$(date +%s%N)
tsm status >/dev/null
elapsed_ms=$(( ($(date +%s%N) - start_ns) / 1000000 ))
wait "$reindex_kick" 2>/dev/null || true
if [ "$elapsed_ms" -gt 2000 ]; then
  echo "FAIL: tsm status took ${elapsed_ms}ms during reindex (expected < 2000ms)"
  exit 1
fi
echo "PASS: status responsive during reindex (${elapsed_ms}ms)"
```

> The 2000ms bound is a generous ceiling — the freeze it guards against is multi-second to indefinite. Tune to the suite's corpus if it runs on a tiny dataset where reindex finishes instantly (in that case, also assert the reindex actually ran by checking a larger corpus or a `tsm doctor` field).

- [ ] **Step 2: Add the write-preemption check**

Set a small `reindex_fts_batch_size` (e.g. export `TSM_REINDEX_FTS_BATCH_SIZE=10` for the daemon under test, or set it in the test's tsm.toml), kick a reindex, then time a `tsm index` of one file:

```bash
# --- Regression: a file index preempts an in-progress reindex (ADR-0015) ---
tsm reindex fts >/dev/null 2>&1 &
reindex_kick=$!
start_ns=$(date +%s%N)
echo "$SOME_INDEXED_FILE" | tsm index --files-from-stdin >/dev/null
elapsed_ms=$(( ($(date +%s%N) - start_ns) / 1000000 ))
wait "$reindex_kick" 2>/dev/null || true
if [ "$elapsed_ms" -gt 3000 ]; then
  echo "FAIL: tsm index took ${elapsed_ms}ms during reindex (expected preemption < 3000ms)"
  exit 1
fi
echo "PASS: index preempts reindex (${elapsed_ms}ms)"
```

> Use a file the corpus already contains so the index is a quick diff/no-op write, isolating scheduling latency from indexing work. The bound assumes the small batch size; if the harness corpus makes reindex finish before the index even starts, enlarge the corpus or raise the batch via env so the race is real.

- [ ] **Step 3: Run the e2e suite**

Run: `cargo build --release && bash tests/e2e.sh`
Expected: the suite passes, including both new PASS lines.

- [ ] **Step 4: Commit**

```bash
git add tests/e2e.sh
git commit -m "test(e2e): assert reads responsive and writes preempt during reindex"
```

---

### Task 9: Documentation sync

**Files:**
- Modify: `CLAUDE.md` (Architecture / Data Flow / Gotchas)
- Modify: `README.md` and `README.ja.md` (keep in sync)

**Interfaces:**
- Consumes: nothing.
- Produces: docs describing the connection model, write fairness, and the new config keys.

- [ ] **Step 1: Update `CLAUDE.md`**

- In the daemon/data-flow description, note: tsmd owns one writer connection plus a read-only pool (`query_only`); reads (status/doctor/search/ping) are served from the pool concurrently with writes.
- Add a Gotchas bullet: "Read requests are routed by `DaemonRequest::is_read_only()` (exhaustive match — a new request variant must be classified or it won't compile). Reads run on the `query_only` reader pool sized by `reader_pool_size` (default CPU cores); writes serialize on the single writer."
- Add a Gotchas bullet on write fairness: "reindex/backfill yields the writer to a pending `Index` (`yield_to_pending_writes`), so a file/interactive index preempts an in-progress reindex within one batch. The FTS reindex batch is `reindex_fts_batch_size` (default 200); smaller = finer preemption, more fsync."
- Note `busy_timeout` is now set on all connections.

- [ ] **Step 2: Update `README.md` and `README.ja.md`**

Add `reader_pool_size` (default CPU cores; caps concurrent reads) and `reindex_fts_batch_size` (default 200; preemption granularity vs reindex throughput) to the configuration reference in both (English + Japanese). Keep wording parallel between the two files.

- [ ] **Step 3: Lint docs**

Run: `rumdl check CLAUDE.md README.md README.ja.md`
Expected: no errors.

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md README.md README.ja.md
git commit -m "docs: describe read/write connection split, write fairness, config knobs"
```

---

## Final verification (before PR / merge)

- [ ] `cargo test` — all pass.
- [ ] `cargo clippy -- -D warnings` — clean.
- [ ] `cargo fmt --check` — clean.
- [ ] `cargo llvm-cov --ignore-filename-regex '(embedder|main|cli|tsmd|tsm_watcher|status|logging|daemon_mode|embedder_mode|watcher_mode|child|backfill)\.rs' --fail-under-lines 90` — ≥ 90% (new lib modules `read_pool`, `daemon_protocol::is_read_only`, db/config additions are on covered modules).
- [ ] `npx jscpd` ≤ 5%; `lizard src/ --language rust -Tcyclomatic_complexity=15 -w` — no new warnings.
- [ ] `bash tests/e2e.sh` — passes (IPC/search/index touched).
- [ ] Flip ADR-0015 `status: proposed → accepted`, bump `updated`, update the `decisions/README.md` index row to `Accepted`, in the final pre-merge commit.
- [ ] PR body in English; branch `feat/db-connection-rw-split`.
```
