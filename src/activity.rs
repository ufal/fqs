//! Activity log (`serve --activity-log FILE`): one JSON object per line, for
//! looking back at what the server did — which queries came in and how they
//! fared, and how the set of warm (open) corpora evolved.
//!
//! Events (`"event"`):
//!   * `query`       — a /query, /run or /fcs request: corpus, endpoint, role / tier,
//!                     user (hashed by default), the query, status, elapsed / queued ms,
//!                     whether the corpus had to be opened for it, and the error if any;
//!   * `warm_open`   — a corpus opened (open_ms, how many are warm now);
//!   * `warm_close`  — a corpus closed: reason `lru` (room for another), `idle`
//!                     (idle TTL); its age, idle time and request count;
//!   * `warm_full`   — no room could be made (every warm corpus in use or busy):
//!                     the map goes over `max_warm` for a while;
//!   * `warm_state`  — every `--activity-state-secs`: the warm corpora and the
//!                     process's memory.
//! `--activity-events` picks the kinds (`queries`, `warm`; default both).
//! Rotation follows the request log (`--log-max-bytes`, `--log-keep-files`).

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserMode {
    /// user names as sent (TEITOK login, KonText user id)
    Plain,
    /// the first 12 hex digits of SHA-256(salt + user): stable within one log, not readable
    Hash,
    /// no user field
    None,
}

impl UserMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "plain" => Ok(Self::Plain),
            "hash" => Ok(Self::Hash),
            "none" | "off" => Ok(Self::None),
            other => Err(format!("--activity-log-users: '{other}' (plain, hash or none)")),
        }
    }
}

pub struct ActivityLog {
    path: PathBuf,
    queries: bool,
    warm: bool,
    users: UserMode,
    salt: String,
    max_bytes: u64,
    keep_files: usize,
    file: Mutex<Option<File>>,
}

