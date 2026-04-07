use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use std::path::PathBuf;

use the_space_memory::config;
use the_space_memory::embedder::Embedder;
use the_space_memory::ipc::{read_message, write_message};

use candle_core::Device;

/// Entry point for `tsmd --embedder`.
pub fn run(model: Option<PathBuf>, no_idle_timeout: bool) -> Result<()> {
    config::ensure_model_cache_env();
    if no_idle_timeout {
        // Must be set BEFORE init_logger, which triggers config singleton init.
        // SAFETY: called single-threaded before any thread spawn or logger init.
        unsafe { std::env::set_var("TSM_EMBEDDER_IDLE_TIMEOUT", "0") };
    }
    the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::Daemon {
        name: "tsmd-embedder",
    })?;
    let socket_path = config::embedder_socket_path();
    run_daemon(&socket_path, model.as_deref())
}

/// Run the embedder socket server loop.
fn run_daemon(socket_path: &Path, model_dir: Option<&Path>) -> Result<()> {
    // Clean up stale socket
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }

    log::info!("Loading model...");
    let embedder = if let Some(dir) = model_dir {
        Embedder::load_from_paths(
            &dir.join("config.json"),
            &dir.join("tokenizer.json"),
            &dir.join("model.safetensors"),
            &Device::Cpu,
        )?
    } else {
        Embedder::load(&Device::Cpu)?
    };
    log::info!("Model loaded.");

    log::info!("Listening on {}", socket_path.display());

    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;

    let running = Arc::new(AtomicBool::new(true));
    let last_activity = Arc::new(std::sync::Mutex::new(Instant::now()));

    // Watchdog thread (skipped when idle timeout is 0)
    let idle_timeout_secs = config::embedder_idle_timeout_secs();
    if idle_timeout_secs > 0 {
        let running = Arc::clone(&running);
        let last_activity = Arc::clone(&last_activity);
        let socket_path = socket_path.to_path_buf();
        std::thread::spawn(move || {
            watchdog(&running, &last_activity, &socket_path, idle_timeout_secs);
        });
    } else {
        log::info!("Idle timeout disabled.");
    }

    while running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                *last_activity.lock().unwrap() = Instant::now();
                if let Err(e) = handle_client(stream, &embedder) {
                    log::warn!("Client error: {e}");
                }
                *last_activity.lock().unwrap() = Instant::now();
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                if running.load(Ordering::Relaxed) {
                    log::error!("fatal accept error: {e}; embedder shutting down");
                    running.store(false, Ordering::Relaxed);
                }
            }
        }
    }

    log::info!("Shutting down.");
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

fn watchdog(
    running: &AtomicBool,
    last_activity: &std::sync::Mutex<Instant>,
    socket_path: &Path,
    timeout_secs: u64,
) {
    let timeout = Duration::from_secs(timeout_secs);
    loop {
        std::thread::sleep(Duration::from_secs(10));
        if !running.load(Ordering::Relaxed) {
            break;
        }
        let elapsed = last_activity.lock().unwrap().elapsed();
        if elapsed >= timeout {
            log::info!("Idle timeout reached ({timeout_secs}s). Stopping.");
            running.store(false, Ordering::Relaxed);
            // Poke the listener to unblock accept
            let _ = UnixStream::connect(socket_path);
            break;
        }
    }
}

fn handle_client(mut stream: UnixStream, embedder: &Embedder) -> Result<()> {
    let request_data = read_message(&mut stream)?;
    let request: serde_json::Value = serde_json::from_slice(&request_data)?;

    let texts: Vec<String> = request
        .get("texts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let encode_result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| embedder.encode(&texts)));

    let embeddings = match encode_result {
        Ok(Ok(emb)) => emb,
        Ok(Err(e)) => {
            log::error!("Encode error: {e}");
            let err_resp = serde_json::json!({ "error": format!("{e}") });
            let err_bytes = serde_json::to_vec(&err_resp)?;
            write_message(&mut stream, &err_bytes)?;
            let _ = stream.shutdown(Shutdown::Both);
            return Ok(());
        }
        Err(panic_info) => {
            let msg = if let Some(s) = panic_info.downcast_ref::<String>() {
                s.clone()
            } else if let Some(&s) = panic_info.downcast_ref::<&str>() {
                s.to_string()
            } else {
                "unknown panic".to_string()
            };
            log::error!("PANIC in encode: {msg}");
            let err_resp = serde_json::json!({ "error": format!("panic: {msg}") });
            let err_bytes = serde_json::to_vec(&err_resp)?;
            write_message(&mut stream, &err_bytes)?;
            let _ = stream.shutdown(Shutdown::Both);
            return Ok(());
        }
    };

    let response = serde_json::json!({ "embeddings": embeddings });
    let response_bytes = serde_json::to_vec(&response)?;
    write_message(&mut stream, &response_bytes)?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}
