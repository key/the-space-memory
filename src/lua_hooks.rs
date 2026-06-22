//! Embedded Lua runtime for user-editable metadata `extract` and scoring
//! `score` hooks (ADR-0013).

use mlua::{Lua, LuaOptions, StdLib};

/// Per-VM memory ceiling for hook execution (bytes). Sandboxing: no stdlib
/// (`io`/`os`/`package` absent) so hooks cannot touch FS/process/network.
const LUA_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// Create a sandboxed Lua VM: no standard library, bounded memory.
/// Used by all later lua_hooks tasks.
#[allow(dead_code)]
fn new_sandboxed_lua() -> anyhow::Result<Lua> {
    let lua = Lua::new_with(StdLib::NONE, LuaOptions::default())?;
    lua.set_memory_limit(LUA_MEMORY_LIMIT)?;
    Ok(lua)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
