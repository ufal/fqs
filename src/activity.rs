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
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
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
