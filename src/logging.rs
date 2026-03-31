use std::io::Write;
use std::sync::OnceLock;

use flexi_logger::{
    Age, Cleanup, Criterion, DeferredNow, Duplicate, FileSpec, Logger, Naming,
};
use log::Record;

use crate::config;

pub enum LogMode {
    /// CLI (tsm) — log to stderr only
    Stderr,
    /// Daemon (tsmd, tsm-embedder, tsm-watcher) — log to file with daily rotation
    Daemon { name: &'static str },
}

static LOGGER_INIT: OnceLock<Result<(), String>> = OnceLock::new();

/// Initialize the logger. Safe to call multiple times (idempotent via OnceLock).
pub fn init_logger(mode: LogMode) -> anyhow::Result<()> {
    let result = LOGGER_INIT.get_or_init(|| {
        let logger = Logger::try_with_env_or_str("info")
            .map_err(|e| format!("failed to parse log spec: {e}"))?
            .use_utc();
        match mode {
            LogMode::Stderr => {
                logger
                    .log_to_stderr()
                    .format(tsm_log_format)
                    .start()
                    .map(|_| ())
                    .map_err(|e| format!("failed to start stderr logger: {e}"))
            }
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
                    .duplicate_to_stderr(Duplicate::Warn)
                    .rotate(
                        Criterion::Age(Age::Day),
                        Naming::Timestamps,
                        Cleanup::KeepLogFiles(7),
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
        assert_eq!(short_module("the_space_memory::indexer::backfill_vectors"), "backfill_vectors");
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
    fn test_init_logger_stderr_does_not_panic() {
        // OnceLock makes this idempotent — safe even if another test already initialized
        let result = init_logger(LogMode::Stderr);
        assert!(result.is_ok());
    }
}
