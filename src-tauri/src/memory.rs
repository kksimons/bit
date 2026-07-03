//! Persistent memory + activity log for the agent.
//!
//! Two files in the app config dir (beside settings.json):
//!   - activity.jsonl: append-only, one JSON event per line (every tool call
//!     and every ask), each timestamped. The raw, grep-able source of truth.
//!   - context.md: a durable Markdown "second brain". A one-line summary is
//!     appended after each command, and the agent appends lessons via the
//!     `remember` tool. Hand-editable; persists until the user prunes it.
//!
//! Nothing here is injected into the prompt by default — the agent pulls
//! history on demand via the `recall_history` tool, so isolated commands cost
//! no extra tokens or latency.
//!
//! The file-touching core takes a `&Path` (the config dir) so it's unit-testable
//! with a temp dir; the `AppHandle`-based public API is a thin wrapper over it.

use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::Manager;

fn activity_path(dir: &Path) -> PathBuf {
    dir.join("activity.jsonl")
}
fn context_path(dir: &Path) -> PathBuf {
    dir.join("context.md")
}

/// The config dir that holds both memory files (beside settings.json).
fn dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok()
}

/// ISO-8601 UTC timestamp without pulling in chrono, e.g. 2026-07-03T14:22:01Z.
fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let day = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let (h, m, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    // civil_from_days (Howard Hinnant's algorithm) — valid for all day >= 0.
    let z = day + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Collapse whitespace + trim, so single-line log/markdown entries stay clean.
fn scrub(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to ~n chars on a UTF-8 boundary, with an ellipsis. One pass.
fn take(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Append one JSON line to activity.jsonl. Best-effort: logging never fails the agent.
fn append_event(dir: &Path, event: Value) {
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(activity_path(dir))
    {
        let _ = writeln!(f, "{event}");
    }
}

/// Append a Markdown line to context.md (per-command summaries + lessons).
fn append_context_line(dir: &Path, line: &str) -> Result<(), String> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(context_path(dir))
        .map_err(|e| e.to_string())?;
    writeln!(f, "{line}").map_err(|e| e.to_string())
}

// ---- core (dir-based; unit-tested) ----

/// Log a tool call (called from the agent loop after each execute).
pub fn log_tool_call_to(
    dir: &Path,
    name: &str,
    args: &Value,
    result: &str,
    is_error: bool,
    ms: u128,
) {
    append_event(
        dir,
        json!({
            "ts": now_iso(),
            "type": "tool",
            "name": name,
            "args": args,
            "ok": !is_error,
            "ms": ms,
            "result": take(result, 600),
        }),
    );
}

/// Log a completed ask and append a one-line Markdown summary to context.md.
pub fn log_ask_to(
    dir: &Path,
    transcript: &str,
    verdict: &str,
    times: u8,
    n_tools: usize,
    ms: u128,
    err: Option<&str>,
) {
    append_event(
        dir,
        json!({
            "ts": now_iso(),
            "type": "ask",
            "transcript": scrub(transcript),
            "verdict": verdict,
            "times": times,
            "tools": n_tools,
            "ms": ms,
            "error": err,
        }),
    );
    let suffix = if times > 1 {
        format!(" x{times}")
    } else {
        String::new()
    };
    let line = match err {
        Some(e) => format!(
            "- [{}] \"{}\" → ERROR: {} ({} tool{}, {}ms)",
            now_iso(),
            scrub(transcript),
            take(e, 160),
            n_tools,
            if n_tools == 1 { "" } else { "s" },
            ms
        ),
        None => format!(
            "- [{}] \"{}\" → {}{} ({} tool{}, {}ms)",
            now_iso(),
            scrub(transcript),
            verdict,
            suffix,
            n_tools,
            if n_tools == 1 { "" } else { "s" },
            ms
        ),
    };
    let _ = append_context_line(dir, &line);
}

/// Format one parsed activity event as a readable line for the agent.
fn format_event(v: &Value) -> Option<String> {
    let ts = v.get("ts").and_then(|t| t.as_str()).unwrap_or("?");
    match v.get("type").and_then(|t| t.as_str()) {
        Some("tool") => {
            let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let args =
                serde_json::to_string(v.get("args").unwrap_or(&Value::Null)).unwrap_or_default();
            let ok = v.get("ok").and_then(|b| b.as_bool()).unwrap_or(true);
            let res = v.get("result").and_then(|r| r.as_str()).unwrap_or("");
            Some(format!(
                "[{ts}] tool {name} args={args} {} {}",
                if ok { "ok:" } else { "ERROR:" },
                take(res, 200)
            ))
        }
        Some("ask") => {
            let transcript = v.get("transcript").and_then(|t| t.as_str()).unwrap_or("");
            let verdict = v.get("verdict").and_then(|t| t.as_str()).unwrap_or("?");
            let err = v.get("error").and_then(|e| e.as_str());
            Some(format!(
                "[{ts}] asked \"{}\" → {}",
                scrub(transcript),
                err.map(|e| format!("ERROR: {}", take(e, 120)))
                    .unwrap_or_else(|| verdict.to_string())
            ))
        }
        _ => None,
    }
}

/// Return matching (or most recent) context.md lines, newest first.
fn grep_context(dir: &Path, query: Option<&str>, limit: usize) -> Vec<String> {
    let Ok(text) = fs::read_to_string(context_path(dir)) else {
        return Vec::new();
    };
    let mut all: Vec<String> = text.lines().map(str::to_string).collect();
    if let Some(q) = query {
        let ql = q.to_lowercase();
        all.retain(|l| l.to_lowercase().contains(&ql));
    }
    all.reverse();
    all.into_iter().take(limit).collect()
}

/// Recall recent activity (+ matching memory notes) as readable text for the
/// agent. `query` filters (case-insensitive substring); `limit` caps the count.
pub fn recall_from(dir: &Path, query: Option<&str>, limit: usize) -> String {
    let limit = limit.clamp(1, 50);
    let q = query.map(str::to_lowercase);

    let mut events: Vec<String> = Vec::new();
    if let Ok(text) = fs::read_to_string(activity_path(dir)) {
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(fmt) = format_event(&v) else {
                continue;
            };
            if let Some(q) = &q {
                if !fmt.to_lowercase().contains(q) {
                    continue;
                }
            }
            events.push(fmt);
        }
    }
    let start = events.len().saturating_sub(limit);

    let notes = grep_context(dir, q.as_deref(), 8);

    if events.is_empty() && notes.is_empty() {
        return "No history yet.".to_string();
    }
    let mut out = String::new();
    if !events.is_empty() {
        out.push_str("Recent activity (oldest first):\n");
        out.push_str(&events[start..].join("\n"));
    }
    if !notes.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Memory notes (newest first):\n");
        out.push_str(&notes.join("\n"));
    }
    out
}

