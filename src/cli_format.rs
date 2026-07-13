//! Pure text rendering for CLI status / doctor / candidate output.
//!
//! These helpers take fully-resolved data plus any environment- or
//! clock-derived inputs (the `use_color` flag, the `now` timestamp) as
//! parameters and return the rendered `String`. The I/O shell in `cli.rs` reads
//! `NO_COLOR`, samples the clock, and prints the result to stdout / stderr;
//! keeping that side out of here makes every rendering branch exercisable
//! without capturing real output.
//!
//! Rendering builds into a `String` via `std::fmt::Write` (infallible) rather
//! than streaming through `io::Write` with `?` at every line, so the branch
//! count reflects the layout logic, not one error arm per write.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};

use crate::cli::{CheckStatus, DoctorReport, StatusInfo};
use crate::user_dict;

/// Strip ANSI escape sequences so visible width can be measured for box
/// alignment.
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Render the `tsm doctor` report as a bordered box. `use_color` is resolved by
/// the caller from `NO_COLOR`; when `false` no ANSI codes are emitted, which is
/// also what the characterization tests pin.
pub fn render_doctor_report(use_color: bool, report: &DoctorReport) -> String {
    let (green, yellow, red, bold, dim, reset) = if use_color {
        (
            "\x1b[32m", "\x1b[33m", "\x1b[31m", "\x1b[1m", "\x1b[2m", "\x1b[0m",
        )
    } else {
        ("", "", "", "", "", "")
    };

    // Collect all rendered lines to compute box width
    let title = "The Space Memory \u{2014} Doctor";
    let mut body_lines: Vec<String> = Vec::new();

    for (i, section) in report.sections.iter().enumerate() {
        if i > 0 {
            body_lines.push(String::new()); // blank separator
        }
        body_lines.push(format!("{bold}  {}{reset}", section.name));
        for item in &section.items {
            let (icon, color) = match item.status {
                CheckStatus::Ok => ("\u{2714}", green),       // ✔
                CheckStatus::Warning => ("\u{26a0}", yellow), // ⚠
                CheckStatus::Error => ("\u{2718}", red),      // ✘
            };
            let line = match &item.hint {
                Some(hint) => format!(
                    "    {color}{icon}{reset} {}  {dim}{hint}{reset}",
                    item.message
                ),
                None => format!("    {color}{icon}{reset} {}", item.message),
            };
            body_lines.push(line);
        }
    }

    // Summary line
    let issue_count = report.issue_count();
    body_lines.push(String::new());
    if issue_count > 0 {
        body_lines.push(format!("  {yellow}{issue_count} issue(s) found.{reset}"));
    } else {
        body_lines.push(format!("  {green}All good.{reset}"));
    }

    let content_width = body_lines
        .iter()
        .map(|l| strip_ansi(l).chars().count())
        .max()
        .unwrap_or(0)
        .max(title.chars().count() + 4);
    let box_width = content_width + 2; // padding

    let mut out = String::new();
    // Top border + leading blank row
    let _ = writeln!(
        out,
        "{dim}\u{256d}\u{2500} {reset}{bold}{title}{reset} {dim}{}\u{256e}{reset}",
        "\u{2500}".repeat(box_width - title.chars().count() - 3)
    );
    let _ = writeln!(
        out,
        "{dim}\u{2502}{reset}{}{dim}\u{2502}{reset}",
        " ".repeat(box_width)
    );

    for line in &body_lines {
        let visible_len = strip_ansi(line).chars().count();
        let pad = box_width.saturating_sub(visible_len);
        let _ = writeln!(
            out,
            "{dim}\u{2502}{reset}{line}{}{dim}\u{2502}{reset}",
            " ".repeat(pad)
        );
    }

    // Trailing blank row + bottom border
    let _ = writeln!(
        out,
        "{dim}\u{2502}{reset}{}{dim}\u{2502}{reset}",
        " ".repeat(box_width)
    );
    let _ = writeln!(
        out,
        "{dim}\u{2570}{}\u{256f}{reset}",
        "\u{2500}".repeat(box_width)
    );
    out
}

