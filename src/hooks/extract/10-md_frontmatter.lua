-- Default extract hook: reproduces frontmatter.rs behavior.
-- ctx.frontmatter is the full parsed YAML mapping (all top-level keys).
function extract(ctx)
  local fm = ctx.frontmatter or {}
  return { status = fm.status, effective_date = fm.updated }
end