/// Persist a one-line lesson/note to context.md (the `remember` tool).
pub fn remember_to(dir: &Path, note: &str) -> Result<String, String> {
    let note = scrub(note);
    if note.is_empty() {
        return Err("note is empty".into());
    }
    let line = format!("- [{}] (lesson) {}", now_iso(), note);
    append_context_line(dir, &line)?;
    Ok(format!("Remembered: {line}"))
}

// ---- AppHandle wrappers (used by agent/tools/lib) ----

pub fn log_tool_call(
    app: &tauri::AppHandle,
    name: &str,
    args: &Value,
    result: &str,
    is_error: bool,
    ms: u128,
) {
    if let Some(d) = dir(app) {
        log_tool_call_to(&d, name, args, result, is_error, ms);
    }
}

pub fn log_ask(
    app: &tauri::AppHandle,
    transcript: &str,
    verdict: &str,
    times: u8,
    n_tools: usize,
    ms: u128,
    err: Option<&str>,
) {
    if let Some(d) = dir(app) {
        log_ask_to(&d, transcript, verdict, times, n_tools, ms, err);
    }
}

pub fn recall(app: &tauri::AppHandle, query: Option<&str>, limit: usize) -> String {
    match dir(app) {
        Some(d) => recall_from(&d, query, limit),
        None => "No history yet.".to_string(),
    }
}

pub fn remember(app: &tauri::AppHandle, note: &str) -> Result<String, String> {
    let d = dir(app).ok_or("no config dir")?;
    remember_to(&d, note)
}

