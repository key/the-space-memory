fn main() -> anyhow::Result<()> {
    // When spawned as a backfill-worker subprocess (via WorkerHandle::spawn
    // which calls `current_exe() backfill-worker`), dispatch to the worker
    // entry point instead of starting a full embedder server.
    if std::env::args_os().nth(1).is_some_and(|a| a == "backfill-worker") {
        the_space_memory::config::ensure_model_cache_env();
        the_space_memory::logging::init_logger(
            the_space_memory::logging::LogMode::Daemon { name: "tsm-embedder" },
        )?;
        return the_space_memory::cli::cmd_backfill_worker();
    }

    the_space_memory::config::ensure_model_cache_env();
    the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::Daemon { name: "tsm-embedder" })?;
    the_space_memory::cli::cmd_embedder_start(None)
}
