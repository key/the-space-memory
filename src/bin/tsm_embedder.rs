fn main() -> anyhow::Result<()> {
    let arg = std::env::args_os().nth(1);

    match arg.as_deref().and_then(|a| a.to_str()) {
        Some("backfill-worker") => {
            // Dispatched by WorkerHandle::spawn via `tsm-embedder backfill-worker`.
            // Use stderr logging — the parent's WorkerHandle reads our stderr
            // and forwards it as [worker] lines to the parent's file logger.
            // Using Daemon mode here would compete for the same log file and
            // trigger spurious rotation.
            the_space_memory::config::ensure_model_cache_env();
            the_space_memory::logging::init_logger(
                the_space_memory::logging::LogMode::Stderr,
            )?;
            the_space_memory::cli::cmd_backfill_worker()
        }
        None => {
            // No arguments — start the embedder server.
            the_space_memory::config::ensure_model_cache_env();
            the_space_memory::logging::init_logger(
                the_space_memory::logging::LogMode::Daemon { name: "tsm-embedder" },
            )?;
            the_space_memory::cli::cmd_embedder_start(None)
        }
        Some(other) => {
            eprintln!("tsm-embedder: unknown argument '{other}'");
            eprintln!("Usage: tsm-embedder                  Start the embedder server");
            eprintln!("       tsm-embedder backfill-worker   Run as a backfill worker (internal)");
            std::process::exit(1);
        }
    }
}
