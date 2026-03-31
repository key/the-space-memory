use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ipc::{read_message, write_message};

/// Request from tsm CLI to tsmd daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum DaemonRequest {
    Search {
        query: String,
        top_k: usize,
        format: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        include_content: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        after: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        recent: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        year: Option<i32>,
    },
    Index {
        files: Vec<String>,
    },
    IngestSession {
        session_file: String,
    },
    Doctor {
        format: String,
    },
    Status,
    VectorFill {
        batch_size: usize,
    },
    DictUpdate {
        threshold: i64,
        yes: bool,
        format: String,
    },
    ImportWordnet {
        wordnet_db: String,
    },
    Rebuild {
        force: bool,
    },
    Shutdown,
    Ping,
}

/// Response from tsmd daemon to tsm CLI.
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

impl DaemonResponse {
    pub fn success(payload: serde_json::Value) -> Self {
        Self {
            ok: true,
            error: None,
            payload: Some(payload),
        }
    }

    pub fn success_empty() -> Self {
        Self {
            ok: true,
            error: None,
            payload: None,
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            payload: None,
        }
    }
}

// ─── Client helpers ───────────────────────────────────────────────

/// Send a request to the daemon and wait for a response.
pub fn send_request(socket: &Path, req: &DaemonRequest) -> Result<DaemonResponse> {
    let mut stream =
        UnixStream::connect(socket).context("Failed to connect to tsmd. Is it running?")?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(300)))?;

    let req_bytes = serde_json::to_vec(req)?;
    write_message(&mut stream, &req_bytes)?;

    let resp_bytes = read_message(&mut stream)?;
    let resp: DaemonResponse = serde_json::from_slice(&resp_bytes)?;
    Ok(resp)
}

/// Try to send a request to the daemon, with transparent fallback.
///
/// Returns `None` if the daemon is not running (socket absent or connection refused).
/// Returns `Some(Ok(resp))` on successful communication.
/// Returns `Some(Err(e))` on protocol-level errors.
pub fn try_send_request(socket: &Path, req: &DaemonRequest) -> Option<Result<DaemonResponse>> {
    if !socket.exists() {
        return None;
    }
    let mut stream = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(_) => return None, // stale socket or connection refused
    };
    if let Err(e) = stream.set_read_timeout(Some(std::time::Duration::from_secs(300))) {
        return Some(Err(anyhow::anyhow!("Failed to set socket read timeout: {e}")));
    }
    let req_bytes = match serde_json::to_vec(req) {
        Ok(b) => b,
        Err(e) => return Some(Err(e.into())),
    };
    if let Err(e) = write_message(&mut stream, &req_bytes) {
        return Some(Err(e));
    }
    match read_message(&mut stream) {
        Ok(resp_bytes) => Some(serde_json::from_slice(&resp_bytes).map_err(Into::into)),
        Err(e) => Some(Err(e)),
    }
}

// ─── Server helpers ───────────────────────────────────────────────

/// Read a request from a connected client stream.
pub fn read_request(stream: &mut UnixStream) -> Result<DaemonRequest> {
    let data = read_message(stream)?;
    let req: DaemonRequest = serde_json::from_slice(&data)?;
    Ok(req)
}