/// Render the embedder/watcher "running since" line body, or a bare "running"
/// when no timestamp is known. Factored out so `render_status` stays focused on
/// the section layout rather than the per-section running/stopped branching.
fn running_since_suffix(since: Option<&String>, now: DateTime<Utc>) -> String {
    match since {
        Some(s) => format!(" (since {})", format_since(s, now)),
        None => String::new(),
    }
}

/// Render the backfill section line(s) into `out`.
fn render_backfill(out: &mut String, info: &StatusInfo, now: DateTime<Utc>) {
    let Some(bf) = info.backfill.as_ref() else {
        let _ = writeln!(out, "  Backfill:  idle");
        return;
    };
    let pct = if bf.total > 0 {
        (bf.filled as f64 / bf.total as f64 * 100.0) as u32
    } else {
        0
    };
    let since = format_since(&bf.since, now);
    let processed = bf.filled + bf.errors;
    let eta = if processed > 0 && bf.total > 0 {
        estimate_eta(&bf.since, processed, bf.total as usize, now)
    } else {
        "calculating...".to_string()
    };
    let _ = writeln!(
        out,
        "  Backfill:  {}/{} ({pct}%) — running since {since}, ETA {eta}",
        bf.filled, bf.total
    );
    if bf.errors > 0 {
        let _ = writeln!(out, "             {} errors", bf.errors);
    }
}

/// Render the documents/chunks/vectors/dict section into `out`.
fn render_db_stats(out: &mut String, info: &StatusInfo) {
    let (Some(docs), Some(chunks), Some(vecs)) = (info.documents, info.chunks, info.vectors) else {
        let _ = writeln!(out, "  DB:        not found");
        return;
    };
    let _ = writeln!(out, "  Documents: {docs}");
    let _ = writeln!(out, "  Chunks:    {chunks}");
    if chunks > 0 {
        let pct = (vecs as f64 / chunks as f64 * 100.0) as u32;
        let _ = writeln!(out, "  Vectors:   {vecs}/{chunks} ({pct}%)");
    } else {
        let _ = writeln!(out, "  Vectors:   0");
    }
    if let Some(ready) = info.dict_candidates_ready {
        if ready > 0 {
            let _ = writeln!(out, "  Dict:      {ready} candidates ready");
        } else {
            let _ = writeln!(out, "  Dict:      no candidates ready");
        }
    }
}

/// Render `tsm status` as a plain-text block. `now` is the reference instant for
/// the backfill ETA; the caller samples the clock so the rendering stays
/// deterministic under test.
pub fn render_status(info: &StatusInfo, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "=== The Space Memory Status ===\n");

    // Daemon
    match (info.daemon_running, info.daemon_pid) {
        (true, Some(pid)) => {
            let _ = writeln!(out, "  Daemon:    running (PID {pid})");
        }
        (true, None) => {
            let _ = writeln!(out, "  Daemon:    running");
        }
        (false, _) => {
            let _ = writeln!(out, "  Daemon:    stopped");
        }
    }

    // Embedder
    if info.embedder_running {
        let suffix = match (info.embedder_pid, info.embedder_since.as_ref()) {
            (Some(pid), Some(since)) => {
                format!(" (since {}, PID {pid})", format_since(since, now))
            }
            _ => String::new(),
        };
        let _ = writeln!(out, "  Embedder:  running{suffix}");
    } else {
        let _ = writeln!(out, "  Embedder:  stopped");
    }

    // Watcher
    if info.watcher_running {
        let suffix = running_since_suffix(info.watcher_since.as_ref(), now);
        let _ = writeln!(out, "  Watcher:   running{suffix}");
    } else {
        let _ = writeln!(out, "  Watcher:   stopped");
    }

    render_backfill(&mut out, info, now);
    render_db_stats(&mut out, info);
    out
}

