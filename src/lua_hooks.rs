//! Embedded Lua runtime for user-editable metadata `extract` and scoring
//! `score` hooks (ADR-0013).

use std::path::Path;
use std::sync::{Arc, Mutex};

use mlua::{Lua, LuaOptions, StdLib};

use crate::config;

/// Per-VM memory ceiling for hook execution (bytes). Sandboxing: no stdlib
/// (`io`/`os`/`package` absent) so hooks cannot touch FS/process/network.
const LUA_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// Embedded default extract hook (reproduces frontmatter.rs behavior).
pub const DEFAULT_EXTRACT_HOOK: &str = include_str!("hooks/extract/10-md_frontmatter.lua");

/// Embedded default score hook (reproduces time_decay x status_penalty).
pub const DEFAULT_SCORE_HOOK: &str = include_str!("hooks/score/10-default.lua");

/// A single Lua hook script with its logical name and source text.
#[derive(Debug, Clone)]
pub struct HookScript {
    pub name: String,
    pub source: String,
}

/// All hook scripts for the process, split by hook type.
#[derive(Debug, Clone, Default)]
pub struct HookSources {
    pub extract: Vec<HookScript>,
    pub score: Vec<HookScript>,
}

/// Create a sandboxed Lua VM: no standard library, bounded memory.
/// Used by all later lua_hooks tasks.
fn new_sandboxed_lua() -> anyhow::Result<Lua> {
    let lua = Lua::new_with(StdLib::NONE, LuaOptions::default())?;
    lua.set_memory_limit(LUA_MEMORY_LIMIT)?;
    Ok(lua)
}

/// Read `*.lua` from `dir` sorted by file name. Empty/absent dir -> empty vec.
fn read_scripts(dir: &Path) -> anyhow::Result<Vec<HookScript>> {
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(Result::ok).map(|e| e.path()).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    entries.retain(|p| p.extension().and_then(|s| s.to_str()) == Some("lua"));
    entries.sort();
    let mut out = Vec::new();
    for p in entries {
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let source = std::fs::read_to_string(&p)?;
        out.push(HookScript { name, source });
    }
    Ok(out)
}

/// Validate each script compiles and defines `entrypoint`. Returns Err naming
/// the first offending script (fail-fast).
fn validate(scripts: &[HookScript], entrypoint: &str) -> anyhow::Result<()> {
    for s in scripts {
        let lua = new_sandboxed_lua()?;
        lua.load(&s.source)
            .exec()
            .map_err(|e| anyhow::anyhow!("hook {}: compile error: {e}", s.name))?;
        let f: mlua::Value = lua.globals().get(entrypoint)?;
        if !matches!(f, mlua::Value::Function(_)) {
            anyhow::bail!("hook {}: missing `{}` function", s.name, entrypoint);
        }
    }
    Ok(())
}

/// Discover `*.lua` scripts from disk (sorted), fall back to embedded defaults
/// when a directory has no scripts, and validate every script compiles and
/// defines its entrypoint. Returns `Err` on the first failing script (fail-fast).
pub fn load_hook_sources(extract_dir: &Path, score_dir: &Path) -> anyhow::Result<HookSources> {
    let mut extract = read_scripts(extract_dir)?;
    if extract.is_empty() {
        extract.push(HookScript {
            name: "<embedded:10-md_frontmatter.lua>".into(),
            source: DEFAULT_EXTRACT_HOOK.into(),
        });
    }
    let mut score = read_scripts(score_dir)?;
    if score.is_empty() {
        score.push(HookScript {
            name: "<embedded:10-default.lua>".into(),
            source: DEFAULT_SCORE_HOOK.into(),
        });
    }
    validate(&extract, "extract")?;
    validate(&score, "score")?;
    Ok(HookSources { extract, score })
}

static HOOKS: Mutex<Option<Arc<HookSources>>> = Mutex::new(None);

/// Eager process-wide load from `config` dirs (fail-fast, for daemon startup).
pub fn init_hooks() -> anyhow::Result<()> {
    let sources = load_hook_sources(&config::hooks_extract_dir(), &config::hooks_score_dir())?;
    let mut guard = HOOKS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(Arc::new(sources));
    Ok(())
}

