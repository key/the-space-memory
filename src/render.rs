//! Render daemon responses into terminal output.
//!
//! Extracted from `main.rs` so the formatting logic sits under the coverage
//! gate while the binary entry point stays a thin shell. Each renderer writes
//! into a `&mut impl Write` sink (`main` passes stdout; tests pass a buffer),
//! making the rendered bytes unit-testable.
//!
//! Status and doctor text rendering still delegates to `cli::print_status_info`
//! / `cli::render_doctor_report` (stdout), which other work will later move onto
//! a writer; everything those don't cover (response validation, JSON, parse
//! errors) is writer-based here.

use std::io::Write;

use anyhow::Result;

use crate::cli;
use crate::daemon_protocol::DaemonResponse;

/// Bail with the daemon's error message when a response is not `ok`.
fn check_resp(resp: &DaemonResponse) -> Result<()> {
    if !resp.ok {
        anyhow::bail!(
            "{}",
            resp.error
                .clone()
                .unwrap_or_else(|| "(daemon returned error with no message)".into())
        );
    }
    Ok(())
}

/// Write a JSON value as pretty-printed text followed by a newline.
fn write_json(w: &mut impl Write, value: &serde_json::Value) -> Result<()> {
    writeln!(
        w,
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    )?;
    Ok(())
}

/// Render a search response: pretty JSON for `format == "json"`, otherwise the
/// CWD-relative text view via `searcher::format_text`.
pub fn render_search(w: &mut impl Write, resp: DaemonResponse, format: &str) -> Result<()> {
    check_resp(&resp)?;
    let payload = resp.payload.unwrap_or_default();
    match format {
        "json" => write_json(w, &payload)?,
        _ => {
            let total_hits = payload["total_hits"].as_u64().unwrap_or(0) as usize;
            let results: Vec<crate::searcher::SearchResult> =
                serde_json::from_value(payload["results"].clone())
                    .map_err(|e| anyhow::anyhow!("Failed to parse search results: {e}"))?;
            // Text output is relative to the caller's CWD.
            let cwd = std::env::current_dir().unwrap_or_default();
            write!(
                w,
                "{}",
                crate::searcher::format_text(&results, total_hits, &cwd)
            )?;
        }
    }
    Ok(())
}

/// Render an index response: `indexed/skipped/removed` counts.
pub fn render_index(w: &mut impl Write, resp: DaemonResponse) -> Result<()> {
    check_resp(&resp)?;
    if let Some(payload) = resp.payload {
        let indexed = payload["indexed"].as_i64().unwrap_or(0);
        let skipped = payload["skipped"].as_i64().unwrap_or(0);
        let removed = payload["removed"].as_i64().unwrap_or(0);
        writeln!(
            w,
            "indexed: {indexed}, skipped: {skipped}, removed: {removed}"
        )?;
    }
    Ok(())
}

/// Render a session-ingest response: indexed vs unchanged, by file name.
pub fn render_ingest(
    w: &mut impl Write,
    resp: DaemonResponse,
    session_file: &std::path::Path,
) -> Result<()> {
    check_resp(&resp)?;
    let name = session_file
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    if let Some(payload) = resp.payload {
        if payload["indexed"].as_bool().unwrap_or(false) {
            writeln!(w, "session indexed: {name}")?;
        } else {
            writeln!(w, "session unchanged: {name}")?;
        }
    }
    Ok(())
}

/// Render a status response. Validation and parsing are handled here; the text
/// view delegates to `cli::print_status_info` (stdout), so this renderer takes
/// no writer (the lone exception among the renderers).
pub fn render_status(resp: DaemonResponse) -> Result<()> {
    check_resp(&resp)?;
    if let Some(payload) = resp.payload {
        let info: cli::StatusInfo = serde_json::from_value(payload).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse daemon status: {e}\nTry `tsm stop && tsm start` to refresh."
            )
        })?;
        cli::print_status_info(&info);
    }
    Ok(())
}

/// Render a doctor report: pretty JSON for `format == "json"`, otherwise the
/// text report via `cli::render_doctor_report`.
///
/// Both branches write to stdout (JSON directly; text delegated to the cli
/// printer), so — like [`render_status`] — this renderer takes no writer rather
/// than accepting one it would ignore on the text path. Unifying the cli text
/// printers onto a writer is tracked separately.
pub fn render_doctor(resp: DaemonResponse, format: &str) -> Result<()> {
    check_resp(&resp)?;
    let payload = resp.payload.unwrap_or_default();
    if format == "json" {
        write_json(&mut std::io::stdout(), &payload)?;
        return Ok(());
    }
    let report: cli::DoctorReport = serde_json::from_value(payload).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse daemon doctor report: {e}\nTry `tsm stop && tsm start` to refresh."
        )
    })?;
    cli::render_doctor_report(&report);
    Ok(())
}

