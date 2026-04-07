mod backfill;
mod child;
mod daemon_mode;
mod embedder_mode;
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
    if args.embedder {
        embedder_mode::run(args.model, args.no_idle_timeout)
    } else if args.fs_watcher {
        watcher_mode::run()
    } else {
        daemon_mode::run(args)
    }
}