/// The memory folder (revealed in Finder by the "Open memory folder" button).
pub fn folder(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let d = dir(app).ok_or("no config dir")?;
    fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn scrub_collapses_whitespace() {
        assert_eq!(scrub("  hello   world  "), "hello world");
        assert_eq!(scrub("line\nbreak\ttab"), "line break tab");
    }

    #[test]
    fn take_truncates_on_char_boundary() {
        // Short string unchanged.
        assert_eq!(take("abc", 10), "abc");
        // ASCII cut.
        assert_eq!(take("abcdef", 3), "abc…");
        // Unicode cut does not panic and keeps whole chars.
        let cut = take("héllo🌍world", 6);
        assert!(cut.ends_with('…'));
        assert!(cut.chars().count() <= 7); // 6 chars + ellipsis
                                           // A char boundary: "héllo" is h,é,l,l,o (5 chars); cut at 5 keeps "héllo…"
        assert_eq!(take("héllo🌍", 5), "héllo…");
    }

    #[test]
    fn now_iso_is_well_formed() {
        // Expected shape: YYYY-MM-DDTHH:MM:SSZ (20 chars), no chrono/regex dep.
        let ts = now_iso();
        assert_eq!(ts.len(), 20, "ts: {ts}");
        let b = ts.as_bytes();
        assert_eq!(
            [b[4], b[7], b[10], b[13], b[16], b[19]],
            [b'-', b'-', b'T', b':', b':', b'Z']
        );
        for &i in &[0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18] {
            assert!(b[i].is_ascii_digit(), "non-digit at {i} in {ts}");
        }
        let (month, day) = (
            ts[5..7].parse::<u32>().unwrap(),
            ts[8..10].parse::<u32>().unwrap(),
        );
        let (h, m, s) = (
            ts[11..13].parse::<u32>().unwrap(),
            ts[14..16].parse::<u32>().unwrap(),
            ts[17..19].parse::<u32>().unwrap(),
        );
        assert!((1..=12).contains(&month) && (1..=31).contains(&day));
        assert!(h < 24 && m < 60 && s < 60);
    }

    #[test]
    fn log_tool_call_then_recall() {
        let d = tmp();
        log_tool_call_to(
            d.path(),
            "mcp__fizzy__create_card",
            &json!({"board":"MAG Solar","title":"fix panel"}),
            r#"{"number":42}"#,
            false,
            320,
        );
        let out = recall_from(d.path(), None, 10);
        assert!(out.contains("create_card"), "got: {out}");
        assert!(
            out.contains("42"),
            "recall should surface the card number: {out}"
        );
        assert!(out.contains("MAG Solar"));
    }

    #[test]
    fn log_ask_writes_jsonl_and_context() {
        let d = tmp();
        log_ask_to(
            d.path(),
            "create a card fix panel on MAG Solar",
            "yes",
            1,
            1,
            4200,
            None,
        );
        // activity.jsonl got an ask event...
        let activity = std::fs::read_to_string(activity_path(d.path())).unwrap();
        assert!(activity.contains("\"type\":\"ask\""));
        assert!(activity.contains("fix panel"));
        // ...and context.md got a one-line summary.
        let ctx = std::fs::read_to_string(context_path(d.path())).unwrap();
        assert!(ctx.contains("create a card fix panel on MAG Solar"));
        assert!(ctx.contains("→ yes"));
    }

    #[test]
    fn recall_resolves_it_after_create() {
        // Simulate a spoken multi-command flow: create (ask + tool), then "move it".
        let d = tmp();
        log_tool_call_to(
            d.path(),
            "mcp__fizzy__create_card",
            &json!({"board":"MAG Solar","title":"fix panel"}),
            r#"{"number":42,"title":"fix panel","board":"MAG Solar","url":"https://fiz/cards/42"}"#,
            false,
            350,
        );
        log_ask_to(
            d.path(),
            "create a card fix panel on MAG Solar",
            "yes",
            1,
            1,
            4600,
            None,
        );

        // Next command: "move it to blocked" — the agent recalls to resolve "it".
        let recalled = recall_from(d.path(), Some("42"), 10);
        assert!(recalled.contains("42"), "should find card 42: {recalled}");

        // Unfiltered recall returns the recent activity, oldest first.
        let recent = recall_from(d.path(), None, 10);
        let tool_idx = recent.find("tool").unwrap();
        let ask_idx = recent.find("asked").unwrap();
        assert!(
            tool_idx < ask_idx,
            "activity should be oldest-first: {recent}"
        );
    }

    #[test]
    fn remember_appends_a_lesson_and_recall_lists_it() {
        let d = tmp();
        let ok = remember_to(
            d.path(),
            "MAG Solar has a Blocked column — pass it straight to move_card",
        );
        assert!(ok.is_ok());
        let ctx = std::fs::read_to_string(context_path(d.path())).unwrap();
        assert!(ctx.contains("(lesson)"));
        assert!(ctx.contains("Blocked column"));
        // recall surfaces lessons under "Memory notes".
        let out = recall_from(d.path(), Some("Blocked"), 10);
        assert!(out.contains("Memory notes"), "got: {out}");
        assert!(out.contains("Blocked column"));
    }

    #[test]
    fn remember_rejects_empty_note() {
        let d = tmp();
        assert!(remember_to(d.path(), "   ").is_err());
    }

    #[test]
    fn error_ask_is_marked_error() {
        let d = tmp();
        log_ask_to(
            d.path(),
            "move it to blocked",
            "no",
            1,
            0,
            900,
            Some("No column matching 'blocked'"),
        );
        let ctx = std::fs::read_to_string(context_path(d.path())).unwrap();
        assert!(ctx.contains("ERROR"), "got: {ctx}");
        let out = recall_from(d.path(), None, 10);
        assert!(out.contains("ERROR"), "got: {out}");
    }

    #[test]
    fn recall_empty_is_no_history() {
        let d = tmp();
        assert_eq!(recall_from(d.path(), None, 10), "No history yet.");
    }

    #[test]
    fn recall_limit_caps_events() {
        let d = tmp();
        for i in 0..15 {
            log_ask_to(d.path(), &format!("command {i}"), "yes", 1, 0, 100, None);
        }
        let out = recall_from(d.path(), None, 5);
        assert_eq!(out.matches("asked").count(), 5, "got: {out}");
    }

    #[test]
    fn multiple_tool_calls_recalled_in_chronological_order() {
        // A multi-step command: list_boards then create_card, then the ask verdict.
        let d = tmp();
        log_tool_call_to(
            d.path(),
            "mcp__fizzy__list_boards",
            &json!({}),
            r#"{"boards":["MAG Solar"]}"#,
            false,
            120,
        );
        log_tool_call_to(
            d.path(),
            "mcp__fizzy__create_card",
            &json!({"board":"MAG Solar","title":"fix panel"}),
            r#"{"number":42}"#,
            false,
            350,
        );
        log_ask_to(
            d.path(),
            "create a card fix panel on MAG Solar",
            "yes",
            1,
            2,
            1200,
            None,
        );

        let out = recall_from(d.path(), None, 10);
        // All three events present, oldest first (tool, tool, ask).
        let list_idx = out.find("list_boards").unwrap();
        let create_idx = out.find("create_card").unwrap();
        let ask_idx = out.find("asked").unwrap();
        assert!(
            list_idx < create_idx,
            "tools should be in call order: {out}"
        );
        assert!(create_idx < ask_idx, "ask should follow its tools: {out}");
    }

    #[test]
    fn recall_query_matching_nothing_returns_empty_sections() {
        let d = tmp();
        log_ask_to(d.path(), "create a card", "yes", 1, 1, 500, None);
        let out = recall_from(d.path(), Some("xyzzy_nothing_matches"), 10);
        assert!(
            out.contains("No history yet") || !out.contains("create a card"),
            "should not contain the unmatched event: {out}"
        );
    }

    #[test]
    fn multiple_lessons_recalled_most_recent_first() {
        let d = tmp();
        remember_to(d.path(), "first lesson").unwrap();
        remember_to(d.path(), "second lesson").unwrap();
        remember_to(d.path(), "third lesson").unwrap();

        let out = recall_from(d.path(), Some("lesson"), 10);
        let first_idx = out.find("first lesson").unwrap();
        let second_idx = out.find("second lesson").unwrap();
        let third_idx = out.find("third lesson").unwrap();
        // grep_context returns newest first.
        assert!(third_idx < second_idx, "newest first: {out}");
        assert!(second_idx < first_idx, "newest first: {out}");
    }

    #[test]
    fn recall_shows_notes_even_without_activity() {
        // remember a lesson without ever logging an ask or tool call.
        let d = tmp();
        remember_to(
            d.path(),
            "MAG Solar has columns: In Progress, Blocked, Testing",
        )
        .unwrap();

        let out = recall_from(d.path(), None, 10);
        assert!(
            out.contains("Memory notes"),
            "notes section should appear: {out}"
        );
        assert!(
            out.contains("MAG Solar has columns"),
            "lesson content: {out}"
        );
    }
}