/// Format an RFC3339 instant relative to `now`: within a rolling 24h window
/// of `now` (in either direction) it renders as `HH:MM:SS`; past that window
/// (or, under clock skew, further in the future) it includes the date so a
/// stale entry from days ago cannot be mistaken for one that started this
/// morning. Falls back to the raw string when it cannot be parsed.
fn format_since(rfc3339: &str, now: DateTime<Utc>) -> String {
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(rfc3339) else {
        return rfc3339.to_string();
    };
    let dt = dt.with_timezone(&Utc);
    if (now - dt).num_hours().abs() >= 24 {
        dt.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        dt.format("%H:%M:%S").to_string()
    }
}

/// Estimate remaining backfill time from the elapsed `now - started_at` and the
/// processed/total counts. `now` is supplied by the caller so the estimate is
/// deterministic; an unparseable start, non-positive elapsed, or zero processed
/// yields a textual placeholder rather than a number.
pub fn estimate_eta(
    started_at: &str,
    processed: usize,
    total: usize,
    now: DateTime<Utc>,
) -> String {
    let Ok(start) = chrono::DateTime::parse_from_rfc3339(started_at) else {
        return "unknown".to_string();
    };
    let elapsed = now.signed_duration_since(start);
    let elapsed_secs = elapsed.num_seconds() as f64;
    if elapsed_secs <= 0.0 || processed == 0 {
        return "calculating...".to_string();
    }
    let remaining = total.saturating_sub(processed);
    let rate = processed as f64 / elapsed_secs;
    let eta_secs = (remaining as f64 / rate) as i64;
    if eta_secs < 60 {
        format!("~{eta_secs}s")
    } else {
        format!("~{}m", eta_secs / 60)
    }
}

