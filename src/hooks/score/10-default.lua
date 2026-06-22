-- Default score hook: reproduces time_decay x status_penalty.
-- Builtins provided by core: decay(date, half_life_days).
function score(ctx)
  local m = ctx.metadata or {}
  local penalty = ({ superseded = 0.2, rejected = 0.3, dropped = 0.3, outdated = 0.4 })[m.status] or 1.0
  return penalty * decay(m.effective_date, ctx.half_life_days)
end