/// Lazy accessor (CLI path): loads on first use. On a user-hook error, logs
/// and falls back to embedded defaults so search/index never hard-fails here.
pub fn hooks() -> Arc<HookSources> {
    let mut guard = HOOKS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = guard.as_ref() {
        return Arc::clone(h);
    }
    let sources = load_hook_sources(&config::hooks_extract_dir(), &config::hooks_score_dir())
        .unwrap_or_else(|e| {
            log::warn!("hook load failed, using embedded defaults: {e}");
            HookSources {
                extract: vec![HookScript {
                    name: "<embedded:10-md_frontmatter.lua>".into(),
                    source: DEFAULT_EXTRACT_HOOK.into(),
                }],
                score: vec![HookScript {
                    name: "<embedded:10-default.lua>".into(),
                    source: DEFAULT_SCORE_HOOK.into(),
                }],
            }
        });
    let arc = Arc::new(sources);
    *guard = Some(Arc::clone(&arc));
    arc
}

/// Test/reset helper for the process-wide cache.
#[cfg(test)]
pub fn reset_hooks_cache() {
    let mut guard = HOOKS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ── Sandbox tests (from Task 1) ─────────────────────────────────────────

    #[test]
    fn test_lua_define_call_number_and_table() {
        // Arrange: a sandboxed VM with a score-like fn and an extract-like fn.
        let lua = new_sandboxed_lua().unwrap();
        lua.load(
            r#"
            function score(ctx) return (ctx.base or 1.0) * 0.5 end
            function extract(ctx) return { status = ctx.fm_status } end
            "#,
        )
        .exec()
        .unwrap();

        // Act + Assert: call score -> f64.
        let globals = lua.globals();
        let score_fn: mlua::Function = globals.get("score").unwrap();
        let ctx = lua.create_table().unwrap();
        ctx.set("base", 2.0_f64).unwrap();
        let got: f64 = score_fn.call(ctx).unwrap();
        assert_eq!(got, 1.0);

        // Act + Assert: call extract -> table, read a key.
        let extract_fn: mlua::Function = globals.get("extract").unwrap();
        let ectx = lua.create_table().unwrap();
        ectx.set("fm_status", "current").unwrap();
        let out: mlua::Table = extract_fn.call(ectx).unwrap();
        let status: Option<String> = out.get("status").unwrap();
        assert_eq!(status.as_deref(), Some("current"));
    }

    #[test]
    fn test_sandbox_has_no_io() {
        // Arrange/Act: io should be nil under StdLib::NONE.
        let lua = new_sandboxed_lua().unwrap();
        let is_nil: bool = lua.load("return io == nil").eval().unwrap();
        // Assert
        assert!(is_nil, "io must be unavailable in sandboxed VM");
    }

    // ── Discovery + fallback + fail-fast tests (Task 3) ────────────────────

    #[test]
    fn test_load_falls_back_to_embedded_when_dirs_empty() {
        let tmp = TempDir::new().unwrap();
        let ex = tmp.path().join("extract");
        let sc = tmp.path().join("score");
        fs::create_dir_all(&ex).unwrap();
        fs::create_dir_all(&sc).unwrap();
        let s = load_hook_sources(&ex, &sc).unwrap();
        assert_eq!(s.extract.len(), 1);
        assert_eq!(s.extract[0].name, "<embedded:10-md_frontmatter.lua>");
        assert_eq!(s.score.len(), 1);
        assert!(s.score[0].source.contains("function score"));
    }

    #[test]
    fn test_load_reads_sorted_disk_scripts() {
        let tmp = TempDir::new().unwrap();
        let ex = tmp.path().join("extract");
        let sc = tmp.path().join("score");
        fs::create_dir_all(&ex).unwrap();
        fs::create_dir_all(&sc).unwrap();
        fs::write(ex.join("20-b.lua"), "function extract(ctx) return {} end").unwrap();
        fs::write(ex.join("10-a.lua"), "function extract(ctx) return {} end").unwrap();
        fs::write(ex.join("notes.txt"), "ignored").unwrap();
        fs::write(
            sc.join("10-default.lua"),
            "function score(ctx) return 1.0 end",
        )
        .unwrap();
        let s = load_hook_sources(&ex, &sc).unwrap();
        assert_eq!(
            s.extract
                .iter()
                .map(|h| h.name.as_str())
                .collect::<Vec<_>>(),
            vec!["10-a.lua", "20-b.lua"]
        );
    }

    #[test]
    fn test_load_fails_fast_on_syntax_error() {
        let tmp = TempDir::new().unwrap();
        let ex = tmp.path().join("extract");
        let sc = tmp.path().join("score");
        fs::create_dir_all(&ex).unwrap();
        fs::create_dir_all(&sc).unwrap();
        fs::write(ex.join("10-bad.lua"), "function extract(ctx) return {").unwrap();
        fs::write(
            sc.join("10-default.lua"),
            "function score(ctx) return 1.0 end",
        )
        .unwrap();
        let err = load_hook_sources(&ex, &sc).unwrap_err();
        assert!(err.to_string().contains("10-bad.lua"));
    }

    #[test]
    fn test_load_fails_fast_on_missing_entrypoint() {
        let tmp = TempDir::new().unwrap();
        let ex = tmp.path().join("extract");
        let sc = tmp.path().join("score");
        fs::create_dir_all(&ex).unwrap();
        fs::create_dir_all(&sc).unwrap();
        fs::write(ex.join("10-noentry.lua"), "local x = 1").unwrap();
        fs::write(
            sc.join("10-default.lua"),
            "function score(ctx) return 1.0 end",
        )
        .unwrap();
        let err = load_hook_sources(&ex, &sc).unwrap_err();
        assert!(err.to_string().contains("extract"));
    }

    // ── Process-wide cache tests (Task 3) ──────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn test_init_hooks_loads_embedded_defaults_when_dirs_absent() {
        // Arrange: point state_dir at a temp dir with no hooks subdirs.
        let tmp = TempDir::new().unwrap();
        unsafe { std::env::set_var("TSM_STATE_DIR", tmp.path()) };
        reset_hooks_cache();

        // Act
        let result = init_hooks();

        // Assert
        unsafe { std::env::remove_var("TSM_STATE_DIR") };
        reset_hooks_cache();
        result.unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn test_hooks_lazy_accessor_returns_populated_sources() {
        // Arrange: temp state_dir, no user hooks -> embedded defaults used.
        let tmp = TempDir::new().unwrap();
        unsafe { std::env::set_var("TSM_STATE_DIR", tmp.path()) };
        reset_hooks_cache();

        // Act
        let h = hooks();

        // Assert: at least the embedded default in each slot.
        unsafe { std::env::remove_var("TSM_STATE_DIR") };
        reset_hooks_cache();
        assert!(!h.extract.is_empty());
        assert!(!h.score.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn test_hooks_returns_cached_arc_on_second_call() {
        // Arrange
        let tmp = TempDir::new().unwrap();
        unsafe { std::env::set_var("TSM_STATE_DIR", tmp.path()) };
        reset_hooks_cache();

        // Act
        let a = hooks();
        let b = hooks();

        // Assert: same allocation.
        unsafe { std::env::remove_var("TSM_STATE_DIR") };
        reset_hooks_cache();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    #[serial_test::serial]
    fn test_hooks_falls_back_to_embedded_on_bad_user_hook() {
        // Arrange: user hook that fails to compile.
        let tmp = TempDir::new().unwrap();
        let ex = tmp.path().join("hooks/extract");
        fs::create_dir_all(&ex).unwrap();
        fs::write(ex.join("10-bad.lua"), "function extract(ctx) return {").unwrap();
        unsafe { std::env::set_var("TSM_STATE_DIR", tmp.path()) };
        reset_hooks_cache();

        // Act: lazy accessor should not panic; falls back to embedded.
        let h = hooks();

        // Assert: still has sources (embedded defaults).
        unsafe { std::env::remove_var("TSM_STATE_DIR") };
        reset_hooks_cache();
        assert!(!h.extract.is_empty());
    }
}
