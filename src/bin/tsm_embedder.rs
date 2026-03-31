fn main() -> anyhow::Result<()> {
    the_space_memory::config::ensure_model_cache_env();
    the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::Daemon { name: "tsm-embedder" })?;
    the_space_memory::cli::cmd_embedder_start(None)
}
