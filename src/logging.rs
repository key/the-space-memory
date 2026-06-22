use std::io::Write;
use std::sync::OnceLock;

use flexi_logger::{Age, Cleanup, Criterion, DeferredNow, FileSpec, Logger, Naming};
use log::Record;

use crate::config;

pub enum LogMode {
    /// CLI (tsm) — log to stderr.
    Stderr,
    /// Daemon children (embedder, watcher) — log to stderr, which `cmd_start`
    /// captures into a single `tsmd-stderr.log` instead of inheriting the
    /// terminal. They keep no own files. Terminal log spam is prevented by that
    /// capture (and by routing user-facing output through `println!`), not by
    /// lowering the log level — so child lifecycle logs stay visible in the file.
    DaemonStderr,
    /// Daemon main (tsmd) — structured log to file with daily rotation
    Daemon { name: &'static str },
}

static LOGGER_INIT: OnceLock<Result<(), String>> = OnceLock::new();

/// Default log level spec, overridable via `RUST_LOG`.
///
/// All modes default to `info`. Terminal log spam — the original problem — is
/// prevented structurally (the daemon/children no longer inherit the terminal:
/// their stderr is captured to a file, and user-facing output goes through
/// `println!`), not by lowering the level. Keeping `info` ensures lifecycle and
/// diagnostic logs stay visible in the log files.
fn default_log_spec(_mode: &LogMode) -> &'static str {
    "info"
}

/// Initialize the logger. Safe to call multiple times (idempotent via OnceLock).
pub fn init_logger(mode: LogMode) -> anyhow::Result<()> {
    let result = LOGGER_INIT.get_or_init(|| {
        let logger = Logger::try_with_env_or_str(default_log_spec(&mode))
            .map_err(|e| format!("failed to parse log spec: {e}"))?
            .use_utc();
        match mode {
            LogMode::Stderr | LogMode::DaemonStderr => logger
                .log_to_stderr()
                .format(tsm_log_format)
                .start()
                .map(|_| ())
                .map_err(|e| format!("failed to start stderr logger: {e}")),
            LogMode::Daemon { name } => {
                let dir = config::log_dir();
                std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("failed to create log dir {}: {e}", dir.display()))?;
                logger
                    .log_to_file(
                        FileSpec::default()
                            .directory(dir)
                            .basename(name)
                            .suffix("log"),
                    )
                    .rotate(
                        Criterion::Age(Age::Day),
                        Naming::Timestamps,
                        Cleanup::KeepLogFiles(3),
                    )
                    .format(tsm_log_format)
                    .start()
                    .map(|_| ())
                    .map_err(|e| format!("failed to start file logger: {e}"))
            }
        }
    });
    result
        .as_ref()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn tsm_log_format(
    w: &mut dyn Write,
    now: &mut DeferredNow,
    record: &Record,
) -> std::io::Result<()> {
    let module = short_module(record.module_path().unwrap_or("?"));
    write!(
        w,
        "[{}] [{:5}] [{}] {}",
        now.format("%Y-%m-%dT%H:%M:%SZ"),
        record.level(),
        module,
        record.args()
    )
}

fn short_module(path: &str) -> &str {
    path.split("::").last().unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_module_nested() {
        assert_eq!(
            short_module("the_space_memory::indexer::backfill_vectors"),
            "backfill_vectors"
        );
    }

    #[test]
    fn test_short_module_single() {
        assert_eq!(short_module("tsm"), "tsm");
    }

    #[test]
    fn test_short_module_empty() {
        assert_eq!(short_module("?"), "?");
    }

    #[test]
    fn test_default_log_spec_stderr_is_info() {
        assert_eq!(default_log_spec(&LogMode::Stderr), "info");
    }

    #[test]
    fn test_default_log_spec_daemon_is_info() {
        assert_eq!(default_log_spec(&LogMode::Daemon { name: "tsmd" }), "info");
    }

    #[test]
    fn test_default_log_spec_daemon_stderr_is_info() {
        // Levels stay at info; terminal spam is prevented by capturing child
        // stderr to a file and routing results through println!, not by lowering
        // the level — so child lifecycle logs remain visible.
        assert_eq!(default_log_spec(&LogMode::DaemonStderr), "info");
    }

    #[test]
    fn test_init_logger_stderr_does_not_panic() {
        // OnceLock makes this idempotent — safe even if another test already initialized
        let result = init_logger(LogMode::Stderr);
        assert!(result.is_ok());
    }
}
