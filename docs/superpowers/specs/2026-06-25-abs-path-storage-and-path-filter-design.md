# Design: absolute `file_path` storage + `--path` filter for multi-context hosts

## Problem

`documents.file_path` is stored **`project_root`-relative** (`indexer/mod.rs:90-94`
strips `project_root`), with a silent fallback to an **absolute** path when
`strip_prefix` fails (files reached through a `..`/absolute `content_dir` that
live outside `project_root`). The same column therefore mixes two meanings.

This blocks `--path` from being a reliable filter for multi-context hosts
(e.g. an orchestration setup where several repos/worktrees sit under one indexed
tree and each calling context wants results restricted to a subtree):

1. **Identity is bound to a mutable context.** `file_path` is the only identity
   key for a file, yet it depends on `project_root` (the `tsm.toml` directory,
   resolved CWD-only per ADR-0009 §2). Move `tsm.toml`, start from a different
   CWD, or cross a `content_dir` boundary, and the same physical file gets a
   different `file_path`.
2. **`--path` has no well-defined base.** Because storage mixes relative and
   absolute, `--path` normalization cannot be defined to match both. A caller's
   `../sibling` (orchestration: "search the repo next to me") binds to
   `LIKE '../sibling%'`, which never matches anything in the DB.
3. **Prefix match leaks across directory boundaries.** The filter SQL is
   `d.file_path LIKE <p>%` (`searcher/rank.rs:64-77`), so `--path daily` also
   matches `daily-report/...`.
4. **Narrow scopes silently starve.** `path_prefixes` is applied **only** at the
   final rank JOIN (`searcher/mod.rs:56` passes it to `rank::rank`, not to
   `retrieve::retrieve`). Each retrieval source (FTS5 `retrieve.rs:90`, vector
   `retrieve.rs:129`, entity `entity.rs:340`) computes `top_k*3` candidates
   scope-blind, so out-of-scope chunks consume rank slots and a scoped query can
   return far fewer than `top_k` hits even when enough in-scope chunks exist. The
   tighter and more correct the filter, the worse the starvation — and
   orchestration's common case is exactly "filter to a narrow subtree."

### Background

