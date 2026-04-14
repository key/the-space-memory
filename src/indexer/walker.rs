//! Unified path filtering and file collection for indexing.
//!
//! `ContentWalker` is the single source of truth for "which files should tsm
//! index?" It centralizes the ad-hoc walker logic that used to be split
//! across `cli::discover_watch_dirs` (now deleted), the watcher event
//! filter, and the stdin path. Callers needing an `index_root` override
//! (the daemon, `cmd_rebuild`) use `from_env_with_index_root` directly.
//!
//! ## Ignore resolution
//!
//! Exclusion rules are applied in this precedence (highest first):
//!
//! 1. **Forced excludes** — `.git/` and `.tsm/` at any depth. Hard-coded
//!    here; NOT part of the `Gitignore` matcher so that a user's
//!    `!.git/` in `.tsmignore` cannot bring them back.
//! 2. **`.tsmignore` patterns** — read from the tsm project root (the
//!    directory containing `tsm.toml`). Patterns are resolved relative
//!    to `index_root`, not relative to the physical `.tsmignore` file.
//! 3. **Root `.gitignore`** — only when `respect_gitignore = true`.
//! 4. **Backward-compat default** — when `.tsmignore` is absent, a
//!    synthetic `.*/` pattern is injected so hidden directories like
//!    `.obsidian` keep the pre-#134 behavior without user action. Once
//!    a user creates `.tsmignore`, they take full control.
//!
//! ## Extension filter
//!
//! Orthogonal to path ignores: `ResolvedConfig::extensions` (default
//! `["md"]`) is the allowlist. `.tsmignore` can additionally drop files
//! via glob patterns like `*.parquet`.

