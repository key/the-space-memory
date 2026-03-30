use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor, D};
use candle_nn::VarBuilder;
use candle_transformers::models::modernbert::{Config, ModernBert};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer};

use crate::config;

const MODEL_ID: &str = "cl-nagoya/ruri-v3-30m";

/// The embedder engine: loads model and produces embeddings.
pub struct Embedder {
    model: ModernBert,
    tokenizer: Tokenizer,
    device: Device,
}

impl Embedder {
    /// Load the ruri-v3-30m model from HuggingFace Hub cache.
    pub fn load(device: &Device) -> Result<Self> {
        let api = hf_hub::api::sync::Api::new()?;
        let repo = api.repo(hf_hub::Repo::new(
            MODEL_ID.to_string(),
            hf_hub::RepoType::Model,
        ));

        let config_path = repo.get("config.json").context("config.json not found")?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("tokenizer.json not found")?;
        let weights_path = repo
            .get("model.safetensors")
            .context("model.safetensors not found")?;

        Self::load_from_paths(&config_path, &tokenizer_path, &weights_path, device)
    }

    /// Load model from explicit file paths.
    pub fn load_from_paths(
        config_path: &Path,
        tokenizer_path: &Path,
        weights_path: &Path,
        device: &Device,
    ) -> Result<Self> {
        let config_str = std::fs::read_to_string(config_path)?;
        let config: Config = serde_json::from_str(&config_str)?;
        let pad_token_id = config.pad_token_id;

        let mut tokenizer =
            Tokenizer::from_file(tokenizer_path).map_err(|e| anyhow::anyhow!("{e}"))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            pad_id: pad_token_id,
            pad_token: "[PAD]".to_string(),
            ..Default::default()
        }));

        let tensors = load_tensors_with_prefix(weights_path, device)?;
        let vb = VarBuilder::from_tensors(tensors, DType::F32, device);
        let model = ModernBert::load(vb, &config)?;

        Ok(Self {
            model,
            tokenizer,
            device: device.clone(),
        })
    }

    /// Encode texts into embedding vectors.
    pub fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Truncate texts to avoid OOM/crash on very long inputs.
        // ruri-v3-30m supports up to 8192 tokens; we limit input chars
        // conservatively (Japanese averages ~1.5 chars/token).
        const MAX_CHARS: usize = 8192;
        let truncated: Vec<String> = texts
            .iter()
            .map(|t| {
                if t.len() > MAX_CHARS {
                    t[..t.floor_char_boundary(MAX_CHARS)].to_string()
                } else {
                    t.clone()
                }
            })
            .collect();

        let encodings = self
            .tokenizer
            .encode_batch(truncated, true)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let input_ids: Vec<Vec<u32>> = encodings.iter().map(|e| e.get_ids().to_vec()).collect();
        let attention_masks: Vec<Vec<u32>> = encodings
            .iter()
            .map(|e| e.get_attention_mask().to_vec())
            .collect();

        let batch_size = input_ids.len();
        let seq_len = input_ids[0].len();

        let input_ids_flat: Vec<u32> = input_ids.into_iter().flatten().collect();
        let mask_flat: Vec<u32> = attention_masks.into_iter().flatten().collect();

        let input_ids_tensor =
            Tensor::from_vec(input_ids_flat, (batch_size, seq_len), &self.device)?
                .to_dtype(DType::U32)?;
        let attention_mask_tensor =
            Tensor::from_vec(mask_flat, (batch_size, seq_len), &self.device)?
                .to_dtype(DType::U32)?;

        let output = self
            .model
            .forward(&input_ids_tensor, &attention_mask_tensor)?;
        let embeddings = mean_pooling(&output, &attention_mask_tensor)?;

        // Convert to Vec<Vec<f32>>
        let mut result = Vec::with_capacity(batch_size);
        for i in 0..batch_size {
            let row = embeddings.get(i)?;
            let values: Vec<f32> = row.to_vec1()?;
            result.push(values);
        }
        Ok(result)
    }

    /// Get the embedding dimension.
    pub fn dim(&self) -> usize {
        config::EMBEDDING_DIM
    }
}

