# Configuration Reference

tsm is configured through environment variables and an optional TOML config file (`tsm.toml`).

## Priority

```text
env var  >  tsm.toml  >  built-in default
```

## Config File Search Order

The `tsm` CLI resolves the **project root** in this order (first match wins),
then loads `<project_root>/tsm.toml` from it (ADR-0009 §2):

1. `$TSM_CONFIG` — its parent directory becomes the project root, and that file
   is the config
2. the current directory, if it contains `tsm.toml`
3. `--project-root <DIR>`, if it contains `tsm.toml`

There is no XDG (`~/.config/tsm/`) fallback on this path: an explicit project
root is authoritative. When none of the above resolves a root, commands fail
(except `tsm init` / `tsm setup`, which fall back to the current directory).

The `tsmd` daemon, started without an injected root, uses legacy discovery,
which additionally consults the user config directory:

1. `$TSM_CONFIG` — explicit override path
2. `./tsm.toml` — working directory
3. the platform user config directory (`~/.config/tsm/config.toml` on Linux;
   `~/Library/Application Support/tsm/config.toml` on macOS)

## Environment Variables

| Env Var | Type | Default | toml field | Description |
|---|---|---|---|---|
| `TSM_CONFIG` | path | _(none)_ | _(no toml equiv)_ | Override path to the config file itself; the directory containing it becomes the project root |
| `TSM_STATE_DIR` | path | `.tsm` | `state_dir` | Root directory for all tsm data (DB, sockets, PID, logs, user dict) |
| `TSM_CACHE_DIR` | path | `{XDG_CACHE_HOME}/tsm` (else `$HOME/.cache/tsm`) | `cache_dir` | Cache directory for the model and WordNet DB |
| `TSM_EMBEDDER_SOCKET` | path | `{state_dir}/embedder.sock` | `embedder_socket_path` | UNIX socket path for the embedder child process |
| `TSM_DAEMON_SOCKET` | path | `{state_dir}/daemon.sock` | `daemon_socket_path` | UNIX socket path for tsmd |
| `TSM_LOG_DIR` | path | `{state_dir}/logs` | `log_dir` | Directory for daemon log files |
| `TSM_EMBEDDER_IDLE_TIMEOUT` | u64 (seconds) | `600` | `embedder_idle_timeout_secs` | Idle timeout before embedder auto-shutdown (0 = never). Note: tsmd spawns embedder with `--no-idle-timeout`; this only affects standalone runs |
| `TSM_EMBEDDER_BACKFILL_INTERVAL` | u64 (seconds) | `300` | `embedder_backfill_interval_secs` | Seconds between periodic vector backfill checks (0 = disable) |
| `TSM_SEARCH_FALLBACK` | enum | `"error"` | `search_fallback` | Behavior when embedder is down: `error` or `fts_only` |
| `TSM_USER_DICT` | path | `{state_dir}/user_dict.simpledic` | `user_dict_path` | Path to the lindera user dictionary |
| `TSM_SETUP_LINK_MODE` | enum | `symlink` | `[setup].link_mode` | How `tsm setup` materializes cached resources: `symlink` or `copy` |
| `TSM_INIT_LINK_MODE` | enum | `symlink` | `[init].link_mode` | How `tsm init` links workspace resources to the cache: `symlink` or `copy` |

## Standard and External Variables

These are not tsm-specific; tsm reads (and in one case sets) well-known
variables. They have no `tsm.toml` equivalent.

| Env Var | Default | Description |
|---|---|---|
| `RUST_LOG` | `info` | Log level / filter spec (parsed by the logger) |
| `NO_COLOR` | _(unset)_ | When set, disables colored terminal output |
| `HF_HUB_CACHE` | `{XDG_CACHE_HOME}/tsm/models` | Hugging Face Hub model cache. `tsm setup` sets this automatically if unset; override to redirect the cache |
| `XDG_CACHE_HOME` | `$HOME/.cache` | Fallback input for the `TSM_CACHE_DIR` default (XDG Base Directory convention) |
| `HOME` | _(OS-provided)_ | Fallback input for the `TSM_CACHE_DIR` default when `XDG_CACHE_HOME` is unset |

## tsm.toml Full Example

```toml
# Root directory for all tsm state files: DB, sockets, PID, logs, user dict.
# Default: .tsm (relative to working directory)
state_dir = ".tsm"

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
# Paths are relative to the project root. Absolute paths are rejected with a warning.
# When content_dirs is empty, tsm auto-discovers all .md files under the project root.
[[index.content_dirs]]
# Directory path relative to the project root (required).
path = "notes"
# Score multiplier for results from this directory. 1.0 is neutral;
# > 1.0 boosts these results (e.g. 1.2, 1.5, 3.0 — no upper bound),
# 0 < weight < 1.0 attenuates them. Non-finite or <= 0 values trigger a
# warning and fall back to 1.0.
# Default: 1.0
weight = 1.2
# Time-decay half-life in days for documents in this directory.
# 0 disables time decay (documents are treated as timeless).
# Negative or non-finite values trigger a warning and fall back to 90.0.
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
# 0 disables time decay; negative or non-finite values fall back to the default.
# Default: 30.0
half_life_days = 30.0
```

## content_dirs Details

### Scoring Parameters

Each entry carries two scoring knobs:

- **`weight`** — a multiplier applied to the final score of results from this
  directory. `1.0` is neutral, `> 1.0` boosts (there is **no upper bound** —
  `1.1`, `1.5`, `3.0` are all valid), and `0 < weight < 1.0` attenuates. Values
  that are non-finite or `<= 0` are rejected with a warning and fall back to
  `1.0`. To make one directory rank above the rest, raise its weight above
  `1.0` rather than lowering every other directory.
- **`half_life_days`** — time-decay half-life. `0` disables decay (timeless);
  negative or non-finite values warn and fall back to the default (`90.0` for
  `content_dirs`, `30.0` for Claude sessions).

### Path Matching

- Paths in `content_dirs` are relative to the project root; absolute paths are rejected with a warning
- Matching uses prefix + `/` boundary check: `notes/foo.md` matches `path = "notes"`, but `notes-extra/bar.md` does not
- Entries are sorted longest-first so more-specific paths take precedence over shorter prefixes
- Unmatched files fall back to source-type defaults (see below)

### Auto-Discover Mode

When `content_dirs` is empty, tsm recursively indexes all `.md` files under the project root.
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