use std::path::{Component, Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::config::{ContentDir, ResolvedConfig};
use crate::indexer::IngestPolicy;

/// Directory names that must never be indexed, regardless of configuration.
/// Prevents DB corruption (`.tsm/tsm.db`) and git metadata pollution.
const FORCED_EXCLUDED_DIRS: &[&str] = &[".git", ".tsm"];

/// Injected when `.tsmignore` is absent. Preserves the pre-#134 behavior of
/// skipping hidden directories across all traversal paths without forcing
/// existing users to create an ignore file.
const FALLBACK_HIDDEN_DIR_PATTERN: &str = ".*/";

pub struct ContentWalker {
    index_root: PathBuf,
    content_dirs: Vec<ContentDir>,
    extensions: Vec<String>,
    matcher: Gitignore,
}

impl ContentWalker {
    /// Core constructor taking raw values. Every other `from_*` constructor
    /// funnels through this — there is exactly one place where a walker is
    /// assembled, and exactly one place that owns the `.tsmignore` lookup
    /// against `project_root`. The `.tsmignore` file is read from
    /// `project_root` (the directory containing `tsm.toml`); a missing file
    /// is silently treated as empty — the common case for users who have
    /// not opted in. See the module-level doc for the rationale on routing
    /// every constructor through `new` instead of mutating `ResolvedConfig`.
    pub fn new(
        index_root: PathBuf,
        project_root: &Path,
        extensions: Vec<String>,
        content_dirs: Vec<ContentDir>,
        respect_gitignore: bool,
        ignore_file: &str,
    ) -> Self {
        let matcher = build_matcher(&index_root, project_root, ignore_file, respect_gitignore);
        Self {
            index_root,
            content_dirs,
            extensions,
            matcher,
        }
    }

    /// Construct a walker from a fully-resolved config. Thin wrapper that
    /// unpacks `cfg` and forwards to `new`. See `new` for `.tsmignore`
    /// lookup semantics.
    pub fn from_config(cfg: &ResolvedConfig) -> Self {
        Self::new(
            cfg.index_root.clone(),
            &cfg.project_root,
            cfg.extensions.clone(),
            cfg.content_dirs.clone(),
            cfg.respect_gitignore,
            &cfg.ignore_file,
        )
    }

    /// Convenience constructor that rebuilds `ResolvedConfig` from disk + env.
    /// Note: this does NOT consult the `config::RESOLVED` singleton — it reads
    /// `tsm.toml` fresh each call. In the common case the two views agree; the
    /// fs-watcher calls `config::reload()` and rebuilds the walker in sequence
    /// on SIGHUP, so both reflect the same on-disk state under normal
    /// conditions (a concurrent edit between the two reads is not atomic).
    pub fn from_env() -> Self {
        Self::from_config(&crate::config::ResolvedConfig::from_env())
    }

    /// Construct a walker rooted at a specific `index_root`, overriding the
    /// value in `ResolvedConfig`. Used by the daemon handler whose
    /// `index_root` argument is the authoritative one per request (and by
    /// tests that exercise the handler with a tempdir).
    ///
    /// `project_root` (and therefore `.tsmignore` location) still comes from
    /// the loaded config — only `index_root` is overridden.
    pub fn from_env_with_index_root(index_root: &Path) -> Self {
        let cfg = crate::config::ResolvedConfig::from_env();
        Self::new(
            index_root.to_path_buf(),
            &cfg.project_root,
            cfg.extensions,
            cfg.content_dirs,
            cfg.respect_gitignore,
            &cfg.ignore_file,
        )
    }

    /// True when `path` must not be indexed.
    ///
    /// Crate-internal because the combined gate — ignore rules *and*
    /// extension allowlist — is exposed via the `IngestPolicy::accepts`
    /// impl. Bypassing that composition (e.g. calling only `is_ignored`)
    /// would let a path skip the extension allowlist — the historical
    /// drift fixed in #134 / #135. Production callers must go through
    /// `accepts()`.
    pub(crate) fn is_ignored(&self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.index_root) else {
            return true;
        };
        if has_forced_excluded_component(rel) {
            return true;
        }
        // path.is_dir() returns false for paths that no longer exist (watcher
        // delete events). For directory-only patterns like `private/` we want
        // the path rejected whether or not the filesystem currently reports a
        // directory — so check both interpretations. A match in either form is
        // enough to exclude.
        self.matcher
            .matched_path_or_any_parents(rel, false)
            .is_ignore()
            || self
                .matcher
                .matched_path_or_any_parents(rel, true)
                .is_ignore()
    }

    /// Returns true when `path`'s extension is in the allowlist.
    /// Files without an extension or with an unlisted one are rejected.
    /// Crate-internal for the same reason as `is_ignored` — see above.
    pub(crate) fn extension_allowed(&self, path: &Path) -> bool {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) => self.extensions.iter().any(|e| e == ext),
            None => false,
        }
    }

    /// Walk the configured roots and collect every file that passes both
    /// the ignore rules and the extension allowlist.
    pub fn collect_files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for root in self.collection_roots() {
            self.walk_dir(&root, &mut out);
        }
        out
    }

    /// The starting points for traversal — either explicit `content_dirs`
    /// entries or auto-discovered immediate subdirectories of `index_root`.
    fn collection_roots(&self) -> Vec<PathBuf> {
        if self.content_dirs.is_empty() {
            self.auto_discover_roots()
        } else {
            let mut roots = Vec::with_capacity(self.content_dirs.len());
            for dir in &self.content_dirs {
                let full = self.index_root.join(&dir.path);
                if !full.is_dir() {
                    log::warn!(
                        "content_dir '{}' not found at {}; skipping",
                        dir.path,
                        full.display()
                    );
                    continue;
                }
                if self.is_ignored(&full) {
                    log::warn!(
                        "content_dir '{}' is excluded by ignore rules; skipping",
                        dir.path
                    );
                    continue;
                }
                roots.push(full);
            }
            roots
        }
    }

    fn auto_discover_roots(&self) -> Vec<PathBuf> {
        let entries = match std::fs::read_dir(&self.index_root) {
            Ok(e) => e,
            Err(e) => {
                log::warn!(
                    "cannot read {}: {e}; no subdirectories discovered",
                    self.index_root.display()
                );
                return Vec::new();
            }
        };
        let mut roots = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if self.is_ignored(&path) {
                continue;
            }
            roots.push(path);
        }
        roots
    }

    fn walk_dir(&self, dir: &Path, out: &mut Vec<PathBuf>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                log::warn!("cannot read directory {}: {e}", dir.display());
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if self.is_ignored(&path) {
                continue;
            }
            // Use file_type() which, unlike path.is_dir(), does NOT follow
            // symlinks. A symlink like `notes/data → ../../.git/` must not
            // allow traversal into the forced-excluded tree under a disguised
            // component name. Missing file_type (rare) is treated as "skip".
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                self.walk_dir(&path, out);
            } else if file_type.is_file() && self.extension_allowed(&path) {
                out.push(path);
            }
        }
    }
}

