//! Unified path filtering and file collection for indexing.
//!
//! `ContentWalker` is the single source of truth for "which files should tsm
//! index?" It replaces the ad-hoc walker logic that used to be scattered
//! across `cli::collect_content_files`, `cli::discover_watch_dirs`, the
//! watcher event filter, and the stdin path.
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
    /// Construct a walker from a fully-resolved config.
    ///
    /// The `.tsmignore` file is read from the current working directory (the
    /// directory containing `tsm.toml`). Missing files are silently treated as
    /// empty — this is the common case for users who have not opted in.
    pub fn from_config(cfg: &ResolvedConfig) -> Self {
        let project_root = std::env::current_dir().unwrap_or_else(|_| cfg.index_root.clone());
        let matcher = build_matcher(
            &cfg.index_root,
            &project_root,
            &cfg.ignore_file,
            cfg.respect_gitignore,
        );
        Self {
            index_root: cfg.index_root.clone(),
            content_dirs: cfg.content_dirs.clone(),
            extensions: cfg.extensions.clone(),
            matcher,
        }
    }

    /// Convenience constructor that reads the current config singleton.
    pub fn from_env() -> Self {
        Self::from_config(&crate::config::ResolvedConfig::from_env())
    }

    /// True when `path` must not be indexed.
    ///
    /// Used for single-path decisions (watcher events, stdin filtering).
    /// Paths outside `index_root` are considered ignored so callers cannot
    /// accidentally index content the walker would never discover.
    pub fn is_ignored(&self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.index_root) else {
            return true;
        };
        if has_forced_excluded_component(rel) {
            return true;
        }
        // is_dir() is a best-effort check (may fail for deleted events from the
        // watcher); treat unknown as file, which is the conservative match.
        let is_dir = path.is_dir();
        self.matcher
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
    }

    /// Returns true when `path`'s extension is in the allowlist.
    /// Files without an extension or with an unlisted one are rejected.
    pub fn extension_allowed(&self, path: &Path) -> bool {
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

    /// Top-level directories the watcher should register for inotify.
    ///
    /// Mirrors `collect_files` roots so watcher and indexer cover the same
    /// tree. Ignored top-level dirs are dropped here too, preventing useless
    /// recursive watches under `.git/`.
    pub fn watch_dirs(&self) -> Vec<PathBuf> {
        self.collection_roots()
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
            if path.is_dir() {
                self.walk_dir(&path, out);
            } else if self.extension_allowed(&path) {
                out.push(path);
            }
        }
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
            log::warn!(
                "ignoring {} due to parse error: {err}",
                tsmignore_path.display()
            );
        }
    } else if let Err(err) = builder.add_line(None, FALLBACK_HIDDEN_DIR_PATTERN) {
        // This should never fail for a hard-coded literal pattern.
        log::warn!("failed to install fallback hidden-dir pattern: {err}");
    }

    builder.build().unwrap_or_else(|err| {
        log::warn!("failed to build ignore matcher: {err}; no patterns applied");
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

    fn cfg_for(root: &Path) -> ResolvedConfig {
        let toml = format!(
            r#"
index_root = "{}"
state_dir = "/tmp/unused-state"
"#,
            root.display()
        );
        let file_cfg: crate::config::ConfigFile = toml::from_str(&toml).unwrap();
        ResolvedConfig::from_config_file(&file_cfg)
    }

    /// Build a walker where the project root (for `.tsmignore` lookup) and
    /// `index_root` are the same directory — the common single-repo case.
    fn walker_in(tempdir: &TempDir) -> ContentWalker {
        // Mutate current_dir so `.tsmignore` is found in `tempdir`. Tests that
        // touch CWD must be serial; these are.
        std::env::set_current_dir(tempdir.path()).unwrap();
        let cfg = cfg_for(tempdir.path());
        ContentWalker::from_config(&cfg)
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
        ResolvedConfig::from_config_file(&file_cfg)
    }

    #[test]
    #[serial_test::serial]
    fn respect_gitignore_true_excludes_gitignored_files() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "notes/a.md", "a");
        write_file(tmp.path(), "build/out.md", "b");
        write_file(tmp.path(), ".gitignore", "build/\n");
        write_file(tmp.path(), ".tsmignore", ""); // disable hidden-dir fallback
        std::env::set_current_dir(tmp.path()).unwrap();
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
        std::env::set_current_dir(tmp.path()).unwrap();
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
        let cfg = ResolvedConfig::from_config_file(&file_cfg);
        std::env::set_current_dir(tmp.path()).unwrap();
        let walker = ContentWalker::from_config(&cfg);
        let files = walker.collect_files();
        assert!(files.iter().any(|p| p.ends_with("a.md")));
        assert!(files.iter().any(|p| p.ends_with("b.txt")));
    }

    // ─── Watch directory discovery ────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn watch_dirs_skips_excluded_subtrees() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("notes")).unwrap();
        std::fs::create_dir_all(tmp.path().join("private")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        write_file(tmp.path(), ".tsmignore", "private/\n");
        let walker = walker_in(&tmp);
        let dirs = walker.watch_dirs();
        assert!(dirs.iter().any(|p| p.ends_with("notes")));
        assert!(!dirs.iter().any(|p| p.ends_with("private")));
        assert!(!dirs.iter().any(|p| p.ends_with(".git")));
    }
}
