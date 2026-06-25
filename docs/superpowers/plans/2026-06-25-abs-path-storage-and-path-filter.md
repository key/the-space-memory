# Absolute file_path storage + --path filter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Store `documents.file_path` as a lexical absolute path and make `tsm search --path` a reliable CWD-anchored, directory-boundary scope filter for multi-context / orchestration hosts.

**Architecture:** A new pure `src/paths.rs` module owns all path resolution (lexical absolutization, `--path` normalization, directory-boundary matching, LIKE-pattern building). Every consumer — ingest/index, `--path` CLI normalization, the three retrieval sources, the final rank JOIN, and content_dir weight matching — calls this one module. Path filtering is pushed into each retrieval source (FTS5 / vector / entity) so narrow scopes fill their candidate budget; the final-JOIN filter stays as a correctness safety net.

**Tech Stack:** Rust, rusqlite (bundled SQLite), sqlite-vec 0.1.9 (vec0), FTS5.

## Global Constraints

- ADR authority: **ADR-0017** (`decisions/0017-absolute-source-file-and-path-filter.md`). Storage = lexical absolute, symlinks NOT resolved. `--path` = CWD-anchored directory-boundary match. Display: text = CWD-relative, JSON = absolute.
- License: MIT. No new dependencies — implement the `.`/`..` fold ourselves (`std::path::absolute` does NOT fold `..`).
- TDD required: Red → Green → Refactor. Unit tests in `#[cfg(test)] mod tests`. DB tests use `:memory:`.
- Coverage ≥ 90% on touched modules (`cargo llvm-cov --fail-under-lines 90`). clippy `-D warnings`, `cargo fmt --check` clean.
- `cargo` is on PATH directly (`~/.cargo/bin/cargo`). Do NOT use `mise exec` (its tool-install step fails on attestation in this env).
- This is a **breaking DB change**: `file_path` meaning changes → `rebuild` required. A pre-migration (relative) DB must be detected and rejected with a clear message, never silently mis-queried.
- Verified fact (spike): sqlite-vec 0.1.9 accepts `WHERE embedding MATCH ? AND rowid in (...)` and returns only the constrained rowids — vector push-down is native, not best-effort.

---

### Task 1: `paths.rs` — shared path resolution module

**Files:**
- Create: `src/paths.rs`
- Modify: `src/lib.rs` (add `pub mod paths;`)
- Test: in-file `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `pub fn absolutize(input: &Path, base: &Path) -> PathBuf` — if `input` is relative, join onto `base`; then fold `.`/`..` lexically (no syscalls, no symlink resolution). Trailing slash collapses naturally.
  - `pub fn normalize_filter_path(arg: &str, cwd: &Path) -> anyhow::Result<PathBuf>` — `--path` normalization: error on empty; else `absolutize(Path::new(arg), cwd)`.
  - `pub fn is_within(candidate: &Path, dir: &Path) -> bool` — true if `candidate == dir` or `candidate` is under `dir` at a component boundary.
  - `pub fn boundary_like(dir: &Path) -> (String, String)` — returns `(eq_value, like_pattern)` where `eq_value` is the dir as a string and `like_pattern` is `"<escaped>/%"` with LIKE metachars (`\ % _`) escaped for `ESCAPE '\'`. Callers build `d.file_path = ?1 OR d.file_path LIKE ?2 ESCAPE '\'`.

- [ ] **Step 1: Write the failing tests**