/// Render a reload response: surface any warnings on `err`, then confirm on
/// `out`. `main` passes stdout/stderr; tests pass buffers so both streams are
/// assertable.
pub fn render_reload(
    out: &mut impl Write,
    err: &mut impl Write,
    resp: DaemonResponse,
) -> Result<()> {
    check_resp(&resp)?;
    if let Some(payload) = &resp.payload {
        if let Some(warnings) = payload.get("warnings").and_then(|v| v.as_array()) {
            for warning in warnings {
                if let Some(s) = warning.as_str() {
                    writeln!(err, "warning: {s}")?;
                }
            }
        }
    }
    writeln!(out, "config reloaded")?;
    Ok(())
}

/// Render a reindex response: a fixed "started" confirmation.
pub fn render_reindex(w: &mut impl Write, resp: DaemonResponse) -> Result<()> {
    check_resp(&resp)?;
    writeln!(w, "reindex started. Run `tsm doctor` to check progress.")?;
    Ok(())
}

/// Render a WordNet-import response: the count of imported synonym pairs.
pub fn render_import_wordnet(w: &mut impl Write, resp: DaemonResponse) -> Result<()> {
    check_resp(&resp)?;
    if let Some(payload) = resp.payload {
        let count = payload["imported"].as_i64().unwrap_or(0);
        writeln!(w, "imported {count} synonym pairs from WordNet")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s(buf: &[u8]) -> String {
        String::from_utf8(buf.to_vec()).unwrap()
    }

    #[test]
    fn check_resp_passes_ok_and_bails_on_error() {
        assert!(check_resp(&DaemonResponse::success_empty()).is_ok());
        let err = check_resp(&DaemonResponse::error("boom")).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn check_resp_error_without_message_has_fallback() {
        let resp = DaemonResponse {
            ok: false,
            error: None,
            payload: None,
        };
        let err = check_resp(&resp).unwrap_err();
        assert!(err.to_string().contains("no message"));
    }

    #[test]
    fn render_search_json_writes_pretty_payload() {
        let mut buf = Vec::new();
        let resp = DaemonResponse::success(json!({"results": [], "total_hits": 0}));
        render_search(&mut buf, resp, "json").unwrap();
        let out = s(&buf);
        assert!(out.contains("\"total_hits\": 0"), "got: {out}");
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn render_search_text_empty_results_renders_via_format_text() {
        let mut buf = Vec::new();
        let resp = DaemonResponse::success(json!({"results": [], "total_hits": 0}));
        render_search(&mut buf, resp, "text").unwrap();
        // Empty results have no paths to relativize, so the output is
        // CWD-independent and deterministic (format_text's empty contract).
        assert_eq!(s(&buf), "No results found.");
    }

    #[test]
    fn render_search_text_nonempty_deserializes_and_formats() {
        let mut buf = Vec::new();
        let resp = DaemonResponse::success(json!({
            "results": [{
                "source_file": "/proj/notes/a.md",
                "source_type": "note",
                "section_path": "Intro",
                "snippet": "hello world",
                "score": 0.5,
                "status": null,
                "related_docs": []
            }],
            "total_hits": 1
        }));
        render_search(&mut buf, resp, "text").unwrap();
        let out = s(&buf);
        // Assert the CWD-independent parts of format_text's non-empty output
        // (the path is relativized against CWD, so it is not asserted).
        assert!(out.contains("Showing 1 of 1 results"), "got: {out}");
        assert!(out.contains("[note]"), "got: {out}");
        assert!(out.contains("hello world"), "got: {out}");
    }

    #[test]
    fn render_search_bad_results_payload_errors() {
        let mut buf = Vec::new();
        // `results` is not an array of SearchResult → parse error.
        let resp = DaemonResponse::success(json!({"results": 42, "total_hits": 0}));
        let err = render_search(&mut buf, resp, "text").unwrap_err();
        assert!(err.to_string().contains("Failed to parse search results"));
    }

    #[test]
    fn render_search_not_ok_bails() {
        let mut buf = Vec::new();
        assert!(render_search(&mut buf, DaemonResponse::error("x"), "json").is_err());
    }

    #[test]
    fn render_index_writes_counts() {
        let mut buf = Vec::new();
        let resp = DaemonResponse::success(json!({"indexed": 3, "skipped": 1, "removed": 2}));
        render_index(&mut buf, resp).unwrap();
        assert_eq!(s(&buf), "indexed: 3, skipped: 1, removed: 2\n");
    }

    #[test]
    fn render_index_missing_fields_default_to_zero() {
        let mut buf = Vec::new();
        render_index(&mut buf, DaemonResponse::success(json!({}))).unwrap();
        assert_eq!(s(&buf), "indexed: 0, skipped: 0, removed: 0\n");
    }

    #[test]
    fn render_ingest_indexed_vs_unchanged() {
        let path = std::path::Path::new("/tmp/sess/foo.jsonl");
        let mut buf = Vec::new();
        render_ingest(
            &mut buf,
            DaemonResponse::success(json!({"indexed": true})),
            path,
        )
        .unwrap();
        assert_eq!(s(&buf), "session indexed: foo.jsonl\n");

        let mut buf2 = Vec::new();
        render_ingest(
            &mut buf2,
            DaemonResponse::success(json!({"indexed": false})),
            path,
        )
        .unwrap();
        assert_eq!(s(&buf2), "session unchanged: foo.jsonl\n");
    }

    #[test]
    fn render_status_parses_and_renders_minimal_info() {
        // Minimal valid StatusInfo: required bools present, Options default to None.
        let resp = DaemonResponse::success(json!({
            "daemon_running": false,
            "embedder_running": false,
            "watcher_running": false
        }));
        // Text goes to stdout via cli::print_status_info; assert it parses + runs.
        render_status(resp).unwrap();
    }

    #[test]
    fn render_status_bad_payload_errors() {
        let resp = DaemonResponse::success(json!({"daemon_running": "not-a-bool"}));
        let err = render_status(resp).unwrap_err();
        assert!(err.to_string().contains("Failed to parse daemon status"));
    }

    #[test]
    fn render_status_not_ok_bails() {
        assert!(render_status(DaemonResponse::error("down")).is_err());
    }

    #[test]
    fn render_doctor_json_parses_and_renders() {
        // JSON goes to stdout via write_json; assert it parses + runs (the
        // pretty-print bytes are asserted via render_search_json).
        let resp = DaemonResponse::success(json!({"sections": []}));
        render_doctor(resp, "json").unwrap();
    }

    #[test]
    fn render_doctor_text_parses_and_renders() {
        let resp = DaemonResponse::success(json!({"sections": []}));
        // Text goes to stdout via cli::render_doctor_report; assert it parses + runs.
        render_doctor(resp, "text").unwrap();
    }

    #[test]
    fn render_doctor_bad_payload_text_errors() {
        let resp = DaemonResponse::success(json!({"sections": "nope"}));
        let err = render_doctor(resp, "text").unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to parse daemon doctor report"));
    }

    #[test]
    fn render_doctor_not_ok_bails() {
        assert!(render_doctor(DaemonResponse::error("down"), "json").is_err());
    }

    #[test]
    fn render_reload_writes_confirmation_and_warnings() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        // Mixed array: non-string elements are skipped, strings go to `err`.
        let resp = DaemonResponse::success(json!({"warnings": ["w1", 42, "w2"]}));
        render_reload(&mut out, &mut err, resp).unwrap();
        assert_eq!(s(&out), "config reloaded\n");
        assert_eq!(s(&err), "warning: w1\nwarning: w2\n");
    }

    #[test]
    fn render_reload_no_warnings_writes_only_confirmation() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        render_reload(&mut out, &mut err, DaemonResponse::success_empty()).unwrap();
        assert_eq!(s(&out), "config reloaded\n");
        assert!(s(&err).is_empty());
    }

    #[test]
    fn render_reload_not_ok_bails() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert!(render_reload(&mut out, &mut err, DaemonResponse::error("x")).is_err());
    }

    #[test]
    fn render_reindex_writes_started_message() {
        let mut buf = Vec::new();
        render_reindex(&mut buf, DaemonResponse::success_empty()).unwrap();
        assert_eq!(
            s(&buf),
            "reindex started. Run `tsm doctor` to check progress.\n"
        );
    }

    #[test]
    fn render_import_wordnet_writes_count() {
        let mut buf = Vec::new();
        render_import_wordnet(&mut buf, DaemonResponse::success(json!({"imported": 524}))).unwrap();
        assert_eq!(s(&buf), "imported 524 synonym pairs from WordNet\n");
    }

    #[test]
    fn render_import_wordnet_missing_count_defaults_zero() {
        let mut buf = Vec::new();
        render_import_wordnet(&mut buf, DaemonResponse::success(json!({}))).unwrap();
        assert_eq!(s(&buf), "imported 0 synonym pairs from WordNet\n");
    }

    // A successful response with no payload writes nothing (the `if let
    // Some(payload)` guard's None arm). Pins the silent-on-empty behavior.
    #[test]
    fn render_index_no_payload_writes_nothing() {
        let mut buf = Vec::new();
        render_index(&mut buf, DaemonResponse::success_empty()).unwrap();
        assert!(s(&buf).is_empty());
    }

    #[test]
    fn render_ingest_no_payload_writes_nothing() {
        let mut buf = Vec::new();
        let path = std::path::Path::new("/tmp/sess/foo.jsonl");
        render_ingest(&mut buf, DaemonResponse::success_empty(), path).unwrap();
        assert!(s(&buf).is_empty());
    }

    #[test]
    fn render_import_wordnet_no_payload_writes_nothing() {
        let mut buf = Vec::new();
        render_import_wordnet(&mut buf, DaemonResponse::success_empty()).unwrap();
        assert!(s(&buf).is_empty());
    }
}