/// Load safetensors with "model." prefix added to tensor names.
/// Uses mmap to leverage OS page cache for faster repeated loads.
fn load_tensors_with_prefix(path: &Path, device: &Device) -> Result<HashMap<String, Tensor>> {
    let mmaped = unsafe { candle_core::safetensors::MmapedSafetensors::new(path)? };
    let mut renamed = HashMap::new();
    for (name, _view) in mmaped.tensors() {
        let tensor = mmaped.load(&name, device)?;
        renamed.insert(format!("model.{name}"), tensor);
    }
    Ok(renamed)
}

/// Mean pooling with L2 normalization.
fn mean_pooling(output: &Tensor, attention_mask: &Tensor) -> Result<Tensor> {
    let mask = attention_mask.unsqueeze(D::Minus1)?.to_dtype(DType::F32)?;
    let masked = output.broadcast_mul(&mask)?;
    let sum = masked.sum(1)?;
    let count = attention_mask
        .sum_keepdim(1)?
        .to_dtype(DType::F32)?
        .clamp(1e-9, f64::MAX)?;
    let pooled = sum.broadcast_div(&count)?;

    let norms = pooled
        .sqr()?
        .sum_keepdim(D::Minus1)?
        .sqrt()?
        .clamp(1e-9, f64::MAX)?;
    Ok(pooled.broadcast_div(&norms)?)
}

// ─── Socket protocol ───────────────────────────────────────────────

/// Read a length-prefixed message from a stream.
pub fn read_message(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let msg_len = u32::from_be_bytes(len_buf) as usize;

    let mut buf = vec![0u8; msg_len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

/// Write a length-prefixed message to a stream.
pub fn write_message(stream: &mut impl Write, data: &[u8]) -> Result<()> {
    let len_bytes = (data.len() as u32).to_be_bytes();
    stream.write_all(&len_bytes)?;
    stream.write_all(data)?;
    stream.flush()?;
    Ok(())
}

// ─── Daemon ────────────────────────────────────────────────────────

/// Run the embedder daemon on a UNIX socket.
pub fn run_daemon(socket_path: &Path) -> Result<()> {
    // Clean up stale socket
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }

    eprintln!("Loading model...");
    let embedder = Embedder::load(&Device::Cpu)?;
    eprintln!("Model loaded.");

    // Backfill via worker subprocess in background (crash-isolated)
    {
        let db_path = crate::config::db_path();
        if db_path.exists() {
            std::thread::spawn(move || {
                if let Err(e) = crate::cli::backfill_with_worker(&db_path) {
                    eprintln!("Backfill warning: {e}");
                }
            });
        }
    }

    // Write embedder status
    crate::status::update(&crate::config::data_dir(), |s| {
        s.embedder = Some(crate::status::EmbedderStatus {
            started_at: chrono::Utc::now().to_rfc3339(),
            pid: std::process::id(),
        });
    });

    eprintln!("Listening on {}", socket_path.display());

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
        eprintln!("Idle timeout disabled.");
    }

    // Periodic backfill thread (skipped when interval is 0)
    let backfill_interval_secs = config::embedder_backfill_interval_secs();
    if backfill_interval_secs > 0 {
        let running = Arc::clone(&running);
        std::thread::spawn(move || {
            periodic_backfill(&running, backfill_interval_secs);
        });
    }

    while running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                *last_activity.lock().unwrap() = Instant::now();
                if let Err(e) = handle_client(stream, &embedder) {
                    eprintln!("Client error: {e}");
                }
                *last_activity.lock().unwrap() = Instant::now();
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                if running.load(Ordering::Relaxed) {
                    eprintln!("Accept error: {e}");
                }
            }
        }
    }

    eprintln!("Shutting down (idle timeout).");
    crate::status::update(&crate::config::data_dir(), |s| {
        s.embedder = None;
    });
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
            eprintln!("Idle timeout reached ({timeout_secs}s). Stopping.");
            running.store(false, Ordering::Relaxed);
            // Poke the listener to unblock accept
            let _ = UnixStream::connect(socket_path);
            break;
        }
    }
}