impl ActivityLog {
    /// `events`: comma-separated `queries`, `warm` (or `all`).
    pub fn new(path: PathBuf, events: &str, users: UserMode, salt: Option<String>,
               max_bytes: u64, keep_files: usize) -> Result<Self, String> {
        let mut queries = false;
        let mut warm = false;
        for e in events.split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()) {
            match e.as_str() {
                "queries" | "query" => queries = true,
                "warm" => warm = true,
                "all" => {
                    queries = true;
                    warm = true;
                }
                other => return Err(format!("--activity-events: unknown '{other}' (queries, warm, all)")),
            }
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            }
        }
        let f = OpenOptions::new().create(true).append(true).open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // a salt per start unless one is given: hashes are comparable within a run
        let salt = salt.unwrap_or_else(|| format!("{:x}", OffsetDateTime::now_utc().unix_timestamp_nanos()));
        Ok(Self { path, queries, warm, users, salt, max_bytes, keep_files, file: Mutex::new(Some(f)) })
    }

    pub fn logs_queries(&self) -> bool {
        self.queries
    }
    pub fn logs_warm(&self) -> bool {
        self.warm
    }

    pub fn status_json(&self) -> Value {
        json!({
            "path": self.path,
            "queries": self.queries,
            "warm": self.warm,
            "users": match self.users { UserMode::Plain => "plain", UserMode::Hash => "hash", UserMode::None => "none" },
        })
    }

    /// Tail the JSONL activity log and build a summary + recent event list for the admin UI.
    ///
    /// `event_filter`: `interesting` (default — all except `warm_state`), `all`, `query`,
    /// `warm` (`warm_*`), `admin` (`admin_*`), or a concrete event name.
    pub fn admin_overview(
        &self,
        limit: usize,
        event_filter: &str,
        corpus: Option<&str>,
        scan_bytes: u64,
    ) -> Value {
        let limit = limit.clamp(1, 500);
        let scan_bytes = scan_bytes.clamp(64 * 1024, 8 * 1024 * 1024);
        let (lines, file_bytes, truncated_read) = match tail_lines(&self.path, scan_bytes) {
            Ok(t) => t,
            Err(e) => {
                return json!({
                    "ok": false,
                    "enabled": true,
                    "path": self.path,
                    "config": self.status_json(),
                    "error": e,
                });
            }
        };

        let corpus_f = corpus.map(str::trim).filter(|s| !s.is_empty());
        let mut by_event: HashMap<String, u64> = HashMap::new();
        let mut query_total = 0u64;
        let mut query_ok = 0u64;
        let mut query_err = 0u64;
        let mut query_busy = 0u64;
        let mut query_denied = 0u64;
        let mut elapsed_sum = 0u64;
        let mut elapsed_n = 0u64;
        let mut corpus_counts: HashMap<String, u64> = HashMap::new();
        let mut warm_opens = 0u64;
        let mut warm_closes = 0u64;
        let mut close_reasons: HashMap<String, u64> = HashMap::new();
        let mut admin_writes = 0u64;
        let mut last_start: Option<Value> = None;
        let mut matched: Vec<Value> = Vec::new();

        for line in &lines {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let event = v
                .get("event")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if event.is_empty() {
                continue;
            }
            *by_event.entry(event.clone()).or_default() += 1;

            match event.as_str() {
                "query" => {
                    query_total += 1;
                    let status = v.get("status").and_then(Value::as_u64).unwrap_or(0);
                    if (200..300).contains(&status) {
                        query_ok += 1;
                    } else {
                        query_err += 1;
                    }
                    if v.get("busy").and_then(Value::as_bool) == Some(true) {
                        query_busy += 1;
                    }
                    if v.get("denied").and_then(Value::as_bool) == Some(true) {
                        query_denied += 1;
                    }
                    if let Some(ms) = v.get("elapsed_ms").and_then(Value::as_u64) {
                        elapsed_sum += ms;
                        elapsed_n += 1;
                    }
                    if let Some(c) = v.get("corpus").and_then(Value::as_str) {
                        *corpus_counts.entry(c.to_string()).or_default() += 1;
                    }
                }
                "warm_open" => warm_opens += 1,
                "warm_close" => {
                    warm_closes += 1;
                    let reason = v
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string();
                    *close_reasons.entry(reason).or_default() += 1;
                }
                "start" => last_start = Some(v.clone()),
                e if e.starts_with("admin_") => admin_writes += 1,
                _ => {}
            }

            if !event_matches(event_filter, &event) {
                continue;
            }
            if let Some(want) = corpus_f {
                let c = v
                    .get("corpus")
                    .or_else(|| v.get("corpus_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !c.eq_ignore_ascii_case(want) {
                    continue;
                }
            }
            matched.push(v);
        }

        // Newest first for the UI list.
        matched.reverse();
        let truncated_list = matched.len() > limit;
        matched.truncate(limit);

        let mut top_corpora: Vec<(String, u64)> = corpus_counts.into_iter().collect();
        top_corpora.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        top_corpora.truncate(10);

        let mut by_event_sorted: Vec<(String, u64)> = by_event.into_iter().collect();
        by_event_sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        json!({
            "ok": true,
            "enabled": true,
            "path": self.path,
            "config": self.status_json(),
            "window": {
                "file_bytes": file_bytes,
                "bytes_scanned": scan_bytes.min(file_bytes),
                "lines_parsed": lines.len(),
                "truncated_read": truncated_read,
                "event_filter": event_filter,
                "corpus_filter": corpus_f,
            },
            "summary": {
                "by_event": by_event_sorted.into_iter().map(|(k, n)| json!({"event": k, "count": n})).collect::<Vec<_>>(),
                "queries": {
                    "total": query_total,
                    "ok": query_ok,
                    "error": query_err,
                    "busy": query_busy,
                    "denied": query_denied,
                    "avg_elapsed_ms": if elapsed_n > 0 { Some(elapsed_sum / elapsed_n) } else { None },
                },
                "top_corpora": top_corpora.into_iter().map(|(id, n)| json!({"corpus": id, "queries": n})).collect::<Vec<_>>(),
                "warm": {
                    "opens": warm_opens,
                    "closes": warm_closes,
                    "close_reasons": close_reasons,
                },
                "admin_writes": admin_writes,
                "last_start": last_start,
            },
            "events": matched,
            "truncated": truncated_list,
        })
    }
}

fn event_matches(filter: &str, event: &str) -> bool {
    match filter.trim().to_ascii_lowercase().as_str() {
        "" | "interesting" | "default" => event != "warm_state",
        "all" => true,
        "query" | "queries" => event == "query",
        "warm" => event.starts_with("warm_"),
        "admin" => event.starts_with("admin_"),
        "start" => event == "start",
        exact => event.eq_ignore_ascii_case(exact),
    }
}

/// Read up to `max_bytes` from the end of `path`. Returns (lines, file_len, truncated).
fn tail_lines(path: &Path, max_bytes: u64) -> Result<(Vec<String>, u64, bool), String> {
    let mut f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file_bytes = f.metadata().map_err(|e| e.to_string())?.len();
    let truncated = file_bytes > max_bytes;
    let start = file_bytes.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    let mut buf = String::new();
    f.read_to_string(&mut buf).map_err(|e| e.to_string())?;
    let mut lines: Vec<String> = buf.lines().map(|s| s.to_string()).collect();
    if truncated && !lines.is_empty() {
        // First line may be a partial JSON object — drop it.
        lines.remove(0);
    }
    Ok((lines, file_bytes, truncated))
}