/// Write a response to a connected client stream.
pub fn write_response(stream: &mut UnixStream, resp: &DaemonResponse) -> Result<()> {
    let data = serde_json::to_vec(resp)?;
    write_message(stream, &data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_search() {
        let req = DaemonRequest::Search {
            query: "テスト".into(),
            top_k: 5,
            format: "json".into(),
            include_content: Some(3),
            after: None,
            before: Some("2026-01-01".into()),
            recent: None,
            year: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        match decoded {
            DaemonRequest::Search {
                query,
                top_k,
                include_content,
                before,
                ..
            } => {
                assert_eq!(query, "テスト");
                assert_eq!(top_k, 5);
                assert_eq!(include_content, Some(3));
                assert_eq!(before, Some("2026-01-01".into()));
            }
            _ => panic!("Expected Search variant"),
        }
    }

    #[test]
    fn serde_roundtrip_index() {
        let req = DaemonRequest::Index {
            files: vec!["daily/notes/test.md".into()],
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        match decoded {
            DaemonRequest::Index { files } => {
                assert_eq!(files, vec!["daily/notes/test.md"]);
            }
            _ => panic!("Expected Index variant"),
        }
    }

    #[test]
    fn serde_roundtrip_ingest_session() {
        let req = DaemonRequest::IngestSession {
            session_file: "/tmp/session.jsonl".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        match decoded {
            DaemonRequest::IngestSession { session_file } => {
                assert_eq!(session_file, "/tmp/session.jsonl");
            }
            _ => panic!("Expected IngestSession variant"),
        }
    }

    #[test]
    fn serde_roundtrip_doctor() {
        let req = DaemonRequest::Doctor {
            format: "json".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        match decoded {
            DaemonRequest::Doctor { format } => assert_eq!(format, "json"),
            _ => panic!("Expected Doctor variant"),
        }
    }

    #[test]
    fn serde_roundtrip_status() {
        let req = DaemonRequest::Status;
        let json = serde_json::to_string(&req).unwrap();
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, DaemonRequest::Status));
    }

    #[test]
    fn serde_roundtrip_vector_fill() {
        let req = DaemonRequest::VectorFill { batch_size: 64 };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        match decoded {
            DaemonRequest::VectorFill { batch_size } => assert_eq!(batch_size, 64),
            _ => panic!("Expected VectorFill variant"),
        }
    }

    #[test]
    fn serde_roundtrip_dict_update() {
        let req = DaemonRequest::DictUpdate {
            threshold: 10,
            yes: true,
            format: "ipadic".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        match decoded {
            DaemonRequest::DictUpdate {
                threshold,
                yes,
                format,
            } => {
                assert_eq!(threshold, 10);
                assert!(yes);
                assert_eq!(format, "ipadic");
            }
            _ => panic!("Expected DictUpdate variant"),
        }
    }

    #[test]
    fn serde_roundtrip_import_wordnet() {
        let req = DaemonRequest::ImportWordnet {
            wordnet_db: "/path/to/wnjpn.db".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        match decoded {
            DaemonRequest::ImportWordnet { wordnet_db } => {
                assert_eq!(wordnet_db, "/path/to/wnjpn.db");
            }
            _ => panic!("Expected ImportWordnet variant"),
        }
    }

    #[test]
    fn serde_roundtrip_rebuild() {
        let req = DaemonRequest::Rebuild { force: true };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        match decoded {
            DaemonRequest::Rebuild { force } => assert!(force),
            _ => panic!("Expected Rebuild variant"),
        }
    }

    #[test]
    fn serde_roundtrip_shutdown() {
        let req = DaemonRequest::Shutdown;
        let json = serde_json::to_string(&req).unwrap();
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, DaemonRequest::Shutdown));
    }

    #[test]
    fn serde_roundtrip_ping() {
        let req = DaemonRequest::Ping;
        let json = serde_json::to_string(&req).unwrap();
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, DaemonRequest::Ping));
    }

    #[test]
    fn response_success() {
        let resp = DaemonResponse::success(serde_json::json!({"count": 42}));
        assert!(resp.ok);
        assert!(resp.error.is_none());
        assert_eq!(resp.payload.unwrap()["count"], 42);
    }

    #[test]
    fn response_success_empty() {
        let resp = DaemonResponse::success_empty();
        assert!(resp.ok);
        assert!(resp.error.is_none());
        assert!(resp.payload.is_none());

        // Verify JSON has no null fields
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("error"));
        assert!(!json.contains("payload"));
    }

    #[test]
    fn response_error() {
        let resp = DaemonResponse::error("something went wrong");
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap(), "something went wrong");
        assert!(resp.payload.is_none());
    }

    #[test]
    fn response_serde_roundtrip() {
        let resp = DaemonResponse::success(serde_json::json!({"results": []}));
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert!(decoded.ok);
        assert_eq!(decoded.payload.unwrap()["results"], serde_json::json!([]));
    }

    #[test]
    fn request_wire_roundtrip() {
        let req = DaemonRequest::Search {
            query: "テスト".into(),
            top_k: 5,
            format: "text".into(),
            include_content: None,
            after: None,
            before: None,
            recent: None,
            year: None,
        };
        let req_bytes = serde_json::to_vec(&req).unwrap();
        let mut buf = Vec::new();
        write_message(&mut buf, &req_bytes).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded_bytes = read_message(&mut cursor).unwrap();
        let decoded: DaemonRequest = serde_json::from_slice(&decoded_bytes).unwrap();
        match decoded {
            DaemonRequest::Search { query, top_k, .. } => {
                assert_eq!(query, "テスト");
                assert_eq!(top_k, 5);
            }
            _ => panic!("Expected Search"),
        }
    }

    #[test]
    fn response_wire_roundtrip() {
        let resp = DaemonResponse::success(serde_json::json!({"status": "ok"}));
        let resp_bytes = serde_json::to_vec(&resp).unwrap();
        let mut buf = Vec::new();
        write_message(&mut buf, &resp_bytes).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded_bytes = read_message(&mut cursor).unwrap();
        let decoded: DaemonResponse = serde_json::from_slice(&decoded_bytes).unwrap();
        assert!(decoded.ok);
        assert_eq!(decoded.payload.unwrap()["status"], "ok");
    }

    #[test]
    fn invalid_json_fails_to_deserialize() {
        // Valid IPC frame but invalid JSON content
        let garbage = b"not json at all";
        let mut buf = Vec::new();
        write_message(&mut buf, garbage).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded_bytes = read_message(&mut cursor).unwrap();
        let result = serde_json::from_slice::<DaemonRequest>(&decoded_bytes);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_cmd_fails_to_deserialize() {
        let unknown = serde_json::json!({"cmd": "UnknownCommand", "foo": "bar"});
        let result = serde_json::from_value::<DaemonRequest>(unknown);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_json_via_socket() {
        use std::os::unix::net::{UnixListener, UnixStream};

        let dir = tempfile::TempDir::new().unwrap();
        let sock_path = dir.path().join("test-invalid.sock");
        let sock_path_clone = sock_path.clone();

        let server = std::thread::spawn(move || {
            let listener = UnixListener::bind(&sock_path_clone).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            // read_request should fail on invalid JSON
            let result = read_request(&mut stream);
            assert!(result.is_err());
        });

        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Client sends garbage through the socket
        let mut client = UnixStream::connect(&sock_path).unwrap();
        write_message(&mut client, b"not valid json").unwrap();
        drop(client);

        server.join().unwrap();
    }

    #[test]
    fn try_send_request_success() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::TempDir::new().unwrap();
        let sock_path = dir.path().join("test-try.sock");
        let sock_path_clone = sock_path.clone();

        let server = std::thread::spawn(move || {
            let listener = UnixListener::bind(&sock_path_clone).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let req = read_request(&mut stream).unwrap();
            assert!(matches!(req, DaemonRequest::Ping));
            let resp = DaemonResponse::success_empty();
            write_response(&mut stream, &resp).unwrap();
        });

        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let result = try_send_request(&sock_path, &DaemonRequest::Ping);
        assert!(result.is_some());
        let resp = result.unwrap().unwrap();
        assert!(resp.ok);

        server.join().unwrap();
    }

    #[test]
    fn try_send_request_no_socket_returns_none() {
        let result = try_send_request(
            std::path::Path::new("/tmp/nonexistent-tsm-test.sock"),
            &DaemonRequest::Ping,
        );
        assert!(result.is_none());
    }

    #[test]
    fn try_send_request_stale_socket_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock_path = dir.path().join("stale.sock");
        // Create a file (not a socket listener) to simulate a stale socket
        std::fs::write(&sock_path, "stale").unwrap();
        let result = try_send_request(&sock_path, &DaemonRequest::Ping);
        assert!(result.is_none());
    }
}