impl IngestPolicy for ContentWalker {
    /// A path is accepted iff it passes both the ignore matcher and the
    /// extension allowlist. This is the single definition of "should the
    /// indexer take this file"; the indexer calls it at its entry point,
    /// and the CLI stdin reader + watcher event loop use it as a
    /// pre-filter.
    fn accepts(&self, path: &Path) -> bool {
        !self.is_ignored(path) && self.extension_allowed(path)
    }
}

/// Build the combined `.tsmignore` + (optional) `.gitignore` + fallback
/// matcher. The builder is anchored at `index_root` so pattern resolution
/// matches the spec, even when `.tsmignore` physically lives in a different
/// directory (the tsm project root).
fn build_matcher(
    index_root: &Path,
    project_root: &Path,
    ignore_file: &str,
    respect_gitignore: bool,
) -> Gitignore {
    let mut builder = GitignoreBuilder::new(index_root);
    let tsmignore_path = project_root.join(ignore_file);
    let tsmignore_exists = tsmignore_path.is_file();

    if respect_gitignore {
        let gi = index_root.join(".gitignore");
        if gi.is_file() {
            if let Some(err) = builder.add(&gi) {
                log::warn!("ignoring .gitignore at {}: {err}", gi.display());
            }
        }
    }

    if tsmignore_exists {
        if let Some(err) = builder.add(&tsmignore_path) {
            // `GitignoreBuilder::add` returns `Some(err)` on the first
            // problem line but still applies the parseable patterns above
            // it. Word the log accordingly so users don't assume the whole
            // file was dropped. ERROR level so it surfaces without log
            // tuning — their exclusions may be partially applied.
            log::error!(
                "{}: parse error — some patterns may not have been applied: {err}",
                tsmignore_path.display()
            );
        }
    } else if let Err(err) = builder.add_line(None, FALLBACK_HIDDEN_DIR_PATTERN) {
        // This should never fail for a hard-coded literal pattern.
        log::warn!("failed to install fallback hidden-dir pattern: {err}");
    }

    builder.build().unwrap_or_else(|err| {
        // Pattern compilation failed after all add()s succeeded. Rare but
        // leaves the walker with no exclusions — ERROR level so it's noticed.
        log::error!("failed to build ignore matcher: {err}; no patterns applied");
        Gitignore::empty()
    })
}

