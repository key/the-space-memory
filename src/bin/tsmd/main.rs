mod backfill_logic;
mod backfill_proc;
mod child_proc;
mod daemon_logic;
mod daemon_proc;
mod embedder_logic;
mod embedder_proc;
mod watch_logic;
mod watcher_mode;

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

/// Global shutdown flag shared across modes and modules.
pub(crate) static SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub(crate) extern "C" fn signal_handler(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

#[derive(Parser)]
#[command(name = "tsmd", version, about = "The Space Memory daemon")]
pub(crate) struct Args {
    /// Project root to operate in (canonical absolute path).
    ///
    /// `tsm` always passes this so `ps`/`pgrep` reveal the owning project, and
    /// `tsmd` `chdir()`s here at startup so every relative state path (`.tsm/…`)
    /// resolves under it. Applies to the daemon and every spawned child.
    #[arg(long)]
    pub project_root: Option<PathBuf>,

    /// UNIX socket path
    #[arg(long, conflicts_with_all = ["embedder", "fs_watcher"])]
    pub socket: Option<PathBuf>,

    /// Database path
    #[arg(long, conflicts_with_all = ["embedder", "fs_watcher"])]
    pub db: Option<PathBuf>,

    /// Skip embedder startup
    #[arg(long, conflicts_with_all = ["embedder", "fs_watcher"])]
    pub no_embedder: bool,

    /// Skip watcher startup
    #[arg(long, conflicts_with_all = ["embedder", "fs_watcher"])]
    pub no_watcher: bool,

    /// Run as embedder subprocess (internal)
    #[arg(long, conflicts_with = "fs_watcher", hide = true)]
    embedder: bool,

    /// Model directory for embedder mode
    #[arg(long, requires = "embedder", hide = true)]
    model: Option<PathBuf>,

    /// Disable idle timeout in embedder mode
    #[arg(long, requires = "embedder", hide = true)]
    no_idle_timeout: bool,

    /// Run as fs-watcher subprocess (internal)
    #[arg(long, conflicts_with = "embedder", hide = true)]
    fs_watcher: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Establish the project working directory BEFORE any config access. The
    // config singleton (config::RESOLVED) lazily resolves `.tsm/` relative to
    // the CWD on first use, and the daemon logger triggers that init — so the
    // chdir must happen here, right after arg parsing, ahead of every mode.
    // This makes all relative state paths resolve under the project root
    // without touching config resolution.
    if let Some(root) = &args.project_root {
        let canonical = std::fs::canonicalize(root)
            .map_err(|e| anyhow::anyhow!("--project-root {}: {e}", root.display()))?;
        std::env::set_current_dir(&canonical)
            .map_err(|e| anyhow::anyhow!("chdir to {}: {e}", canonical.display()))?;
    }

    if args.embedder {
        embedder_proc::run(args.model, args.no_idle_timeout)
    } else if args.fs_watcher {
        watcher_mode::run()
    } else {
        daemon_proc::run(args)
    }
}
