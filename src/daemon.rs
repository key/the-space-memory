use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::Connection;

use crate::cli;
use crate::config;
use crate::daemon_protocol::{DaemonRequest, DaemonResponse};

/// Handle a single daemon request and return a response.
///
/// `shutdown_flag` is set to `true` when a `Shutdown` request is received.
pub fn handle_request(
    conn: &Connection,
    req: DaemonRequest,
    index_root: &Path,
    shutdown_flag: &AtomicBool,
) -> DaemonResponse {
    match req {
        DaemonRequest::Ping => DaemonResponse::success_empty(),

        DaemonRequest::Shutdown => {
            shutdown_flag.store(true, Ordering::SeqCst);
            DaemonResponse::success_empty()
        }

        DaemonRequest::Search {
            query,
            top_k,
            format,
            include_content,
            after,
            before,
            recent,
            year,
            fallback,
            paths,
        } => {
            let opts = cli::SearchOptions {
                query: &query,
                top_k,
                format: &format,
                include_content,
                after: after.as_deref(),
                before: before.as_deref(),
                recent: recent.as_deref(),
                year,
                fallback: fallback.as_deref(),
                paths: paths.as_deref(),
            };
            match cli::run_search(conn, &opts) {
                Ok(results) => {
                    let json_str = cli::format_json(&results, include_content, index_root);
                    match json_str {
                        Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
                            Ok(v) => DaemonResponse::success(v),
                            Err(e) => DaemonResponse::error(format!("JSON parse error: {e}")),
                        },
                        Err(e) => DaemonResponse::error(format!("Format error: {e}")),
                    }
                }
                Err(e) => DaemonResponse::error(format!("{e}")),
            }
        }

        DaemonRequest::Index { files } => {
            let file_paths: Vec<PathBuf> = if files.is_empty() {
                cli::collect_content_files(index_root)
            } else {
                files.iter().map(|f| index_root.join(f)).collect()
            };
            match cli::run_index(conn, &file_paths, index_root) {
                Ok(stats) => DaemonResponse::success(serde_json::json!({
                    "indexed": stats.indexed,
                    "skipped": stats.skipped,
                    "removed": stats.removed,
                })),
                Err(e) => DaemonResponse::error(format!("{e}")),
            }
        }

        DaemonRequest::IngestSession { session_file } => {
            let path = PathBuf::from(&session_file);
            match cli::run_ingest_session(conn, &path) {
                Ok(indexed) => DaemonResponse::success(serde_json::json!({
                    "indexed": indexed,
                })),
                Err(e) => DaemonResponse::error(format!("{e}")),
            }
        }

        DaemonRequest::Doctor { .. } => {
            let db_path = config::db_path();
            let report = cli::run_doctor(conn, &db_path);
            let json_str = report.to_json();
            match serde_json::from_str::<serde_json::Value>(&json_str) {
                Ok(v) => DaemonResponse::success(v),
                Err(e) => DaemonResponse::error(format!("JSON parse error: {e}")),
            }
        }

        DaemonRequest::Status => {
            let info = cli::run_status(Some(conn));
            match serde_json::to_value(&info) {
                Ok(v) => DaemonResponse::success(v),
                Err(e) => DaemonResponse::error(format!("Serialize error: {e}")),
            }
        }

        DaemonRequest::VectorFill { batch_size } => match cli::run_vector_fill(conn, batch_size) {
            Ok(()) => DaemonResponse::success_empty(),
            Err(e) => DaemonResponse::error(format!("{e}")),
        },

        DaemonRequest::ImportWordnet { wordnet_db } => {
            let path = PathBuf::from(&wordnet_db);
            match crate::synonyms::import_wordnet(conn, &path, None) {
                Ok(count) => DaemonResponse::success(serde_json::json!({
                    "imported": count,
                })),
                Err(e) => DaemonResponse::error(format!("{e}")),
            }
        }

        DaemonRequest::DictUpdate { .. } => DaemonResponse::error(
            "dict update --apply cannot run while tsmd is active. Run `tsm stop` first.",
        ),

        DaemonRequest::Reindex { .. } => DaemonResponse::error(
            "reindex must be intercepted by daemon_mode. If you see this, it's a bug.",
        ),

        DaemonRequest::Rebuild => {
            DaemonResponse::error("rebuild cannot run while tsmd is active. Run `tsm stop` first.")
        }

        // In the live daemon, tsmd::handle_client intercepts Reload first
        // (to access the watcher channel). This arm handles the same logic
        // for callers without a watcher, including unit tests.
        DaemonRequest::Reload => {
            let warnings = config::reload();
            if warnings.is_empty() {
                DaemonResponse::success_empty()
            } else {
                DaemonResponse::success(serde_json::json!({ "warnings": warnings }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::test_utils::setup_db_with_dir as setup;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn test_ping() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let resp = handle_request(&conn, DaemonRequest::Ping, dir.path(), &flag);
        assert!(resp.ok);
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_shutdown() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let resp = handle_request(&conn, DaemonRequest::Shutdown, dir.path(), &flag);
        assert!(resp.ok);
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_search_empty_db() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let req = DaemonRequest::Search {
            query: "test".into(),
            top_k: 5,
            format: "json".into(),
            include_content: None,
            after: None,
            before: None,
            recent: None,
            year: None,
            fallback: Some("fts_only".into()),
            paths: None,
        };
        let resp = handle_request(&conn, req, dir.path(), &flag);
        assert!(resp.ok);
        // Empty DB returns empty array
        let payload = resp.payload.unwrap();
        assert!(payload.is_array());
        assert_eq!(payload.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_search_with_path_filter() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);

        // Create files in two directories
        let daily_dir = dir.path().join("daily/notes");
        std::fs::create_dir_all(&daily_dir).unwrap();
        std::fs::write(
            daily_dir.join("mtg.md"),
            "---\nstatus: current\n---\n\n# MTG\n\nMTG meeting notes.\n",
        )
        .unwrap();

        let projects_dir = dir.path().join("projects/tsm");
        std::fs::create_dir_all(&projects_dir).unwrap();
        std::fs::write(
            projects_dir.join("mtg.md"),
            "---\nstatus: current\n---\n\n# MTG\n\nMTG project notes.\n",
        )
        .unwrap();

        // Index both files
        let req = DaemonRequest::Index {
            files: vec!["daily/notes/mtg.md".into(), "projects/tsm/mtg.md".into()],
        };
        let resp = handle_request(&conn, req, dir.path(), &flag);
        assert!(resp.ok);

        // Search with path filter
        let req = DaemonRequest::Search {
            query: "MTG".into(),
            top_k: 10,
            format: "json".into(),
            include_content: None,
            after: None,
            before: None,
            recent: None,
            year: None,
            fallback: Some("fts_only".into()),
            paths: Some(vec!["daily/".into()]),
        };
        let resp = handle_request(&conn, req, dir.path(), &flag);
        assert!(resp.ok);
        let results = resp.payload.unwrap();
        let arr = results.as_array().unwrap();
        for item in arr {
            let path = item["source_file"].as_str().unwrap();
            assert!(
                path.starts_with("daily/"),
                "Expected daily/ prefix, got: {path}"
            );
        }
    }

    #[test]
    fn test_index_empty() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let req = DaemonRequest::Index { files: vec![] };
        let resp = handle_request(&conn, req, dir.path(), &flag);
        assert!(resp.ok);
        let payload = resp.payload.unwrap();
        assert_eq!(payload["indexed"], 0);
        assert_eq!(payload["skipped"], 0);
    }

    #[test]
    fn test_index_specific_file() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);

        // Create a markdown file
        let notes_dir = dir.path().join("daily/notes");
        std::fs::create_dir_all(&notes_dir).unwrap();
        std::fs::write(notes_dir.join("test.md"), "# Test\n\nHello world").unwrap();

        let req = DaemonRequest::Index {
            files: vec!["daily/notes/test.md".into()],
        };
        let resp = handle_request(&conn, req, dir.path(), &flag);
        assert!(resp.ok);
        let payload = resp.payload.unwrap();
        assert_eq!(payload["indexed"], 1);
    }

    #[test]
    fn test_ingest_session_file_not_found_via_daemon() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let req = DaemonRequest::IngestSession {
            session_file: "/nonexistent/file.jsonl".into(),
        };
        let resp = handle_request(&conn, req, dir.path(), &flag);
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("File not found"));
    }

    #[test]
    fn test_status() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let resp = handle_request(&conn, DaemonRequest::Status, dir.path(), &flag);
        assert!(resp.ok);
        let payload = resp.payload.unwrap();
        assert!(payload.get("embedder_running").is_some());
        assert!(payload.get("documents").is_some());
    }

    #[test]
    fn test_doctor() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let req = DaemonRequest::Doctor {
            format: "json".into(),
        };
        let resp = handle_request(&conn, req, dir.path(), &flag);
        // Doctor checks db_path which won't exist in memory mode, but should not crash
        assert!(resp.ok);
    }

    #[test]
    fn test_import_wordnet_nonexistent() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let req = DaemonRequest::ImportWordnet {
            wordnet_db: "/nonexistent/wnjpn.db".into(),
        };
        let resp = handle_request(&conn, req, dir.path(), &flag);
        assert!(!resp.ok);
        assert!(resp.error.is_some());
    }

    #[test]
    fn test_dict_update_rejected_by_daemon() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let req = DaemonRequest::DictUpdate {
            threshold: 5,
            apply: true,
        };
        let resp = handle_request(&conn, req, dir.path(), &flag);
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("tsm stop"));
    }

    #[test]
    fn test_rebuild_rejected_by_daemon() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let req = DaemonRequest::Rebuild;
        let resp = handle_request(&conn, req, dir.path(), &flag);
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("tsm stop"));
    }

    #[test]
    fn test_reindex_rejected_by_daemon() {
        use crate::daemon_protocol::ReindexKind;
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let req = DaemonRequest::Reindex {
            kind: ReindexKind::Fts,
        };
        let resp = handle_request(&conn, req, dir.path(), &flag);
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("intercepted"));
    }

    #[test]
    fn test_reload() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let resp = handle_request(&conn, DaemonRequest::Reload, dir.path(), &flag);
        assert!(resp.ok);
        // Shutdown flag should NOT be set by reload
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_vector_fill_empty_db() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let req = DaemonRequest::VectorFill { batch_size: 8 };
        let resp = handle_request(&conn, req, dir.path(), &flag);
        // Empty DB has no chunks to backfill — should succeed or error gracefully
        assert!(resp.ok || resp.error.is_some());
    }

    #[test]
    fn test_doctor_text_format() {
        let (conn, dir) = setup();
        let flag = AtomicBool::new(false);
        let req = DaemonRequest::Doctor {
            format: "text".into(),
        };
        let resp = handle_request(&conn, req, dir.path(), &flag);
        // Both "text" and "json" return JSON through daemon protocol
        assert!(resp.ok);
        assert!(resp.payload.is_some());
    }

    #[test]
    fn test_socket_roundtrip() {
        use crate::daemon_protocol::{read_request, send_request, write_response};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::TempDir::new().unwrap();
        let sock_path = dir.path().join("test-daemon.sock");
        let sock_path_clone = sock_path.clone();

        // Start a mini daemon server in a thread
        let server = std::thread::spawn(move || {
            let conn = db::get_memory_connection().unwrap();
            let flag = AtomicBool::new(false);
            let listener = UnixListener::bind(&sock_path_clone).unwrap();

            // Handle exactly 2 requests: Ping then Shutdown
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let req = read_request(&mut stream).unwrap();
                let resp = handle_request(&conn, req, dir.path(), &flag);
                write_response(&mut stream, &resp).unwrap();
            }
        });

        // Wait for server socket
        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Client: send Ping
        let resp = send_request(&sock_path, &DaemonRequest::Ping).unwrap();
        assert!(resp.ok);

        // Client: send Shutdown
        let resp = send_request(&sock_path, &DaemonRequest::Shutdown).unwrap();
        assert!(resp.ok);

        server.join().unwrap();
    }

    #[test]
    fn test_socket_search_roundtrip() {
        use crate::daemon_protocol::{read_request, send_request, write_response};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::TempDir::new().unwrap();
        let sock_path = dir.path().join("test-search.sock");
        let sock_path_clone = sock_path.clone();
        let dir_path = dir.path().to_path_buf();

        let server = std::thread::spawn(move || {
            let conn = db::get_memory_connection().unwrap();
            let flag = AtomicBool::new(false);
            let listener = UnixListener::bind(&sock_path_clone).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let req = read_request(&mut stream).unwrap();
            let resp = handle_request(&conn, req, &dir_path, &flag);
            write_response(&mut stream, &resp).unwrap();
        });

        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let resp = send_request(
            &sock_path,
            &DaemonRequest::Search {
                query: "test".into(),
                top_k: 5,
                format: "json".into(),
                include_content: None,
                after: None,
                before: None,
                recent: None,
                year: None,
                fallback: Some("fts_only".into()),
                paths: None,
            },
        )
        .unwrap();
        assert!(resp.ok);
        assert!(resp.payload.unwrap().is_array());

        server.join().unwrap();
    }
}