`index_root` was abolished in commit df23358 (#229, 2026-06-23); content now
resolves against `project_root` + `content_dirs`. ADR-0009 §4 already proposed
storing `file_path` as an absolute canonical path and a component-boundary match
rule, but **§4 was deferred** and ADR-0009 is still `proposed`. GitHub issue #160
("extend `--path`") was written against the old `index_root` model and is stale:
its Input-forms / Normalization spec is entirely `index_root`-based.

## Goals

- `documents.file_path` stored as a single, well-defined **absolute path**.
- `--path` accepts absolute **or** CWD-relative input and filters at a strict
  directory boundary, so orchestration contexts can scope reliably.
- Scoped queries return up to `top_k` in-scope hits: the path filter is applied
  at **retrieval time** in every source (FTS5, vector, entity), not only at the
  final JOIN.
- Human output stays readable (paths shown relative to CWD); machine output
  (JSON) stays unambiguous (absolute).

## Non-goals

- **Symlink resolution / canonical dedup.** Paths are absolutized lexically; the
  real file behind a symlink is not resolved (decision below). The "canonical
  dedup" deferred in #229 is therefore intentionally **not** done.
- **DB portability.** `.tsm/` is a local, rebuild-able artifact; absolute paths
  in it are fine.
- **Glob / regex / Windows separators / single-file `--path`** (unchanged from
  #160's out-of-scope list).
- Multi-tenant / per-tenant DB, HTTP/MCP surfaces.

## Decisions (from brainstorming)

1. **Absolutization = lexical, symlinks NOT resolved (option A).** Prepend CWD to
   relative inputs, resolve `.`/`..` lexically. Do not `fs::canonicalize` the
   files. Rationale: `current_dir()` is already OS-canonicalized, and the path
   strings a caller writes (`/workspaces/...` under bind mounts) must match the
   stored values for `--path` to work; canonicalize would leak the real
   `/private/var/...` and break that match. A `content_dir` *root* may be
   canonicalized once to stabilize the base, but files below it are joined
   lexically.
2. **Storage absolute, display split by format.** Store absolute. Render
   **JSON = absolute** (machine/orchestration), **text = CWD-relative** (human).
3. **Migration = `rebuild` required.** Absolute storage changes the meaning of
   every `file_path` row. Detect a pre-migration (relative) DB via a schema
   migration marker and fail with "run `tsm rebuild`". No in-place auto-migration
   (the relative/absolute mix cannot be disambiguated row-by-row).
4. **`--path` re-spec (supersedes #160's `index_root`-based spec).**
   - Input: absolute **or** CWD-relative.
   - Normalize: CWD-prepend, lexical `.`/`..` resolve, strip trailing `/`, dedup.
     Same A rule as storage.
   - Match: component-boundary, `d.file_path = <abs> OR d.file_path LIKE <abs> || '/%'`.
     `--path .../daily` matches `.../daily/notes/x.md`, not `.../daily-report/...`.
   - **No range check.** With `index_root` gone there is no enclosing boundary;
     paths outside any `content_dir`, or matching nothing, return **0 hits, not
     an error**. Cross-`content_dir` scoping falls out naturally.
   - Remaining error: empty string only. Bare `.` resolves to CWD (≈ no filter).
5. **`--path` pushed into retrieval (Gap 3 + 4 of #160).** Pass `path_prefixes`
   into `retrieve::retrieve`; apply the boundary match in FTS5, vector, and entity
   sources (including entity 2nd-hop expansion) so each returns `top_k*3` in-scope
   candidates. Keep the final-JOIN filter as a belt-and-suspenders safety net.

## Architecture (module-by-module)

| Module | Change |
|---|---|
| `indexer/mod.rs` | Drop the `strip_prefix(project_root)`; store the lexically-absolutized path. Same change in the existing-doc lookup (`mod.rs:183-187`). |
| `searcher/format.rs` | Render text output CWD-relative; JSON absolute. |
| `searcher/rank.rs` | LIKE prefix → boundary match (`= <abs> OR LIKE <abs> || '/%'`), LIKE metachar escaping preserved. |
| `searcher/retrieve.rs` | Accept `path_prefixes`; apply boundary match in FTS5 / vector candidate queries. |
| `searcher/mod.rs` | Thread `path_prefixes` into `retrieve()` (currently only `rank()`). |
| `entity.rs` | Path filter in `entity_results_by_ids` and `find_related_entity_ids` (2nd-hop), via a JOIN to `documents.file_path`. |
| `cli.rs` / `main.rs` | Replace the absolute-reject + empty/`.` checks (`main.rs:347-357`) with the §4 normalization (absolute/CWD-relative both accepted; empty-only error). |
| `config.rs` | `directory_weight` / `half_life_days` matching (`file_path.starts_with(dir.path)`, `config.rs:1143-1213`): absolutize `dir.path` with the A rule and switch to boundary match (`= dir_abs OR starts_with(dir_abs + "/")`). |
| `db.rs` | Schema migration marker; reject a relative-era DB with a "run `tsm rebuild`" error. |
| docs | ADR-0009 §4 promoted to accepted; `command-reference.md` updated; #160 re-spec'd. |

## Data flow

```text
index:   file abs path (lexical) ─► documents.file_path (absolute)
search:  --path (abs | cwd-rel) ─► normalize (A rule) ─► boundary-match SQL
         applied in: FTS5 ┐
                     vec  ├─ retrieve (top_k*3 in-scope each) ─► RRF ─► rank (JOIN re-filter, safety)
                     ent  ┘                                                   │
output:  documents.file_path (abs) ─► format ─► { JSON: abs, text: cwd-rel }
```

## Error handling

- `--path ""` → error (empty).
- `--path` outside any `content_dir` / matching nothing → 0 hits (not an error).
- Relative-era DB (no migration marker) → fail-fast "run `tsm rebuild`".
- `strip_prefix` fallback path is removed; absolutization always yields an
  absolute path, so the mixed-meaning case disappears.

## Testing

- Index stores absolute path (no `project_root` strip).
- Output: text = CWD-relative, JSON = absolute.
- Boundary match: `--path daily` excludes `daily-report/...`; trailing-slash
  insensitive (`daily` ≡ `daily/`).
- `--path` accepts both absolute and CWD-relative; `..` resolves lexically.
- Empty `--path` errors; out-of-scope `--path` returns 0 hits (no error).
- **Retrieval push-down**: a narrow scope returns up to `top_k` in-scope hits
  from FTS5, vector, and entity sources (Gap 3 regression guard).
- Entity 2nd-hop expansion respects the path filter (Gap 4).
- `config` weight/half-life matching uses boundary match (`daily` ≠ `daily-report`).
- Migration: a relative-era DB triggers the rebuild error.
- E2E: multiple `content_dir`s under one root; scoped search returns only
  in-scope chunks with ranking preserved.

## Sequencing

1. Land ADR-0009 §4 (absolute storage) + rebuild migration first — the identity
   foundation.
2. Then the `--path` rewrite (boundary match + abs/cwd-rel input + retrieval
   push-down) on top of absolute storage.

PRs may split (e.g. storage+migration, then `--path`), but the design is one
thread. #160 is re-spec'd to this; its `index_root`-based spec is dropped.