impl ActivityLog {
    /// The user as it goes into the log (None = leave the field out).
    pub fn user_field(&self, user: &str) -> Option<String> {
        let u = user.trim();
        if u.is_empty() {
            return None;
        }
        match self.users {
            UserMode::Plain => Some(u.to_string()),
            UserMode::None => None,
            UserMode::Hash => {
                let mut h = Sha256::new();
                h.update(self.salt.as_bytes());
                h.update(b"\x1f");
                h.update(u.as_bytes());
                let d = h.finalize();
                Some(d.iter().take(6).map(|b| format!("{b:02x}")).collect())
            }
        }
    }

    pub fn query(&self, fields: Map<String, Value>) {
        if self.queries {
            self.event("query", fields);
        }
    }

    pub fn warm(&self, event: &str, fields: Map<String, Value>) {
        if self.warm {
            self.event(event, fields);
        }
    }

    /// Any event (always written: `start`, …).
    pub fn event(&self, event: &str, fields: Map<String, Value>) {
        let ts = OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default();
        let mut obj = Map::with_capacity(fields.len() + 2);
        obj.insert("ts".into(), Value::String(ts));
        obj.insert("event".into(), Value::String(event.into()));
        obj.extend(fields);
        let mut line = Value::Object(obj).to_string();
        line.push('\n');
        let mut g = match self.file.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        self.rotate_locked(&mut g);
        if let Some(f) = g.as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn rotate_locked(&self, g: &mut Option<File>) {
        if self.max_bytes == 0 || self.keep_files == 0 {
            return;
        }
        let len = g.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len()).unwrap_or(0);
        if len < self.max_bytes {
            return;
        }
        *g = None;
        let p = self.path.display().to_string();
        let _ = fs::remove_file(format!("{p}.{}", self.keep_files));
        for i in (1..self.keep_files).rev() {
            let _ = fs::rename(format!("{p}.{i}"), format!("{p}.{}", i + 1));
        }
        let _ = fs::rename(&self.path, format!("{p}.1"));
        *g = OpenOptions::new().create(true).append(true).open(&self.path).ok();
    }
}

/// Resident and peak memory of this process, in bytes (None where unknown).
pub fn process_memory() -> (Option<u64>, Option<u64>) {
    #[cfg(target_os = "linux")]
    {
        let mut rss = None;
        let mut hwm = None;
        if let Ok(s) = fs::read_to_string("/proc/self/status") {
            for line in s.lines() {
                let kb = |l: &str| l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).map(|v| v * 1024);
                if line.starts_with("VmRSS:") {
                    rss = kb(line);
                } else if line.starts_with("VmHWM:") {
                    hwm = kb(line);
                }
            }
        }
        (rss, hwm)
    }
    #[cfg(target_os = "macos")]
    {
        // current RSS via `ps` (no extra crate); peak unknown
        let pid = std::process::id().to_string();
        let rss = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|kb| kb * 1024);
        (rss, None)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        (None, None)
    }
}

/// `json!`-like helper: an object from pairs, dropping nulls.
pub fn fields(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
    let mut m = Map::with_capacity(pairs.len());
    for (k, v) in pairs {
        if !v.is_null() {
            m.insert(k.to_string(), v);
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn admin_overview_skips_warm_state_by_default() {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fqs-activity-test-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let mut f = File::create(&p).unwrap();
            writeln!(
                f,
                r#"{{"event":"warm_state","ts":"t0","warm":0}}
{{"event":"query","ts":"t1","corpus":"c1","status":200,"elapsed_ms":10,"query":"[word=\"a\"]"}}
{{"event":"warm_open","ts":"t2","corpus":"c1"}}
{{"event":"admin_scan","ts":"t3","by":"ops"}}"#
            )
            .unwrap();
        }
        let log = ActivityLog::new(p.clone(), "all", UserMode::None, Some("salt".into()), 0, 0)
            .unwrap();
        let report = log.admin_overview(50, "interesting", None, 1024 * 1024);
        let _ = fs::remove_file(&p);
        assert_eq!(report["ok"], true);
        let events = report["events"].as_array().unwrap();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|e| e["event"] != "warm_state"));
        assert_eq!(report["summary"]["queries"]["total"], 1);
        assert_eq!(report["summary"]["warm"]["opens"], 1);
        assert_eq!(report["summary"]["admin_writes"], 1);
    }
}