```rust
// src/paths.rs
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn absolutize_relative_joins_base() {
        assert_eq!(absolutize(Path::new("daily"), Path::new("/root")), PathBuf::from("/root/daily"));
    }
    #[test]
    fn absolutize_folds_dotdot() {
        assert_eq!(absolutize(Path::new("../sib/x"), Path::new("/root/a")), PathBuf::from("/root/sib/x"));
    }
    #[test]
    fn absolutize_folds_dot_and_trailing() {
        assert_eq!(absolutize(Path::new("./daily/"), Path::new("/root")), PathBuf::from("/root/daily"));
    }
    #[test]
    fn absolutize_absolute_input_ignores_base() {
        assert_eq!(absolutize(Path::new("/abs/x"), Path::new("/root")), PathBuf::from("/abs/x"));
    }
    #[test]
    fn normalize_filter_empty_errors() {
        assert!(normalize_filter_path("", Path::new("/cwd")).is_err());
    }
    #[test]
    fn normalize_filter_dot_is_cwd() {
        assert_eq!(normalize_filter_path(".", Path::new("/cwd")).unwrap(), PathBuf::from("/cwd"));
    }
    #[test]
    fn is_within_boundary_not_substring() {
        assert!(is_within(Path::new("/r/daily/x.md"), Path::new("/r/daily")));
        assert!(is_within(Path::new("/r/daily"), Path::new("/r/daily")));
        assert!(!is_within(Path::new("/r/daily-report/x.md"), Path::new("/r/daily")));
    }
    #[test]
    fn boundary_like_escapes_metachars() {
        let (eq, like) = boundary_like(Path::new("/r/daily_notes"));
        assert_eq!(eq, "/r/daily_notes");
        assert_eq!(like, r"/r/daily\_notes/%");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib paths:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function absolutize` (module not yet created / not wired).

- [ ] **Step 3: Write the implementation**