/// True if any component of the (index_root-relative) path is a forced
/// excluded directory name. Checked outside the `Gitignore` matcher so that
/// users cannot re-include these via negation patterns.
fn has_forced_excluded_component(rel_path: &Path) -> bool {
    rel_path.components().any(|c| match c {
        Component::Normal(name) => {
            let s = name.to_string_lossy();
            FORCED_EXCLUDED_DIRS.iter().any(|d| *d == s)
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(dir: &Path, rel: &str, body: &str) {
        let full = dir.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, body).unwrap();
    }

    /// Build a `ResolvedConfig` where `index_root` and `project_root` both
    /// point at `root`. This is the common single-repo case and suffices
    /// for tests that don't care about the split between the two.
    fn cfg_for(root: &Path) -> ResolvedConfig {
        let toml = format!(
            r#"
index_root = "{}"
state_dir = "/tmp/unused-state"
"#,
            root.display()
        );
        let file_cfg: crate::config::ConfigFile = toml::from_str(&toml).unwrap();
        ResolvedConfig::from_config_file(&file_cfg, root.to_path_buf())
    }

    /// Build a walker anchored entirely at `tempdir` (both `index_root`
    /// and `project_root`). Project-root-aware tests that need the two to
    /// differ should call `cfg_for` variants and `ContentWalker::from_config`
    /// directly.
    fn walker_in(tempdir: &TempDir) -> ContentWalker {
        ContentWalker::from_config(&cfg_for(tempdir.path()))
    }

    // ─── Forced excludes ──────────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn forced_excludes_block_git_dir() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "notes/keep.md", "keep");
        write_file(tmp.path(), ".git/config.md", "no");
        let walker = walker_in(&tmp);
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("notes/keep.md")));
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == ".git")));
    }

    #[test]
    #[serial_test::serial]
    fn forced_excludes_block_tsm_dir() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "notes/keep.md", "keep");
        write_file(tmp.path(), ".tsm/state.md", "no");
        let walker = walker_in(&tmp);
        let files = walker.collect_files();
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == ".tsm")));
    }

    // ─── Multi-repo / middle-path matching ────────────────────────────
    //
    // Orchestration use case: `index_root` points at a parent dir containing
    // several independent repos (e.g. `/workspaces/{company, daily, ...}`),
    // each with its own `.git/`, `target/`, etc. Patterns and forced
    // excludes must match at any depth, not just top-level.

    #[test]
    #[serial_test::serial]
    fn forced_excludes_match_at_any_depth() {
        // `.git/` and `.tsm/` under *any* sub-repo must be excluded, not
        // only at index_root level. Ensures `has_forced_excluded_component`
        // scans every path component.
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "company/.git/HEAD.md", "no");
        write_file(tmp.path(), "daily/.tsm/sentinel.md", "no");
        write_file(tmp.path(), "daily/notes/keep.md", "yes");
        let walker = walker_in(&tmp);
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("daily/notes/keep.md")));
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == ".git")));
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == ".tsm")));
    }

    #[test]
    #[serial_test::serial]
    fn tsmignore_dir_pattern_matches_nested_subrepos() {
        // `.tsmignore` pattern `target/` with no leading slash must match
        // `target` at every depth per gitignore spec — crucial for the
        // orchestration layout where each sub-repo has its own target/.
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "company/target/out.md", "no");
        write_file(tmp.path(), "daily/target/out.md", "no");
        write_file(tmp.path(), "the-space-memory/target/out.md", "no");
        write_file(tmp.path(), "daily/notes/keep.md", "yes");
        write_file(tmp.path(), ".tsmignore", "target/\n");
        let walker = walker_in(&tmp);
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("daily/notes/keep.md")));
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == "target")));
    }

    #[test]
    #[serial_test::serial]
    fn tsmignore_leading_slash_anchors_to_index_root() {
        // With leading slash, patterns are anchored: `/worktrees/` excludes
        // only the top-level `worktrees/`, NOT sub-repo worktrees dirs.
        // This is the escape hatch when the any-depth default is too broad.
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "worktrees/wt1/a.md", "no");
        write_file(tmp.path(), "company/worktrees/wt2/b.md", "yes");
        write_file(tmp.path(), ".tsmignore", "/worktrees/\n");
        let walker = walker_in(&tmp);
        let files = walker.collect_files();
        // Top-level worktrees/ is excluded.
        assert!(!files.iter().any(|p| p.ends_with("worktrees/wt1/a.md")));
        // Nested company/worktrees/ is kept — leading-slash anchor stopped
        // the pattern from matching at middle paths.
        assert!(files
            .iter()
            .any(|p| p.ends_with("company/worktrees/wt2/b.md")));
    }

    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn symlink_into_forced_excluded_dir_is_not_followed() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        write_file(tmp.path(), ".git/HEAD.md", "no");
        write_file(tmp.path(), "notes/real.md", "yes");
        // Symlink under an innocent-looking name that points into .git/.
        // Without file_type()-based symlink rejection the walker would
        // descend and read .git/HEAD.md under the disguised component.
        symlink(tmp.path().join(".git"), tmp.path().join("notes/gitshadow")).unwrap();
        let walker = walker_in(&tmp);
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("notes/real.md")));
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == "gitshadow")));
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == ".git")));
    }

    #[test]
    #[serial_test::serial]
    fn forced_excludes_not_overridable_by_negation() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "notes/keep.md", "keep");
        write_file(tmp.path(), ".git/inside.md", "no");
        write_file(tmp.path(), ".tsmignore", "!.git/\n");
        let walker = walker_in(&tmp);
        let files = walker.collect_files();
        // .git/ must still be excluded even with an explicit negation pattern.
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == ".git")));
    }

    // ─── .tsmignore behavior ──────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn tsmignore_excludes_by_directory() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "daily/keep.md", "a");
        write_file(tmp.path(), "private/secret.md", "b");
        write_file(tmp.path(), ".tsmignore", "private/\n");
        let walker = walker_in(&tmp);
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("daily/keep.md")));
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == "private")));
    }

    #[test]
    #[serial_test::serial]
    fn tsmignore_excludes_by_glob() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "notes/a.md", "a");
        write_file(tmp.path(), "notes/b-draft.md", "b");
        write_file(tmp.path(), ".tsmignore", "**/*-draft.md\n");
        let walker = walker_in(&tmp);
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("notes/a.md")));
        assert!(!files.iter().any(|p| p.ends_with("b-draft.md")));
    }

    #[test]
    #[serial_test::serial]
    fn is_ignored_filters_stdin_style_paths() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "public/ok.md", "a");
        write_file(tmp.path(), "private/secret.md", "b");
        write_file(tmp.path(), ".tsmignore", "private/\n");
        let walker = walker_in(&tmp);
        assert!(!walker.is_ignored(&tmp.path().join("public/ok.md")));
        assert!(walker.is_ignored(&tmp.path().join("private/secret.md")));
        // Outside index_root → ignored.
        assert!(walker.is_ignored(Path::new("/etc/passwd")));
    }

    // ─── Backward compatibility with hidden dirs ──────────────────────

    #[test]
    #[serial_test::serial]
    fn fallback_excludes_hidden_dirs_when_no_tsmignore() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "notes/a.md", "a");
        write_file(tmp.path(), ".obsidian/plugin.md", "b");
        // No .tsmignore on disk — the synthetic `.*/` fallback must cover .obsidian.
        let walker = walker_in(&tmp);
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("notes/a.md")));
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == ".obsidian")));
    }

    #[test]
    #[serial_test::serial]
    fn tsmignore_presence_disables_hidden_fallback() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "notes/a.md", "a");
        write_file(tmp.path(), ".obsidian/plugin.md", "b");
        // Empty .tsmignore means: user has taken control, no synthetic fallback.
        write_file(tmp.path(), ".tsmignore", "");
        let walker = walker_in(&tmp);
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with(".obsidian/plugin.md")));
    }

    // ─── respect_gitignore ────────────────────────────────────────────

    fn cfg_with_gitignore(root: &Path, respect: bool) -> ResolvedConfig {
        let toml = format!(
            r#"
index_root = "{}"
state_dir = "/tmp/unused-state"
[index]
respect_gitignore = {}
"#,
            root.display(),
            respect
        );
        let file_cfg: crate::config::ConfigFile = toml::from_str(&toml).unwrap();
        ResolvedConfig::from_config_file(&file_cfg, root.to_path_buf())
    }

    #[test]
    #[serial_test::serial]
    fn respect_gitignore_true_excludes_gitignored_files() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "notes/a.md", "a");
        write_file(tmp.path(), "build/out.md", "b");
        write_file(tmp.path(), ".gitignore", "build/\n");
        write_file(tmp.path(), ".tsmignore", ""); // disable hidden-dir fallback
        let walker = ContentWalker::from_config(&cfg_with_gitignore(tmp.path(), true));
        let files = walker.collect_files();
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == "build")));
    }

    #[test]
    #[serial_test::serial]
    fn respect_gitignore_false_ignores_gitignore() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "build/out.md", "b");
        write_file(tmp.path(), ".gitignore", "build/\n");
        write_file(tmp.path(), ".tsmignore", "");
        let walker = ContentWalker::from_config(&cfg_with_gitignore(tmp.path(), false));
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("build/out.md")));
    }

    // ─── Extension allowlist ──────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn default_extensions_only_md() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "notes/a.md", "md");
        write_file(tmp.path(), "notes/b.txt", "txt");
        let walker = walker_in(&tmp);
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("a.md")));
        assert!(!files.iter().any(|p| p.ends_with("b.txt")));
    }

    #[test]
    #[serial_test::serial]
    fn custom_extensions_broaden_scope() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "notes/a.md", "md");
        write_file(tmp.path(), "notes/b.txt", "txt");
        let toml = format!(
            r#"
index_root = "{}"
state_dir = "/tmp/unused-state"
[index]
extensions = ["md", "txt"]
"#,
            tmp.path().display()
        );
        let file_cfg: crate::config::ConfigFile = toml::from_str(&toml).unwrap();
        let cfg = ResolvedConfig::from_config_file(&file_cfg, tmp.path().to_path_buf());
        let walker = ContentWalker::from_config(&cfg);
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("a.md")));
        assert!(files.iter().any(|p| p.ends_with("b.txt")));
    }

    // ─── Watch directory discovery ────────────────────────────────────

    // ─── content_dirs mode × .tsmignore ───────────────────────────────

    /// Build a ResolvedConfig with explicit `content_dirs`. All the tests
    /// above implicitly run in auto-discover mode; this helper exercises the
    /// other branch of `collection_roots()`.
    fn cfg_with_content_dirs(root: &Path, dirs: &[&str]) -> ResolvedConfig {
        let entries: String = dirs
            .iter()
            .map(|d| format!("[[index.content_dirs]]\npath = \"{d}\"\n"))
            .collect();
        let toml = format!(
            r#"index_root = "{}"
state_dir = "/tmp/unused-state"
{}"#,
            root.display(),
            entries
        );
        let file_cfg: crate::config::ConfigFile = toml::from_str(&toml).unwrap();
        ResolvedConfig::from_config_file(&file_cfg, root.to_path_buf())
    }

    #[test]
    #[serial_test::serial]
    fn content_dirs_mode_respects_tsmignore() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "keep/a.md", "a");
        write_file(tmp.path(), "keep/nested/b.md", "b");
        write_file(tmp.path(), "drop/c.md", "c");
        write_file(tmp.path(), ".tsmignore", "drop/\n");
        let walker =
            ContentWalker::from_config(&cfg_with_content_dirs(tmp.path(), &["keep", "drop"]));
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("keep/a.md")));
        assert!(files.iter().any(|p| p.ends_with("keep/nested/b.md")));
        // The `drop/` content_dir is silently skipped because its path is
        // covered by .tsmignore — verify nothing under it is collected.
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == "drop")));
    }

    // ─── project_root != index_root ───────────────────────────────────
    //
    // `.tsmignore` lives in project_root (CWD, next to tsm.toml) but its
    // patterns resolve relative to index_root. These tests exercise the
    // split — they're the #134 spec checklist items.

    /// Build a config where project_root and index_root differ. Used for
    /// tests that exercise the split — `.tsmignore` lookup uses
    /// `project_root`, pattern resolution uses `index_root`.
    fn cfg_split(project_root: &Path, index_root: &Path) -> ResolvedConfig {
        let toml = format!(
            r#"
index_root = "{}"
state_dir = "/tmp/unused-state"
"#,
            index_root.display()
        );
        let file_cfg: crate::config::ConfigFile = toml::from_str(&toml).unwrap();
        ResolvedConfig::from_config_file(&file_cfg, project_root.to_path_buf())
    }

    #[test]
    fn tsmignore_patterns_resolve_relative_to_index_root_not_cwd() {
        // Two independent tempdirs: project_root holds .tsmignore, index_root
        // is the separate tree being scanned. Pattern-confusion trap: same
        // name exists under both roots; if the matcher were (wrongly)
        // anchored at project_root, `project/private/` would be excluded
        // while `index/private/` would slip through.
        let project = TempDir::new().unwrap();
        let index = TempDir::new().unwrap();
        write_file(project.path(), ".tsmignore", "private/\n");
        write_file(
            project.path(),
            "private/unrelated.md",
            "should-never-be-scanned",
        );
        write_file(index.path(), "public/ok.md", "a");
        write_file(index.path(), "private/secret.md", "b");

        let walker = ContentWalker::from_config(&cfg_split(project.path(), index.path()));
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("public/ok.md")));
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == "private")));
    }

    #[test]
    fn tsmignore_placed_in_index_root_is_ignored() {
        // Spec checklist: ".tsmignore in index_root (when different from
        // project root) is ignored". project_root has no .tsmignore, so
        // the synthetic hidden-dir fallback is the only active rule — the
        // index_root/.tsmignore must NOT be picked up.
        let project = TempDir::new().unwrap();
        let index = TempDir::new().unwrap();
        // This .tsmignore would drop public/ok.md IF it were honored.
        write_file(index.path(), ".tsmignore", "public/\n");
        write_file(index.path(), "public/ok.md", "a");

        let walker = ContentWalker::from_config(&cfg_split(project.path(), index.path()));
        let files = walker.collect_files();
        // File is still collected → index_root/.tsmignore was not loaded.
        assert!(files.iter().any(|p| p.ends_with("public/ok.md")));
    }

    // ─── new() constructor ─────────────────────────────────────────────

    #[test]
    fn new_takes_raw_params_without_resolved_config() {
        // The point of `new()` is to let callers (notably the daemon's
        // per-request handler) construct a walker with an `index_root` that
        // differs from the loaded config WITHOUT mutating the singleton's
        // pub fields. Verify the constructor accepts raw params and produces
        // a working walker.
        let project = TempDir::new().unwrap();
        let index = TempDir::new().unwrap();
        write_file(project.path(), ".tsmignore", "skip/\n");
        write_file(index.path(), "keep/a.md", "a");
        write_file(index.path(), "skip/b.md", "b");

        let walker = ContentWalker::new(
            index.path().to_path_buf(),
            project.path(),
            vec!["md".into()],
            vec![],
            false,
            ".tsmignore",
        );
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("keep/a.md")));
        assert!(!files
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == "skip")));
    }

    #[test]
    #[serial_test::serial]
    fn tsmignore_in_subdirectory_has_no_effect() {
        // Spec checklist: ".tsmignore files in subdirectories are correctly
        // ignored". Only the root-level .tsmignore is consulted — no
        // hierarchical merging à la git.
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "notes/a.md", "a");
        // A nested .tsmignore that WOULD exclude a.md if it were honored.
        write_file(tmp.path(), "notes/.tsmignore", "a.md\n");
        let walker = walker_in(&tmp);
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("notes/a.md")));
    }
}