fn periodic_backfill(running: &AtomicBool, interval_secs: u64) {
    let interval = Duration::from_secs(interval_secs);
    let db_path = crate::config::db_path();

    // Wait one full interval before first check (startup backfill handles the initial run)
    let mut elapsed = Duration::ZERO;
    while elapsed < interval {
        std::thread::sleep(Duration::from_secs(10));
        if !running.load(Ordering::Relaxed) {
            return;
        }
        elapsed += Duration::from_secs(10);
    }

    loop {
        if !running.load(Ordering::Relaxed) {
            break;
        }

        if db_path.exists() {
            if let Ok(conn) = crate::db::get_connection(&db_path) {
                let chunks: i64 = conn
                    .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
                    .unwrap_or(0);
                let vecs: i64 = conn
                    .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
                    .unwrap_or(0);
                drop(conn);

                if chunks > vecs {
                    let missing = chunks - vecs;
                    eprintln!("Periodic backfill: {missing} vectors missing. Starting backfill.");
                    if let Err(e) = crate::cli::backfill_with_worker(&db_path) {
                        eprintln!("Periodic backfill warning: {e}");
                    }
                }
            }
        }

        // Sleep for the next interval (in 10s increments to check running flag)
        let mut elapsed = Duration::ZERO;
        while elapsed < interval {
            std::thread::sleep(Duration::from_secs(10));
            if !running.load(Ordering::Relaxed) {
                return;
            }
            elapsed += Duration::from_secs(10);
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
            eprintln!("Encode error: {e}");
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
            eprintln!("PANIC in encode: {msg}");
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

// ─── Client ────────────────────────────────────────────────────────

/// Send texts to the embedder daemon and get embeddings back.
/// Returns None if the daemon is not running or connection fails.
pub fn embed_via_socket(texts: &[String]) -> Option<Vec<Vec<f32>>> {
    embed_via_socket_at(Path::new(config::SOCKET_PATH), texts)
}

/// Send texts to the embedder daemon at a specific socket path.
pub fn embed_via_socket_at(socket_path: &Path, texts: &[String]) -> Option<Vec<Vec<f32>>> {
    if !socket_path.exists() {
        return None;
    }

    let mut stream = UnixStream::connect(socket_path).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;

    let request = serde_json::json!({ "texts": texts });
    let request_bytes = serde_json::to_vec(&request).ok()?;
    write_message(&mut stream, &request_bytes).ok()?;

    let response_data = read_message(&mut stream).ok()?;
    let response: serde_json::Value = serde_json::from_slice(&response_data).ok()?;

    let embeddings = response.get("embeddings")?.as_array()?;
    let result: Vec<Vec<f32>> = embeddings
        .iter()
        .filter_map(|row| {
            row.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect()
            })
        })
        .collect();

    Some(result)
}

// ─── Worker subprocess ──────────────────────────────────────────────

/// Handle to a backfill-worker child process.
/// The child loads the model once and processes encode requests via stdin/stdout pipes.
/// If the child segfaults, the parent survives and can spawn a new worker.
pub struct WorkerHandle {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl WorkerHandle {
    /// Spawn a new `tsm backfill-worker` child process.
    /// Blocks until the child reports "READY" on stderr (with timeout).
    pub fn spawn(timeout: Duration) -> Result<Self> {
        let exe = std::env::current_exe().context("cannot determine executable path")?;
        let mut child = Command::new(exe)
            .arg("backfill-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn backfill-worker")?;

        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("no stdout")?);
        let mut stderr = BufReader::new(child.stderr.take().context("no stderr")?);

        // Wait for READY signal on stderr
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match stderr.read_line(&mut line) {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        eprint!("[worker] {line}");
                        if line.trim() == "READY" {
                            let _ = tx.send(true);
                            // Continue forwarding stderr
                            loop {
                                line.clear();
                                match stderr.read_line(&mut line) {
                                    Ok(0) => break,
                                    Ok(_) => eprint!("[worker] {line}"),
                                    Err(_) => break,
                                }
                            }
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        rx.recv_timeout(timeout).map_err(|_| {
            anyhow::anyhow!("backfill-worker did not become ready within {timeout:?}")
        })?;

        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    /// Send texts and receive embeddings via the pipe protocol.
    /// Returns Err on timeout or if the child has died.
    pub fn encode(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let request = serde_json::json!({ "texts": texts });
        let request_bytes = serde_json::to_vec(&request)?;
        write_message(&mut self.stdin, &request_bytes)?;

        // Read response — blocking read on pipe.
        // If the child crashes, read_message returns Err (broken pipe).
        let response_data = read_message(&mut self.stdout)
            .context("worker pipe read error (child may have crashed)")?;

        let response: serde_json::Value = serde_json::from_slice(&response_data)?;

        if let Some(err) = response.get("error").and_then(|e| e.as_str()) {
            anyhow::bail!("worker encode error: {err}");
        }

        let embeddings = response
            .get("embeddings")
            .and_then(|v| v.as_array())
            .context("missing embeddings in worker response")?;

        let result: Vec<Vec<f32>> = embeddings
            .iter()
            .filter_map(|row| {
                row.as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_f64().map(|f| f as f32))
                        .collect()
                })
            })
            .collect();

        Ok(result)
    }

    /// Check if the child process is still alive.
    pub fn is_alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    /// Kill the child process and wait.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Entry point for the `tsm backfill-worker` subprocess.
/// Loads the model, signals READY, then processes encode requests on stdin/stdout.
pub fn run_backfill_worker() -> Result<()> {
    eprintln!("Loading model...");
    let embedder = Embedder::load(&Device::Cpu)?;
    eprintln!("Model loaded.");
    eprintln!("READY");

    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();

    loop {
        let request_data = match read_message(&mut stdin) {
            Ok(data) => data,
            Err(_) => break, // stdin closed — parent is done
        };

        let request: serde_json::Value = match serde_json::from_slice(&request_data) {
            Ok(v) => v,
            Err(e) => {
                let err_resp = serde_json::json!({ "error": format!("invalid request: {e}") });
                let err_bytes = serde_json::to_vec(&err_resp)?;
                write_message(&mut stdout, &err_bytes)?;
                continue;
            }
        };

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

        let response = match encode_result {
            Ok(Ok(emb)) => serde_json::json!({ "embeddings": emb }),
            Ok(Err(e)) => {
                eprintln!("Encode error: {e}");
                serde_json::json!({ "error": format!("{e}") })
            }
            Err(panic_info) => {
                let msg = if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(&s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "unknown panic".to_string()
                };
                eprintln!("PANIC in encode: {msg}");
                serde_json::json!({ "error": format!("panic: {msg}") })
            }
        };

        let response_bytes = serde_json::to_vec(&response)?;
        write_message(&mut stdout, &response_bytes)?;
    }

    eprintln!("Worker exiting.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_write_message_roundtrip() {
        let original = b"hello world";
        let mut buf = Vec::new();
        write_message(&mut buf, original).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_read_write_message_empty() {
        let original = b"";
        let mut buf = Vec::new();
        write_message(&mut buf, original).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).unwrap();
        assert_eq!(decoded, original.to_vec());
    }

    #[test]
    fn test_read_write_message_large() {
        let original: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
        let mut buf = Vec::new();
        write_message(&mut buf, &original).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_message_format_is_big_endian_length_prefix() {
        let data = b"test";
        let mut buf = Vec::new();
        write_message(&mut buf, data).unwrap();

        assert_eq!(&buf[0..4], &[0, 0, 0, 4]); // 4 bytes, big-endian
        assert_eq!(&buf[4..], b"test");
    }

    #[test]
    fn test_embed_via_socket_nonexistent_path() {
        let result = embed_via_socket_at(Path::new("/tmp/nonexistent.sock"), &[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_embed_via_socket_integration() {
        // Start a mock server that echoes fixed embeddings
        let dir = tempfile::TempDir::new().unwrap();
        let sock_path = dir.path().join("test.sock");

        let sock_path_clone = sock_path.clone();
        let server = std::thread::spawn(move || {
            let listener = UnixListener::bind(&sock_path_clone).unwrap();
            let (mut stream, _) = listener.accept().unwrap();

            // Read request
            let req_data = read_message(&mut stream).unwrap();
            let req: serde_json::Value = serde_json::from_slice(&req_data).unwrap();
            let n = req["texts"].as_array().unwrap().len();

            // Send back fake embeddings (3-dim for simplicity)
            let embeddings: Vec<Vec<f32>> = (0..n).map(|_| vec![0.1, 0.2, 0.3]).collect();
            let response = serde_json::json!({ "embeddings": embeddings });
            let resp_bytes = serde_json::to_vec(&response).unwrap();
            write_message(&mut stream, &resp_bytes).unwrap();
        });

        // Wait for server to be ready
        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let texts = vec!["hello".to_string(), "world".to_string()];
        let result = embed_via_socket_at(&sock_path, &texts);
        assert!(result.is_some());
        let embeddings = result.unwrap();
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0], vec![0.1, 0.2, 0.3]);

        server.join().unwrap();
    }
}