```rust
// src/paths.rs
//! Shared path resolution: lexical absolutization (no symlink resolution),
//! `--path` filter normalization, and directory-boundary matching.
//! All functions are pure (no filesystem syscalls) so they unit-test cleanly
//! and behave identically across CLI and daemon. See ADR-0017.

use std::path::{Component, Path, PathBuf};

/// Lexically absolutize `input` against `base`, folding `.`/`..` without
/// touching the filesystem (symlinks are NOT resolved — ADR-0017 option A).
/// `std::path::absolute` deliberately does not fold `..`; we do, accepting the
/// documented symlink caveat.
pub fn absolutize(input: &Path, base: &Path) -> PathBuf {
    let joined = if input.is_absolute() {
        input.to_path_buf()
    } else {
        base.join(input)
    };
    let mut out: Vec<Component> = Vec::new();
    for comp in joined.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop a normal segment; keep `..` only if nothing to pop
                // (above root — should not happen for absolute inputs).
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !matches!(out.last(), Some(Component::RootDir | Component::Prefix(_))) {
                    out.push(comp);
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// Normalize a `--path` argument to a CWD-anchored absolute path.
/// Empty string is the only error; everything else resolves.
pub fn normalize_filter_path(arg: &str, cwd: &Path) -> anyhow::Result<PathBuf> {
    if arg.is_empty() {
        anyhow::bail!("--path cannot be empty");
    }
    Ok(absolutize(Path::new(arg), cwd))
}

/// True if `candidate` equals `dir` or sits under it at a component boundary.
pub fn is_within(candidate: &Path, dir: &Path) -> bool {
    candidate == dir || candidate.starts_with(dir)
}

/// Build the SQL operands for a directory-boundary match against
/// `documents.file_path`: `(eq_value, like_pattern)`. Caller uses
/// `d.file_path = ?eq OR d.file_path LIKE ?like ESCAPE '\'`.
pub fn boundary_like(dir: &Path) -> (String, String) {
    let s = dir.to_string_lossy();
    let escaped = s.replace('\\', r"\\").replace('%', r"\%").replace('_', r"\_");
    (s.to_string(), format!("{escaped}/%"))
}
```

- [ ] **Step 4: Wire the module**

Add to `src/lib.rs` (alongside the other `pub mod` lines):

```rust
pub mod paths;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib paths:: 2>&1 | tail -20`
Expected: PASS — all 8 tests green.

- [ ] **Step 6: Commit**

```bash
git add src/paths.rs src/lib.rs
git commit -m "feat(paths): shared lexical path resolution + boundary match (ADR-0017)"
```

---

### Task 2: Absolute storage in ingest / index_file

**Files:**
- Modify: `src/indexer/mod.rs:84-94` (`index_file`: drop `strip_prefix`, store absolute) and the matching existing-doc lookup in `index_all_with_progress` (search for the second `strip_prefix(project_root)`).
- Modify: `src/main.rs:375-383` (`Index { files_from_stdin }`: stop converting stdin paths to project_root-relative; pass absolutized paths).
- Test: `src/indexer/mod.rs` tests.

**Interfaces:**
- Consumes: `paths::absolutize` (Task 1).
- Produces: `documents.file_path` rows are absolute. `directory_from_rel_path` now receives an absolute path; the derived `directory` is the absolute parent — acceptable (used only for the `directory` metadata column, not for matching).

- [ ] **Step 1: Write the failing test**

```rust
// in src/indexer/mod.rs #[cfg(test)] mod tests
#[test]
fn index_file_stores_absolute_path() {
    let conn = crate::db::get_memory_connection().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("note.md");
    std::fs::write(&f, "# Title\n\nbody").unwrap();
    index_file(&conn, &f, dir.path()).unwrap();
    let stored: String = conn
        .query_row("SELECT file_path FROM documents LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, f.to_string_lossy());
    assert!(std::path::Path::new(&stored).is_absolute());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib indexer::tests::index_file_stores_absolute_path 2>&1 | tail -15`
Expected: FAIL — stored value is `note.md` (relative), not the absolute path.

- [ ] **Step 3: Implement — replace the strip_prefix with absolutize**

In `src/indexer/mod.rs` `index_file`, replace:

```rust
    let rel_path = file_path
        .strip_prefix(project_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .to_string();

    let directory = directory_from_rel_path(&rel_path);
```

with:

```rust
    let abs_path = crate::paths::absolutize(file_path, project_root);
    let stored_path = abs_path.to_string_lossy().to_string();

    let directory = directory_from_rel_path(&stored_path);
```

Then replace every later use of `rel_path` in `index_file` with `stored_path` (it is the value bound to `documents.file_path`). Apply the same `strip_prefix(project_root)` → `absolutize` change at the existing-doc lookup site in `index_all_with_progress`, comparing against the absolutized path.

In `src/main.rs` `Index { files_from_stdin }`, replace the `rel_paths` block:

```rust
                let rel_paths: Vec<String> = paths
                    .iter()
                    .filter_map(|p| p.strip_prefix(&project_root).ok())
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();
                DaemonRequest::Index { files: rel_paths }
```

with absolutized paths (the daemon re-absolutizes too, but normalize here for a stable wire value):

```rust
                let abs_paths: Vec<String> = paths
                    .iter()
                    .map(|p| crate::paths::absolutize(p, &project_root).to_string_lossy().to_string())
                    .collect();
                DaemonRequest::Index { files: abs_paths }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib indexer:: 2>&1 | tail -20`
Expected: PASS (the new test plus existing indexer tests — fix any that asserted relative paths to assert absolute).

- [ ] **Step 5: Commit**

```bash
git add src/indexer/mod.rs src/main.rs
git commit -m "feat(indexer): store file_path as absolute (ADR-0017)"
```

---

### Task 3: Rebuild-required migration guard

**Files:**
- Modify: `src/db.rs` (add a `user_version` PRAGMA check on connect; bump on init).
- Test: `src/db.rs` tests.

**Interfaces:**
- Produces: `pub fn check_path_schema(conn: &Connection) -> anyhow::Result<()>` — returns `Err` with a "run `tsm rebuild`" message when the DB predates absolute storage. Called from the daemon startup path (where `is_initialized` is checked).

> **Verified**: `PRAGMA user_version` is currently unused anywhere in `src/` — free to use as the path-schema marker. It is independent of the existing additive column migrations (`ensure_content_hash_column`, `ensure_metadata_column`), which run on connect and stay as-is; those upgrade old DBs in place but never touch `user_version`, so a pre-absolute DB still reads `0` and is correctly rejected.

- [ ] **Step 1: Write the failing test**

```rust
// in src/db.rs #[cfg(test)] mod tests
#[test]
fn check_path_schema_rejects_old_version() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA user_version = 0;").unwrap();
    assert!(check_path_schema(&conn).is_err());
}
#[test]
fn check_path_schema_accepts_current_version() {
    let conn = get_memory_connection().unwrap(); // init bumps user_version
    assert!(check_path_schema(&conn).is_ok());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib db::tests::check_path_schema 2>&1 | tail -15`
Expected: FAIL — `cannot find function check_path_schema`.

- [ ] **Step 3: Implement**

In `src/db.rs`, add a schema-version constant and bump it in `init_db` after `conn.execute_batch(SCHEMA_SQL)`:

```rust
/// DB layout version. Bumped to 1 when file_path became absolute (ADR-0017).
pub const PATH_SCHEMA_VERSION: i64 = 1;
```

In `init_db`, after schema creation, add:

```rust
    conn.pragma_update(None, "user_version", PATH_SCHEMA_VERSION)?;
```

Add the guard:

```rust
/// Reject a DB created before absolute-path storage. The `file_path` column
/// changed meaning (relative → absolute, ADR-0017), so an old DB would
/// silently mis-match every `--path`. Fail fast and tell the user to rebuild.
pub fn check_path_schema(conn: &Connection) -> anyhow::Result<()> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v < PATH_SCHEMA_VERSION {
        anyhow::bail!(
            "This index predates absolute-path storage and must be rebuilt. \
             Run `tsm rebuild --apply`."
        );
    }
    Ok(())
}
```

Call `check_path_schema` from the daemon startup, right after the post-lock `is_initialized` re-check in `src/bin/tsmd/daemon_mode.rs` (search for `is_initialized`), propagating the error so `cmd_start` surfaces it.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib db::tests::check_path_schema 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/db.rs src/bin/tsmd/daemon_mode.rs
git commit -m "feat(db): reject pre-absolute-path index, require rebuild (ADR-0017)"
```

---

### Task 4: `--path` CLI normalization (absolute or CWD-relative)

**Files:**
- Modify: `src/main.rs:347-356` (replace the empty/absolute validation with `paths::normalize_filter_path` + dedup).
- Test: `src/main.rs` or a `tests/` integration test for the normalization (the loop logic can be extracted to a small helper for unit testing).

**Interfaces:**
- Consumes: `paths::normalize_filter_path` (Task 1).
- Produces: `DaemonRequest::Search.paths` now carries **absolute** path strings (CWD-anchored), deduped. `daemon_protocol.rs` unchanged (already `Option<Vec<String>>`).

- [ ] **Step 1: Write the failing test**

Extract the per-arg normalization into a helper in `src/cli.rs` so it is unit-testable:

```rust
// src/cli.rs
/// Normalize `--path` args to deduped absolute paths anchored at `cwd`.
pub fn normalize_path_filters(args: &[String], cwd: &std::path::Path) -> anyhow::Result<Option<Vec<String>>> {
    if args.is_empty() {
        return Ok(None);
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for a in args {
        let p = crate::paths::normalize_filter_path(a, cwd)?.to_string_lossy().to_string();
        if seen.insert(p.clone()) {
            out.push(p);
        }
    }
    Ok(Some(out))
}
```

Test:

```rust
// in src/cli.rs #[cfg(test)] mod tests
#[test]
fn normalize_path_filters_abs_and_rel_and_dedup() {
    let cwd = std::path::Path::new("/root/repoA");
    let got = normalize_path_filters(
        &["daily".into(), "/root/repoA/daily".into(), "../repoB".into()],
        cwd,
    ).unwrap().unwrap();
    assert_eq!(got, vec!["/root/repoA/daily".to_string(), "/root/repoB".to_string()]);
}
#[test]
fn normalize_path_filters_empty_arg_errors() {
    assert!(normalize_path_filters(&["".into()], std::path::Path::new("/c")).is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cli::tests::normalize_path_filters 2>&1 | tail -15`
Expected: FAIL — `cannot find function normalize_path_filters`.

- [ ] **Step 3: Implement + wire into main.rs**

Add the helper above to `src/cli.rs`. In `src/main.rs`, replace the validation loop and the `paths` binding:

```rust
            for p in &paths {
                if p.is_empty() {
                    anyhow::bail!("--path cannot be empty");
                }
                if std::path::Path::new(p).is_absolute() {
                    anyhow::bail!(
                        "--path must be a relative path (e.g. 'daily/'), got absolute: {p}"
                    );
                }
            }
            let paths = if paths.is_empty() { None } else { Some(paths) };
```

with:

```rust
            let cwd = std::env::current_dir()?;
            let paths = cli::normalize_path_filters(&paths, &cwd)?;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib cli::tests::normalize_path_filters 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs src/main.rs
git commit -m "feat(cli): --path accepts absolute or CWD-relative, normalized + deduped (ADR-0017)"
```

---

### Task 5: Boundary match at the final rank JOIN

**Files:**
- Modify: `src/searcher/rank.rs:64-77` (`build_filter_clauses`: prefix LIKE → boundary match via `paths::boundary_like`).
- Test: `src/searcher/mod.rs` tests (existing `test_search_with_path_filter_*` adjusted to absolute paths + a new boundary test).

**Interfaces:**
- Consumes: `paths::boundary_like`. `path_prefixes` values are now absolute (from Task 4).
- Produces: final JOIN matches `d.file_path = ?eq OR d.file_path LIKE ?like ESCAPE '\'` per prefix.

- [ ] **Step 1: Write the failing test**

```rust
// in src/searcher/mod.rs tests — adapt the existing helper to absolute paths
#[test]
fn path_filter_boundary_excludes_sibling_prefix() {
    // index docs at /r/daily/x.md and /r/daily-report/y.md (absolute file_path)
    // search with path_prefixes = ["/r/daily"]
    // assert only /r/daily/x.md is returned, NOT /r/daily-report/y.md
    // (Arrange via the module's existing insert helper, switched to absolute paths.)
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib searcher::mod::tests::path_filter_boundary 2>&1 | tail -15`
Expected: FAIL — current `LIKE 'daily%'` logic matches `daily-report` too (and/or the relative-path arrange no longer matches absolute storage).

- [ ] **Step 3: Implement**

In `src/searcher/rank.rs` `build_filter_clauses`, replace the `path_sql` block:

```rust
    let path_sql = match path_prefixes {
        Some(prefixes) if !prefixes.is_empty() => {
            let conditions: Vec<String> = prefixes
                .iter()
                .map(|_| "d.file_path LIKE ? ESCAPE '\\'".to_string())
                .collect();
            for p in prefixes {
                let escaped = p
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_");
                extra_params.push(Box::new(format!("{}%", escaped)));
            }
            format!(" AND ({})", conditions.join(" OR "))
        }
        _ => String::new(),
    };
```

with a boundary match built from the shared helper:

```rust
    let path_sql = match path_prefixes {
        Some(prefixes) if !prefixes.is_empty() => {
            let mut conditions = Vec::new();
            for p in prefixes {
                let (eq, like) = crate::paths::boundary_like(std::path::Path::new(p));
                conditions.push("(d.file_path = ? OR d.file_path LIKE ? ESCAPE '\\')".to_string());
                extra_params.push(Box::new(eq));
                extra_params.push(Box::new(like));
            }
            format!(" AND ({})", conditions.join(" OR "))
        }
        _ => String::new(),
    };
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib searcher:: 2>&1 | tail -25`
Expected: PASS (new boundary test + existing path tests updated to absolute paths).

- [ ] **Step 5: Commit**

```bash
git add src/searcher/rank.rs src/searcher/mod.rs
git commit -m "feat(search): directory-boundary --path match at final JOIN (ADR-0017 Gap 1)"
```

---

### Task 6: Push `--path` into FTS5 + vector retrieval

**Files:**
- Modify: `src/searcher/mod.rs:54-60` (thread `path_prefixes` into `retrieve`).
- Modify: `src/searcher/retrieve.rs` (`retrieve`, `fts_results`, `fts_results_raw`, `vec_results_from_embedding` take an in-scope filter; FTS joins to documents, vector uses `rowid in (...)`).
- Test: `src/searcher/mod.rs` tests (narrow-scope budget guard).

**Interfaces:**
- Consumes: `paths::boundary_like`. Verified: sqlite-vec 0.1.9 supports `rowid in (...)`.
- Produces: `retrieve(conn, plan, limit, require_vector, path_prefixes: Option<&[String]>)`.

- [ ] **Step 1: Write the failing test**

```rust
// in src/searcher/mod.rs tests
#[test]
fn narrow_scope_fills_budget_from_each_source() {
    // Arrange: index many out-of-scope docs + a few in-scope under /r/daily/.
    // Act: search with path_prefixes = ["/r/daily"], top_k small.
    // Assert: the in-scope docs are returned (not crowded out by out-of-scope
    //         candidates that the final JOIN would have dropped).
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib searcher::mod::tests::narrow_scope_fills_budget 2>&1 | tail -15`
Expected: FAIL — scope-blind retrieval crowds out in-scope candidates.

- [ ] **Step 3: Implement**

Add a helper in `src/searcher/retrieve.rs` that builds an in-scope SQL fragment + params for `documents`-joined queries:

```rust
/// Build `(sql_fragment, params)` restricting to in-scope file_paths, e.g.
/// `" AND (d.file_path = ? OR d.file_path LIKE ? ESCAPE '\\')"`. Empty when no filter.
fn scope_clause(path_prefixes: Option<&[String]>) -> (String, Vec<String>) {
    match path_prefixes {
        Some(ps) if !ps.is_empty() => {
            let mut conds = Vec::new();
            let mut params = Vec::new();
            for p in ps {
                let (eq, like) = crate::paths::boundary_like(std::path::Path::new(p));
                conds.push("(d.file_path = ? OR d.file_path LIKE ? ESCAPE '\\')".to_string());
                params.push(eq);
                params.push(like);
            }
            (format!(" AND ({})", conds.join(" OR ")), params)
        }
        _ => (String::new(), Vec::new()),
    }
}
```

FTS5 — join `chunks` + `documents` and apply the scope clause (`fts_results_raw`):

```rust
    let (scope_sql, scope_params) = scope_clause(path_prefixes);
    let sql = format!(
        "SELECT f.rowid AS chunk_id
         FROM chunks_fts f
         JOIN chunks c ON c.id = f.rowid
         JOIN documents d ON d.id = c.document_id
         WHERE f MATCH ?{scope_sql}
         ORDER BY rank
         LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&fts_query];
    for p in &scope_params { binds.push(p); }
    binds.push(&(limit as i64));
    let rows = stmt.query_map(binds.as_slice(), |row| row.get::<_, i64>(0))?;
```

Vector — pre-compute in-scope chunk ids and constrain the KNN with `rowid in (...)` (verified on 0.1.9). When no filter, keep the current unconstrained query:

```rust
    let scoped_ids: Option<Vec<i64>> = match path_prefixes {
        Some(ps) if !ps.is_empty() => {
            let (scope_sql, scope_params) = scope_clause(path_prefixes);
            let sql = format!(
                "SELECT c.id FROM chunks c JOIN documents d ON d.id = c.document_id \
                 WHERE 1=1{scope_sql}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let binds: Vec<&dyn rusqlite::ToSql> = scope_params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
            let ids = stmt.query_map(binds.as_slice(), |r| r.get::<_, i64>(0))?
                .collect::<Result<Vec<i64>, _>>()?;
            Some(ids)
        }
        _ => None,
    };

    let sql = match &scoped_ids {
        Some(ids) => {
            let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            format!("SELECT rowid, distance FROM chunks_vec WHERE embedding MATCH ? AND rowid in ({list}) ORDER BY distance LIMIT ?")
        }
        None => "SELECT rowid, distance FROM chunks_vec WHERE embedding MATCH ? ORDER BY distance LIMIT ?".to_string(),
    };
```

Thread `path_prefixes` through `retrieve(...)` and from `searcher/mod.rs`:

```rust
    let candidates = retrieve::retrieve(conn, &qp, limit, require_vector, path_prefixes)?;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib searcher:: 2>&1 | tail -25`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/searcher/retrieve.rs src/searcher/mod.rs
git commit -m "feat(search): push --path into FTS5 + vector retrieval (ADR-0017 Gap 3)"
```

---

### Task 7: Push `--path` into entity retrieval (incl. 2nd-hop)

**Files:**
- Modify: `src/entity.rs` (`entity_results_by_ids`, `find_related_entity_ids`: filter chunk results by document path).
- Modify: `src/searcher/retrieve.rs` (pass `path_prefixes` to `entity_results_by_ids`).
- Test: `src/entity.rs` tests.

**Interfaces:**
- Consumes: `paths::boundary_like`.
- Produces: `entity_results_by_ids(conn, ids, limit, path_prefixes: Option<&[String]>)`.

- [ ] **Step 1: Write the failing test**

```rust
// in src/entity.rs tests
#[test]
fn entity_results_respect_path_scope() {
    // Arrange: two docs sharing an entity, one under /r/daily, one under /r/other.
    // Act: entity_results_by_ids(conn, &[entity_id], 10, Some(&["/r/daily".into()])).
    // Assert: only the /r/daily chunk is returned.
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib entity::tests::entity_results_respect_path_scope 2>&1 | tail -15`
Expected: FAIL — signature lacks `path_prefixes` / no scope applied.

- [ ] **Step 3: Implement**

Add `path_prefixes: Option<&[String]>` to `entity_results_by_ids` and `find_related_entity_ids`. In the chunk-fetch query inside `entity_results_by_ids`, join `documents` and append the same `scope_clause`-style fragment so both the direct and 2nd-hop chunk lookups are filtered. Reuse `crate::paths::boundary_like` to build the `(eq, like)` operands. Update the call site in `retrieve.rs`:

```rust
    let entity =
        entity::entity_results_by_ids(conn, &plan.classification.matched_entity_ids, limit, path_prefixes)
            .unwrap_or_default();
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib entity:: searcher:: 2>&1 | tail -25`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/entity.rs src/searcher/retrieve.rs
git commit -m "feat(entity): scope entity + 2nd-hop retrieval by --path (ADR-0017 Gap 4)"
```

---

### Task 8: Fold content_dir resolution + weight matching into `paths.rs` (absolute base)

**Files:**
- Modify: `src/config.rs` (content_dir resolution ~432-475: allow absolute/`..`, resolve to absolute via `paths::absolutize`; `directory_weight`/`half_life_days` matching: use `paths::is_within`, longest-match by absolute path length). Fold `project_root_from` (config.rs:852-861) to delegate to `paths::absolutize`.
- Test: `src/config.rs` tests.

**Interfaces:**
- Consumes: `paths::absolutize`, `paths::is_within`.
- Produces: `ContentDir.path` is an absolute path string (project_root-resolved). Weight matching uses boundary semantics.

- [ ] **Step 1: Write the failing test**

```rust
// in src/config.rs tests
#[test]
fn directory_weight_uses_boundary_not_prefix() {
    // content_dirs: [{ path "/r/daily", weight 2.0 }], default 1.0
    // weight("/r/daily/x.md") == 2.0 ; weight("/r/daily-report/y.md") == default
}
#[test]
fn content_dir_absolute_path_accepted() {
    // a content_dirs entry with an absolute path resolves (no longer rejected)
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib config::tests::directory_weight_uses_boundary config::tests::content_dir_absolute 2>&1 | tail -15`
Expected: FAIL — absolute entries are rejected (config.rs:443) and matching is `starts_with` on relative strings.

- [ ] **Step 3: Implement**

In `src/config.rs` content_dir resolution, remove the absolute-rejection `bail`/skip (config.rs:443) and resolve each entry with `paths::absolutize(Path::new(&entry.path), &project_root)`, storing the absolute string. Keep the longest-match ordering (`sort_by_key` on path length still works on absolute strings). In `directory_weight` / `half_life_days`, replace `file_path.starts_with(dir.path)` with `crate::paths::is_within(Path::new(file_path), Path::new(&dir.path))`. Simplify `project_root_from` to call `paths::absolutize(path, &cwd).parent()`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib config:: 2>&1 | tail -25`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): absolute content_dir resolution + boundary weight match (ADR-0017)"
```

---

### Task 9: Display — text CWD-relative, JSON absolute

**Files:**
- Modify: `src/searcher/format.rs` (text rendering relativizes `file_path` against CWD; JSON leaves it absolute).
- Modify: `src/cli.rs:1637` (any other display strip_prefix → CWD-relative helper).
- Test: `src/searcher/format.rs` tests.

**Interfaces:**
- Consumes: nothing new (relativize is a small local helper: `pathdiff`-style is not needed — use `Path::strip_prefix(cwd)` with a fallback to the absolute path).

- [ ] **Step 1: Write the failing test**

```rust
// in src/searcher/format.rs tests
#[test]
fn text_output_is_cwd_relative_json_is_absolute() {
    // Given a result with file_path "/cwd/daily/x.md" and cwd "/cwd":
    //   text render contains "daily/x.md"
    //   json render contains "/cwd/daily/x.md"
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib format::tests::text_output_is_cwd_relative 2>&1 | tail -15`
Expected: FAIL — text currently prints the stored value verbatim (now absolute).

- [ ] **Step 3: Implement**

Add a relativize helper used only by the text formatter:

```rust
fn display_path(file_path: &str, cwd: &std::path::Path) -> String {
    std::path::Path::new(file_path)
        .strip_prefix(cwd)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| file_path.to_string())
}
```

Apply `display_path(&result.file_path, &cwd)` in the text formatter (resolve `cwd` once via `std::env::current_dir()`). Leave the JSON serializer emitting the raw absolute `file_path`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib format:: 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/searcher/format.rs src/cli.rs
git commit -m "feat(search): text output CWD-relative, JSON absolute (ADR-0017)"
```

---

### Task 10: Docs + full gate + E2E

**Files:**
- Modify: `docs/command-reference.md` (`--path` section: absolute or CWD-relative, boundary match, out-of-scope → 0 hits, empty errors).
- Modify: `README.md` / `README.ja.md` (add `--path` description; keep in sync).
- Modify: `tests/e2e.sh` + `tests/e2e/testdata/**` (scoped scenario across two content_dirs under one root; date placeholders only).

**Interfaces:** none (docs + E2E).

- [ ] **Step 1: Update `docs/command-reference.md`**

Document: input forms (absolute or CWD-relative), CWD-anchored directory-boundary match (`daily` excludes `daily-report`), trailing-slash insensitivity, multiple `--path` OR-joined, out-of-scope/zero-match → 0 hits (not error), empty → error. (`tests/cli_docs.rs` gates structural sync — keep examples valid.)

- [ ] **Step 2: Update READMEs in sync**

Add a `--path` line to both `README.md` and `README.ja.md`.

- [ ] **Step 3: Add the E2E scenario**

In `tests/e2e.sh`, add a scoped-search case: index two content_dirs under one root, run `tsm search --path <one_dir>`, assert only in-scope hits. Use `__TODAY__`/`__1Y_AGO__` placeholders in testdata.

- [ ] **Step 4: Run the full gate**

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo llvm-cov --ignore-filename-regex '(embedder|main|cli|tsmd|tsm_watcher|status|logging|daemon_mode|embedder_mode|watcher_mode|child|backfill)\.rs' --fail-under-lines 90
npx jscpd
lizard src/ --language rust -Tcyclomatic_complexity=15 -w
bash tests/e2e.sh
```

Expected: all green; coverage ≥ 90% on touched modules; no new CCN warnings; jscpd ≤ 5%.

- [ ] **Step 5: Commit**

```bash
git add docs/command-reference.md README.md README.ja.md tests/
git commit -m "docs+test: --path spec, READMEs, scoped E2E (ADR-0017, #160)"
```

---

## Notes for the implementer

- **Sequencing**: Tasks 1→3 are the absolute-storage foundation; Tasks 4→9 are the `--path` rewrite. They cannot ship separately (once `file_path` is absolute, the old prefix `--path` breaks), but they can be reviewed task-by-task. Suggested PR split: Task 1 alone (paths.rs + tests), then Tasks 2–9 together, then Task 10.
- **Safety net**: the final-JOIN filter (Task 5) stays even after retrieval push-down (Tasks 6–7) — retrieval push-down is a budget/quality fix, the JOIN guarantees correctness.
- **`directory` column**: Task 2 changes `directory_from_rel_path`'s input to an absolute path. Confirm no consumer relies on it being relative (it feeds the `directory` metadata column only). If a consumer does, derive `directory` from the content_dir-relative remainder instead.