/// Render one dictionary candidate row (frequency + first/last seen dates).
pub fn render_candidate(c: &user_dict::Candidate) -> String {
    format!(
        "  {:<20} {:>3} hits  (first: {}, last: {})\n",
        c.surface,
        c.frequency,
        &c.first_seen[..10.min(c.first_seen.len())],
        &c.last_seen[..10.min(c.last_seen.len())]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{BackfillInfo, CheckItem, DoctorSection};

    fn item(status: CheckStatus, message: &str, hint: Option<&str>) -> CheckItem {
        CheckItem {
            status,
            message: message.to_string(),
            hint: hint.map(str::to_string),
        }
    }

    // A report spanning every status × hint-present/absent combination plus a
    // multi-section layout, so the box renderer's branches are all exercised.
    fn sample_report() -> DoctorReport {
        DoctorReport {
            sections: vec![
                DoctorSection {
                    name: "Daemon".to_string(),
                    items: vec![
                        item(CheckStatus::Ok, "running", None),
                        item(
                            CheckStatus::Warning,
                            "stale socket",
                            Some("run tsm restart"),
                        ),
                    ],
                },
                DoctorSection {
                    name: "Index".to_string(),
                    items: vec![item(CheckStatus::Error, "DB missing", None)],
                },
            ],
        }
    }

    #[test]
    fn doctor_report_no_color_golden() {
        let got = render_doctor_report(false, &sample_report());
        // Characterization golden captured from the live renderer. The box is
        // sized to the widest visible line ("    ⚠ stale socket  run tsm restart").
        let expected = "\
╭─ The Space Memory — Doctor ─────────╮
│                                     │
│  Daemon                             │
│    ✔ running                        │
│    ⚠ stale socket  run tsm restart  │
│                                     │
│  Index                              │
│    ✘ DB missing                     │
│                                     │
│  2 issue(s) found.                  │
│                                     │
╰─────────────────────────────────────╯
";
        assert_eq!(got, expected);
    }

    #[test]
    fn doctor_report_all_good_when_no_issues() {
        let report = DoctorReport {
            sections: vec![DoctorSection {
                name: "Build".to_string(),
                items: vec![item(CheckStatus::Ok, "Version: v1.0.0", None)],
            }],
        };
        let got = render_doctor_report(false, &report);
        assert!(got.contains("All good."));
        assert!(!got.contains("issue(s) found"));
    }

    #[test]
    fn doctor_report_color_emits_ansi_codes() {
        let got = render_doctor_report(true, &sample_report());
        assert!(got.contains("\x1b[32m"), "green for Ok");
        assert!(got.contains("\x1b[33m"), "yellow for Warning");
        assert!(got.contains("\x1b[31m"), "red for Error");
        assert!(got.contains("\x1b[0m"), "reset");
    }

    #[test]
    fn doctor_report_empty_uses_title_width() {
        // No sections → only the summary line; box still sizes to the title.
        let got = render_doctor_report(false, &DoctorReport::default());
        assert!(got.contains("The Space Memory \u{2014} Doctor"));
        assert!(got.contains("All good."));
        // Every rendered row must share one display width. This guards the
        // title-is-widest path (content_width = title.chars().count() + 4),
        // which the populated golden does not exercise: a byte-vs-char
        // regression there would silently widen the body relative to the
        // title-sized top border. Measure by columns, not bytes, since the
        // box contains multi-byte characters.
        let widths: Vec<usize> = got.lines().map(|l| l.chars().count()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "doctor box rows must share one display width: {widths:?}"
        );
    }

    fn base_status() -> StatusInfo {
        StatusInfo {
            daemon_running: false,
            daemon_pid: None,
            daemon_socket: None,
            embedder_running: false,
            embedder_pid: None,
            embedder_since: None,
            watcher_running: false,
            watcher_since: None,
            backfill: None,
            documents: None,
            chunks: None,
            vectors: None,
            dict_candidates_ready: None,
        }
    }

    #[test]
    fn status_all_stopped_and_db_absent() {
        let got = render_status(&base_status(), Utc::now());
        assert!(got.contains("Daemon:    stopped"));
        assert!(got.contains("Embedder:  stopped"));
        assert!(got.contains("Watcher:   stopped"));
        assert!(got.contains("Backfill:  idle"));
        assert!(got.contains("DB:        not found"));
    }

    #[test]
    fn status_running_with_pids_and_since() {
        // `now` is a few hours after `since` (same day) so both render bare
        // `HH:MM:SS`, not the >=24h date-fallback form.
        let mut info = base_status();
        info.daemon_running = true;
        info.daemon_pid = Some(42);
        info.embedder_running = true;
        info.embedder_pid = Some(7);
        info.embedder_since = Some("2026-06-30T08:00:00Z".to_string());
        info.watcher_running = true;
        info.watcher_since = Some("2026-06-30T08:00:00Z".to_string());
        let now = DateTime::parse_from_rfc3339("2026-06-30T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let got = render_status(&info, now);
        assert!(got.contains("Daemon:    running (PID 42)"));
        assert!(got.contains("Embedder:  running (since 08:00:00, PID 7)"));
        assert!(got.contains("Watcher:   running (since 08:00:00)"));
    }

    #[test]
    fn status_running_since_over_24h_ago_falls_back_to_date() {
        // A stale entry (e.g. a leftover from a previous process) must not be
        // mistaken for one that started "this morning" — the display switches
        // to a date-qualified form once it crosses the 24h boundary.
        let mut info = base_status();
        info.embedder_running = true;
        info.embedder_pid = Some(7);
        info.embedder_since = Some("2026-06-26T06:52:01Z".to_string());
        let now = DateTime::parse_from_rfc3339("2026-07-06T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let got = render_status(&info, now);
        assert!(got.contains("Embedder:  running (since 2026-06-26 06:52:01, PID 7)"));
    }

    #[test]
    fn status_running_without_pid_or_since() {
        let mut info = base_status();
        info.daemon_running = true;
        info.embedder_running = true; // no pid/since → bare "running"
        info.watcher_running = true; // no since → bare "running"
        let got = render_status(&info, Utc::now());
        assert!(got.contains("Daemon:    running\n"));
        assert!(got.contains("Embedder:  running\n"));
        assert!(got.contains("Watcher:   running\n"));
    }

    #[test]
    fn status_embedder_running_with_pid_but_no_since_is_bare() {
        // The extended suffix needs BOTH pid and since; a partial (pid only)
        // must fall through to a bare "running" line, same as the original.
        let mut info = base_status();
        info.embedder_running = true;
        info.embedder_pid = Some(7);
        info.embedder_since = None;
        let got = render_status(&info, Utc::now());
        assert!(got.contains("Embedder:  running\n"));
        assert!(!got.contains("PID 7"));
    }

    #[test]
    fn status_backfill_no_errors_still_computes_eta() {
        // filled>0, errors=0, total>0: ETA is computed and the errors line is
        // absent (guards the `bf.errors > 0` branch independently of the ETA).
        let mut info = base_status();
        info.backfill = Some(BackfillInfo {
            filled: 50,
            total: 100,
            errors: 0,
            since: "2026-06-30T08:00:00Z".to_string(),
        });
        let now = DateTime::parse_from_rfc3339("2026-06-30T08:01:40Z")
            .unwrap()
            .with_timezone(&Utc);
        let got = render_status(&info, now);
        assert!(got.contains("Backfill:  50/100 (50%) — running since 08:00:00, ETA ~"));
        assert!(!got.contains("errors"));
    }

    #[test]
    fn status_backfill_progress_and_eta() {
        let mut info = base_status();
        info.backfill = Some(BackfillInfo {
            filled: 50,
            total: 100,
            errors: 5,
            since: "2026-06-30T08:00:00Z".to_string(),
        });
        // 55 processed in 100s → ~0.55/s; 45 remaining → ~81s → "~1m".
        let now = DateTime::parse_from_rfc3339("2026-06-30T08:01:40Z")
            .unwrap()
            .with_timezone(&Utc);
        let got = render_status(&info, now);
        assert!(got.contains("Backfill:  50/100 (50%) — running since 08:00:00, ETA ~1m"));
        assert!(got.contains("5 errors"));
    }

    #[test]
    fn status_backfill_zero_total_calculating() {
        let mut info = base_status();
        info.backfill = Some(BackfillInfo {
            filled: 0,
            total: 0,
            errors: 0,
            since: "2026-06-30T08:00:00Z".to_string(),
        });
        let got = render_status(&info, Utc::now());
        assert!(got.contains("(0%)"));
        assert!(got.contains("ETA calculating..."));
    }

    #[test]
    fn status_db_stats_with_vectors_and_dict() {
        let mut info = base_status();
        info.documents = Some(10);
        info.chunks = Some(20);
        info.vectors = Some(15);
        info.dict_candidates_ready = Some(3);
        let got = render_status(&info, Utc::now());
        assert!(got.contains("Documents: 10"));
        assert!(got.contains("Chunks:    20"));
        assert!(got.contains("Vectors:   15/20 (75%)"));
        assert!(got.contains("Dict:      3 candidates ready"));
    }

    #[test]
    fn status_db_stats_zero_chunks_and_no_dict_candidates() {
        let mut info = base_status();
        info.documents = Some(0);
        info.chunks = Some(0);
        info.vectors = Some(0);
        info.dict_candidates_ready = Some(0);
        let got = render_status(&info, Utc::now());
        assert!(got.contains("Vectors:   0\n"));
        assert!(got.contains("Dict:      no candidates ready"));
    }

    #[test]
    fn status_db_stats_present_but_dict_count_absent_omits_dict_line() {
        // DB stats present with dict_candidates_ready = None: no `Dict:` line.
        let mut info = base_status();
        info.documents = Some(5);
        info.chunks = Some(10);
        info.vectors = Some(10);
        info.dict_candidates_ready = None;
        let got = render_status(&info, Utc::now());
        assert!(got.contains("Vectors:   10/10 (100%)"));
        assert!(!got.contains("Dict:"));
    }

    #[test]
    fn format_since_parses_and_falls_back() {
        let now = DateTime::parse_from_rfc3339("2026-06-30T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(format_since("2026-06-30T08:09:10Z", now), "08:09:10");
        assert_eq!(format_since("not-a-date", now), "not-a-date");
    }

    #[test]
    fn format_since_falls_back_to_date_past_24h() {
        let now = DateTime::parse_from_rfc3339("2026-07-06T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            format_since("2026-06-26T06:52:01Z", now),
            "2026-06-26 06:52:01"
        );
    }

    #[test]
    fn format_since_handles_future_timestamp_clock_skew() {
        // A `since` in the future (clock skew) also crosses the 24h check via
        // `.abs()`; it must not panic and must switch to the date form too.
        let now = DateTime::parse_from_rfc3339("2026-06-26T06:52:01Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            format_since("2026-07-06T09:00:00Z", now),
            "2026-07-06 09:00:00"
        );
    }

    #[test]
    fn format_since_just_under_24h_stays_time_only() {
        let now = DateTime::parse_from_rfc3339("2026-06-27T06:52:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(format_since("2026-06-26T06:52:01Z", now), "06:52:01");
    }

    #[test]
    fn format_since_exactly_24h_falls_back_to_date() {
        // The boundary itself: `>= 24` is what triggers the date fallback, so
        // this guards against an off-by-one (e.g. a `> 24` regression) on the
        // one comparison the whole #355 fix hinges on.
        let now = DateTime::parse_from_rfc3339("2026-06-27T06:52:01Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            format_since("2026-06-26T06:52:01Z", now),
            "2026-06-26 06:52:01"
        );
    }

    #[test]
    fn estimate_eta_seconds_minutes_and_placeholders() {
        let start = "2026-06-30T08:00:00Z";
        let now = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        // 10 processed in 10s → 1/s; 30 remaining → ~30s.
        assert_eq!(
            estimate_eta(start, 10, 40, now("2026-06-30T08:00:10Z")),
            "~30s"
        );
        // 10 processed in 10s → 1/s; 120 remaining → ~120s → "~2m".
        assert_eq!(
            estimate_eta(start, 10, 130, now("2026-06-30T08:00:10Z")),
            "~2m"
        );
        // Unparseable start.
        assert_eq!(
            estimate_eta("bad", 1, 2, now("2026-06-30T08:00:10Z")),
            "unknown"
        );
        // Non-positive elapsed.
        assert_eq!(
            estimate_eta(start, 1, 2, now("2026-06-30T08:00:00Z")),
            "calculating..."
        );
        // Zero processed.
        assert_eq!(
            estimate_eta(start, 0, 2, now("2026-06-30T08:00:10Z")),
            "calculating..."
        );
    }

    #[test]
    fn render_candidate_truncates_dates_to_ten_chars() {
        let c = user_dict::Candidate {
            surface: "自然言語処理".to_string(),
            frequency: 12,
            pos: "名詞".to_string(),
            source: "auto".to_string(),
            first_seen: "2026-06-01T00:00:00Z".to_string(),
            last_seen: "2026-06-30T00:00:00Z".to_string(),
            status: "pending".to_string(),
        };
        let got = render_candidate(&c);
        assert!(got.contains("12 hits"));
        assert!(got.contains("first: 2026-06-01"));
        assert!(got.contains("last: 2026-06-30"));
        assert!(!got.contains("00:00:00"));
        assert!(got.ends_with('\n'));
    }

    #[test]
    fn render_candidate_short_dates_do_not_panic() {
        // Dates shorter than 10 chars exercise the `10.min(len)` guard's
        // len-branch (vs the always-10 branch the 20-char case takes); the
        // slice must clamp to the string length rather than panicking.
        let c = user_dict::Candidate {
            surface: "x".to_string(),
            frequency: 1,
            pos: "名詞".to_string(),
            source: "auto".to_string(),
            first_seen: "2026-06".to_string(),
            last_seen: "26".to_string(),
            status: "pending".to_string(),
        };
        let got = render_candidate(&c);
        assert!(got.contains("first: 2026-06,"));
        assert!(got.contains("last: 26)"));
    }
}
