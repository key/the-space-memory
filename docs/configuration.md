# Configuration Reference

tsm is configured through environment variables and an optional TOML config file (`tsm.toml`).

## Priority

```text
env var  >  tsm.toml  >  built-in default
```

## Config File Search Order

tsm searches for a config file in the following order; the first file found wins for each field:

1. `$TSM_CONFIG` — explicit override path
2. `./tsm.toml` — working directory
3. `~/.config/tsm/config.toml` — user-level config (XDG)

## Environment Variables

| Env Var | Type | Default | toml field | Description |
|---|---|---|---|---|
| `TSM_CONFIG` | path | _(none)_ | _(no toml equiv)_ | Override path to the config file itself |
| `TSM_STATE_DIR` | path | `.tsm` | `state_dir` | Root directory for all tsm data (DB, sockets, PID, logs, user dict) |
| `TSM_INDEX_ROOT` | path | `/workspaces` | `index_root` | Root directory containing content workspaces to index |
| `TSM_EMBEDDER_SOCKET` | path | `{state_dir}/embedder.sock` | `embedder_socket_path` | UNIX socket path for the embedder child process |
| `TSM_DAEMON_SOCKET` | path | `{state_dir}/daemon.sock` | `daemon_socket_path` | UNIX socket path for tsmd |
| `TSM_LOG_DIR` | path | `{state_dir}/logs` | `log_dir` | Directory for daemon log files |
| `TSM_EMBEDDER_IDLE_TIMEOUT` | u64 (seconds) | `600` | `embedder_idle_timeout_secs` | Idle timeout before embedder auto-shutdown (0 = never). Note: tsmd spawns embedder with `--no-idle-timeout`; this only affects standalone runs |
| `TSM_EMBEDDER_BACKFILL_INTERVAL` | u64 (seconds) | `300` | `embedder_backfill_interval_secs` | Seconds between periodic vector backfill checks (0 = disable) |
| `TSM_SEARCH_FALLBACK` | enum | `"error"` | `search_fallback` | Behavior when embedder is down: `error` or `fts_only` |
| `TSM_USER_DICT` | path | `{state_dir}/user_dict.simpledic` | `user_dict_path` | Path to the lindera user dictionary |
| `HF_HUB_CACHE` | path | `{XDG_CACHE_HOME}/tsm/models/` | _(no toml equiv)_ | Model cache directory for HuggingFace Hub |

## tsm.toml Full Example

```toml
# Root directory for all tsm state files: DB, sockets, PID, logs, user dict.
# Default: .tsm (relative to working directory)
state_dir = ".tsm"

# Root directory containing content workspaces to index.
# Default: /workspaces
index_root = "/workspaces"

# UNIX socket for the embedder child process.
# Default: {state_dir}/embedder.sock
embedder_socket_path = ".tsm/embedder.sock"

# UNIX socket for tsmd (used by tsm CLI clients).
# Default: {state_dir}/daemon.sock
daemon_socket_path = ".tsm/daemon.sock"

# Directory for daemon log files (tsmd, tsmd --embedder, tsmd --fs-watcher).
# Default: {state_dir}/logs
log_dir = ".tsm/logs"

# Seconds of embedder inactivity before auto-shutdown. 0 = never.
# Note: tsmd spawns the embedder with --no-idle-timeout; this affects standalone runs only.
# Default: 600
embedder_idle_timeout_secs = 600

# Seconds between periodic vector backfill checks. 0 = disable.
# Default: 300
embedder_backfill_interval_secs = 300

# Behavior when the embedder is unavailable during search.
# "error"    — refuse to search (default, ensures full hybrid search)
# "fts_only" — fall back to FTS5-only with a warning
# Default: "error"
search_fallback = "error"

# Path to the lindera simpledic user dictionary file.
# Default: {state_dir}/user_dict.simpledic
user_dict_path = ".tsm/user_dict.simpledic"

[index]
# Content directories to index, with per-directory scoring parameters.
# Paths are relative to index_root. Absolute paths are rejected with a warning.
# When content_dirs is empty, tsm auto-discovers all .md files under index_root.
[[index.content_dirs]]
# Directory path relative to index_root (required).
path = "notes"
# Score multiplier for results from this directory.
# Non-finite or <= 0 values trigger a warning and fall back to 1.0.
# Default: 1.0
weight = 1.2
# Time-decay half-life in days for documents in this directory.
# Non-finite or <= 0 values trigger a warning and fall back to 90.0.
# Default: 90.0
half_life_days = 120.0

[[index.content_dirs]]
path = "research"
weight = 1.0
half_life_days = 60.0

[[index.content_dirs]]
path = "projects/work"
weight = 0.8
half_life_days = 90.0

[index.claude_session]
# Score weight for Claude Code session data.
# Applied to all session: paths regardless of content_dirs configuration.
# Default: 0.3
weight = 0.3
# Time-decay half-life in days for Claude Code session data.
# Default: 30.0
half_life_days = 30.0
```

## content_dirs Details

### Path Matching

- Paths in `content_dirs` are relative to `index_root`; absolute paths are rejected with a warning
- Matching uses prefix + `/` boundary check: `notes/foo.md` matches `path = "notes"`, but `notes-extra/bar.md` does not
- Entries are sorted longest-first so more-specific paths take precedence over shorter prefixes
- Unmatched files fall back to source-type defaults (see below)

### Auto-Discover Mode

When `content_dirs` is empty, tsm recursively indexes all `.md` files under `index_root`.
Scoring parameters are derived from the source type:

| source_type | half_life_days |
|---|---|
| `note` | 120 |
| `research` | 60 |
| `session` | 30 |
| other | 90 |

### Claude Session Data

Claude session chunks always use `session_weight` and `session_half_life_days` from
`[index.claude_session]`, regardless of `content_dirs` configuration.

## Derived Paths

The following paths are computed from `state_dir` and are not independently configurable:

| Path | Description |
|---|---|
| `{state_dir}/tsm.db` | SQLite database |
| `{state_dir}/custom_terms.toml` | Custom FTS terms |
| `{state_dir}/stopwords.txt` | FTS stopwords list |
| `{state_dir}/reject_words.txt` | Rejected dictionary candidates |
| `{state_dir}/tsmd.pid` | Daemon PID file |

## Hot-Reload vs Restart

Changes take effect differently depending on the field:

**Requires `tsm restart`** (daemon must be stopped and restarted):

- `state_dir`
- `index_root`
- `daemon_socket_path`
- `embedder_socket_path`
- `log_dir`
- `user_dict_path`

**Hot-reloadable via `tsm reload`** (takes effect without restarting the daemon):

- `index.content_dirs`
- `search_fallback`
- `embedder_idle_timeout_secs`
- `embedder_backfill_interval_secs`
- `index.claude_session.weight` (`session_weight`)
- `index.claude_session.half_life_days` (`session_half_life_days`)
