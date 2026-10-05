//! On-demand hot-corpus manager (Pando adapter first).
//!
//! Eviction follows wiki/Embedding-the-Server.md:
//! close only when no FQS ref holds the handle and `flexicorp_pando_busy(ctx)==0`,
//! and idle time (from `flexicorp_pando_idle_seconds` when api>=3) exceeds TTL.
//! Count + idle only for Pando — no RSS budget.
//!
//! Two eviction paths, deliberately different costs:
//!   * `acquire()` only makes room when it is about to open a corpus that isn't
//!     already warm and the map is already at `max_warm`; picking each victim
//!     uses only the FQS-local `last_used` clock (no engine call), and the
//!     engine's `busy()` is called once per candidate actually removed — not
//!     once per warm-but-unreferenced entry regardless of whether room is
//!     needed at all.
//!   * idle-TTL eviction (a corpus nobody has touched in a while, even though the
//!     map is under budget) runs on a timer (`spawn_idle_sweeper`), not inline in
//!     every request. `busy()` on the pando side locks its job manager and copies
//!     every cached job just to count the running ones, so it is not free, and
//!     under the old code every acquire *and* every release called it for every
//!     warm-but-unreferenced corpus, while holding the one mutex every corpus's
//!     request path shares. The FQS-local idle check (cheap) still gates the
//!     engine check (not cheap) inside the sweep itself, so a sweep tick over N
//!     warm corpora that are all actively in use costs N `Instant::elapsed()`
//!     calls and zero engine calls.

use crate::activity::{fields, process_memory, ActivityLog};
use crate::pando_lib::PandoLib;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

thread_local! {
    /// Set by `acquire` on this thread: Some(ms) when it opened the corpus.
    static LAST_OPEN_MS: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Run `f` (one request's engine work, on a blocking thread) and report whether
/// it had to open its corpus, and how long that took (for the activity log).
pub fn track_open<T>(f: impl FnOnce() -> T) -> (T, Option<u64>) {
    LAST_OPEN_MS.with(|c| c.set(None));
    let r = f();
    (r, LAST_OPEN_MS.with(|c| c.take()))
}

#[derive(Clone, Debug)]
pub struct HotCorpusConfig {
    pub max_warm: usize,
    pub idle_ttl: Duration,
    /// Pando server options for every handle (limits by tier, sessions, …;
    /// `flexicorp_pando_open_opts`, api_version >= 4). None = the defaults.
    pub open_options: Option<String>,
}

impl Default for HotCorpusConfig {
    fn default() -> Self {
        Self {
            max_warm: 8,
            idle_ttl: Duration::from_secs(30 * 60),
            open_options: None,
        }
    }
}

struct WarmEntry {
    corpus_id: String,
    index_dir: PathBuf,
    ctx: *mut std::os::raw::c_void,
    last_used: Instant,
    /// In-flight FQS request refs (spawn_blocking / HotGuard).
    refs: usize,
    opened_at: Instant,
    /// Requests served since it was opened.
    requests: u64,
}

// SAFETY: ServerApi handles are concurrent-safe for queries; we only move ctx
// pointers under the HCM mutex and call into the lib from spawn_blocking.
unsafe impl Send for WarmEntry {}

pub struct HotCorpusManager {
    lib: Arc<PandoLib>,
    cfg: HotCorpusConfig,
    inner: Mutex<HashMap<String, WarmEntry>>,
    activity: OnceLock<Arc<ActivityLog>>,
}

impl HotCorpusManager {
    pub fn new(lib: Arc<PandoLib>, cfg: HotCorpusConfig) -> Arc<Self> {
        Arc::new(Self {
            lib,
            cfg,
            inner: Mutex::new(HashMap::new()),
            activity: OnceLock::new(),
        })
    }

    /// Log opens / closes / snapshots to `log` (`serve --activity-log`).
    pub fn set_activity(&self, log: Arc<ActivityLog>) {
        let _ = self.activity.set(log);
    }

    fn log_warm(&self, event: &str, f: Vec<(&str, Value)>) {
        if let Some(a) = self.activity.get() {
            a.warm(event, fields(f));
        }
    }

    fn log_close(&self, e: &WarmEntry, reason: &str, engine_idle: Option<f64>, warm_after: usize) {
        self.log_warm("warm_close", vec![
            ("corpus", json!(e.corpus_id)),
            ("reason", json!(reason)),
            ("age_secs", json!(e.opened_at.elapsed().as_secs())),
            ("idle_secs", json!(e.last_used.elapsed().as_secs())),
            ("engine_idle_secs", engine_idle.filter(|v| *v < 1e12).map(|v| json!(v.round())).unwrap_or(Value::Null)),
            ("requests", json!(e.requests)),
            ("warm", json!(warm_after)),
        ]);
    }

    /// A `warm_state` record: the warm corpora and the process's memory.
    pub fn log_state(&self) {
        let Some(a) = self.activity.get() else { return };
        if !a.logs_warm() {
            return;
        }
        let entries: Vec<Value> = {
            let g = self.inner.lock().expect("hcm lock");
            let mut v: Vec<&WarmEntry> = g.values().collect();
            v.sort_by_key(|e| e.last_used);
            v.iter().rev().map(|e| json!({
                "corpus": e.corpus_id,
                "refs": e.refs,
                "age_secs": e.opened_at.elapsed().as_secs(),
                "idle_secs": e.last_used.elapsed().as_secs(),
                "requests": e.requests,
            })).collect()
        };
        let (rss, peak) = process_memory();
        a.warm("warm_state", fields(vec![
            ("warm", json!(entries.len())),
            ("max_warm", json!(self.cfg.max_warm)),
            ("corpora", Value::Array(entries)),
            ("rss_bytes", rss.map(|v| json!(v)).unwrap_or(Value::Null)),
            ("peak_rss_bytes", peak.map(|v| json!(v)).unwrap_or(Value::Null)),
        ]));
    }

    /// Log a `warm_state` record every `interval` (the activity log's snapshots).
    pub fn spawn_state_logger(self: &Arc<Self>, interval: Duration) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let t = Arc::clone(&this);
                let _ = tokio::task::spawn_blocking(move || t.log_state()).await;
            }
        });
    }

    pub fn lib(&self) -> &Arc<PandoLib> {
        &self.lib
    }

    pub fn status_json(&self) -> Value {
        let g = self.inner.lock().expect("hcm lock");
        // Admin/monitoring only (not on any request path): the per-entry engine
        // calls here are fine even though they're the same ones the hot paths
        // below deliberately avoid doing on every acquire/release.
        let entries: Vec<Value> = g
            .values()
            .map(|e| {
                let busy = self.lib.busy(e.ctx);
                let idle = self.lib.idle_seconds(e.ctx);
                json!({
                    "corpus_id": e.corpus_id,
                    "index_dir": e.index_dir.display().to_string(),
                    "refs": e.refs,
                    "busy": busy,
                    "idle_secs": idle,
                    "fqs_idle_secs": e.last_used.elapsed().as_secs(),
                    "age_secs": e.opened_at.elapsed().as_secs(),
                    "requests": e.requests,
                })
            })
            .collect();
        let mut out = json!({
            "backend": "pando",
            "engine_build": self.lib.build_string(),
            "api_version": self.lib.api_version(),
            "max_warm": self.cfg.max_warm,
            "engine_options": self.cfg.open_options.as_deref()
                .and_then(|o| serde_json::from_str::<Value>(o).ok()),
            "engine_options_applied": self.cfg.open_options.is_none() || self.lib.has_open_opts(),
            "idle_ttl_secs": self.cfg.idle_ttl.as_secs(),
            "warm": entries,
        });
        if let Some(bj) = self.lib.build_json() {
            out["engine"] = bj;
        }
        out
    }

    /// Borrow a warm handle for one request. Caller must `release` when done.
    /// `open_options`: engine options for this corpus when it has to be opened
    /// (None = the manager's default). A warm handle keeps the options it was opened with.
    pub fn acquire(&self, corpus_id: &str, index_dir: &Path, preload: bool,
                   open_options: Option<&str>) -> Result<()> {
        let mut g = self.inner.lock().expect("hcm lock");
        if let Some(e) = g.get_mut(corpus_id) {
            e.last_used = Instant::now();
            e.refs += 1;
            e.requests += 1;
            return Ok(());
        }
        // Make room only for a genuinely new corpus (never inline in release()).
        self.make_room_locked(&mut g, corpus_id);
        let opts = open_options.or(self.cfg.open_options.as_deref());
        let t0 = Instant::now();
        let ctx = match self.lib.open_with(index_dir, preload, opts) {
            Ok(c) => c,
            Err(err) => {
                self.log_warm("warm_open_failed", vec![
                    ("corpus", json!(corpus_id)),
                    ("error", json!(err.to_string())),
                    ("warm", json!(g.len())),
                ]);
                return Err(err);
            }
        };
        let open_ms = t0.elapsed().as_millis() as u64;
        LAST_OPEN_MS.with(|c| c.set(Some(open_ms)));
        let now = Instant::now();
        g.insert(
            corpus_id.to_string(),
            WarmEntry {
                corpus_id: corpus_id.to_string(),
                index_dir: index_dir.to_path_buf(),
                ctx,
                last_used: now,
                refs: 1,
                opened_at: now,
                requests: 1,
            },
        );
        self.log_warm("warm_open", vec![
            ("corpus", json!(corpus_id)),
            ("open_ms", json!(open_ms)),
            ("warm", json!(g.len())),
            ("max_warm", json!(self.cfg.max_warm)),
        ]);
        Ok(())
    }

    pub fn release(&self, corpus_id: &str) {
        let mut g = self.inner.lock().expect("hcm lock");
        if let Some(e) = g.get_mut(corpus_id) {
            e.refs = e.refs.saturating_sub(1);
            e.last_used = Instant::now();
        }
        // No eviction sweep here: idle-TTL eviction is the timer's job
        // (spawn_idle_sweeper), and over-budget eviction only needs to run when
        // acquire() is about to open a corpus that needs the room.
    }

    pub fn request(
        &self,
        corpus_id: &str,
        method: &str,
        path: &str,
        query: &str,
        body: &str,
    ) -> Result<(i32, Value)> {
        let ctx = {
            let g = self.inner.lock().expect("hcm lock");
            let e = g
                .get(corpus_id)
                .ok_or_else(|| anyhow!("corpus '{corpus_id}' is not warm (acquire first)"))?;
            e.ctx
        };
        self.lib.request(ctx, method, path, query, body)
    }

    /// TEITOK XML around corpus spans from the project's xidx (see PandoLib::xidx_fragments).
    pub fn xidx_fragments(
        &self,
        corpus_id: &str,
        project_root: &Path,
        spans: &[(i64, i64)],
        context_scope: &str,
        context: i32,
    ) -> Result<Vec<Option<(Option<String>, String)>>> {
        let ctx = {
            let g = self.inner.lock().expect("hcm lock");
            let e = g
                .get(corpus_id)
                .ok_or_else(|| anyhow!("corpus '{corpus_id}' is not warm (acquire first)"))?;
            e.ctx
        };
        self.lib.xidx_fragments(ctx, project_root, spans, context_scope, context)
    }

    /// Evict down to `max_warm`, choosing each victim without touching the
    /// engine: the least-recently-used unreferenced entry (FQS-local
    /// `last_used` only). `busy()` is checked once per entry actually removed —
    /// not once per warm-but-unreferenced entry regardless of whether room is
    /// needed, which is what the old code did on every acquire *and* release.
    /// If the current LRU candidate turns out to still be doing background
    /// work, this stops rather than searching for a second one: acquire() may
    /// then briefly exceed `max_warm` (harmless — a busy corpus would refuse
    /// eviction under any ordering) and the next acquire or sweep tries again.
    fn make_room_locked(&self, g: &mut HashMap<String, WarmEntry>, for_corpus: &str) {
        let max = self.cfg.max_warm.max(1);
        while g.len() >= max {
            let victim = g
                .iter()
                .filter(|(_, e)| e.refs == 0)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(id, _)| id.clone());
            let Some(id) = victim else {
                self.log_warm("warm_full", vec![
                    ("corpus", json!(for_corpus)),
                    ("reason", json!("in_use")),
                    ("warm", json!(g.len())),
                    ("max_warm", json!(max)),
                ]);
                break;
            };
            let busy = match g.get(&id) {
                Some(e) => self.lib.busy(e.ctx),
                None => break,
            };
            if busy != 0 {
                self.log_warm("warm_full", vec![
                    ("corpus", json!(for_corpus)),
                    ("reason", json!("busy")),
                    ("busy_corpus", json!(id)),
                    ("warm", json!(g.len())),
                    ("max_warm", json!(max)),
                ]);
                break;
            }
            if let Some(e) = g.remove(&id) {
                self.lib.close(e.ctx);
                self.log_close(&e, "lru", None, g.len());
            }
        }
    }

    /// Idle-TTL eviction: a corpus under budget but not touched in `idle_ttl`.
    /// Called periodically (see `spawn_idle_sweeper`), never inline in a request.
    /// The FQS-local `last_used` filter runs first and costs nothing; the engine
    /// is asked about only the entries that already look idle from here, so a
    /// tick over corpora that are all in active use makes zero engine calls.
    fn sweep_idle_locked(&self, g: &mut HashMap<String, WarmEntry>) {
        let ttl = self.cfg.idle_ttl;
        let candidates: Vec<String> = g
            .iter()
            .filter(|(_, e)| e.refs == 0 && e.last_used.elapsed() >= ttl)
            .map(|(id, _)| id.clone())
            .collect();
        for id in candidates {
            let (busy, engine_idle) = match g.get(&id) {
                Some(e) => (self.lib.busy(e.ctx), self.lib.idle_seconds(e.ctx)),
                None => continue,
            };
            if busy != 0 {
                continue;
            }
            if Duration::from_secs_f64(engine_idle.max(0.0)) < ttl {
                continue;
            }
            if let Some(e) = g.remove(&id) {
                self.lib.close(e.ctx);
                self.log_close(&e, "idle", Some(engine_idle), g.len());
            }
        }
    }

    /// Spawn the periodic idle-TTL sweep. Call once, after construction, from an
    /// async context (e.g. right after `HotCorpusManager::new` in `main`). The
    /// sweep itself runs on the blocking pool, since it makes engine calls.
    pub fn spawn_idle_sweeper(self: &Arc<Self>, interval: Duration) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let sweep_this = Arc::clone(&this);
                let _ = tokio::task::spawn_blocking(move || {
                    let mut g = sweep_this.inner.lock().expect("hcm lock");
                    sweep_this.sweep_idle_locked(&mut g);
                })
                .await;
            }
        });
    }
}

impl Drop for HotCorpusManager {
    fn drop(&mut self) {
        if let Ok(mut g) = self.inner.lock() {
            let entries: Vec<WarmEntry> = g.drain().map(|(_, e)| e).collect();
            let mut left = entries.len();
            for e in entries {
                self.lib.close(e.ctx);
                left -= 1;
                self.log_close(&e, "shutdown", None, left);
            }
        }
    }
}

/// Guard that releases the HCM ref on drop.
pub struct HotGuard {
    hcm: Arc<HotCorpusManager>,
    corpus_id: String,
}

impl HotGuard {
    pub fn acquire(
        hcm: Arc<HotCorpusManager>,
        corpus_id: &str,
        index_dir: &Path,
        preload: bool,
        open_options: Option<&str>,
    ) -> Result<Self> {
        hcm.acquire(corpus_id, index_dir, preload, open_options)?;
        Ok(Self {
            hcm,
            corpus_id: corpus_id.to_string(),
        })
    }

    pub fn request(&self, method: &str, path: &str, query: &str, body: &str) -> Result<(i32, Value)> {
        self.hcm
            .request(&self.corpus_id, method, path, query, body)
    }

    pub fn xidx_fragments(
        &self,
        project_root: &Path,
        spans: &[(i64, i64)],
        context_scope: &str,
        context: i32,
    ) -> Result<Vec<Option<(Option<String>, String)>>> {
        self.hcm.xidx_fragments(&self.corpus_id, project_root, spans, context_scope, context)
    }
}

/// Real TEITOK XML for the hits of a pando-server /query answer (and both sides of its
/// aligned pairs), from the project's xidx: `fragment` / `context_xml` / `context_data`,
/// as flexicorp-pando gives them, so KWIC rows show the XML and highlight by token id.
/// Leaves hits that already carry a fragment (synthetic "fragment": true mode) alone.
pub fn add_xidx_fragments(
    guard: &HotGuard,
    project_root: &Path,
    engine: &mut Value,
    context_scope: &str,
    context: i32,
) -> Result<usize> {
    let Some(result) = engine.get_mut("result") else { return Ok(0) };
    // the hits to fill, as JSON pointers below `result`
    let mut targets: Vec<String> = Vec::new();
    if let Some(hits) = result.get("hits").and_then(Value::as_array) {
        for i in 0..hits.len() {
            targets.push(format!("/hits/{i}"));
        }
    }
    if let Some(pairs) = result.get("pairs").and_then(Value::as_array) {
        for i in 0..pairs.len() {
            targets.push(format!("/pairs/{i}/source"));
            targets.push(format!("/pairs/{i}/target"));
        }
    }
    let mut spans = Vec::new();
    let mut slots = Vec::new();
    for t in &targets {
        let Some(h) = result.pointer(t) else { continue };
        if h.get("fragment").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
            continue;
        }
        let (Some(a), Some(b)) = (
            h.get("match_start").and_then(Value::as_i64),
            h.get("match_end").and_then(Value::as_i64),
        ) else {
            continue;
        };
        spans.push((a, b));
        slots.push(t.clone());
    }
    if spans.is_empty() {
        return Ok(0);
    }
    let frags = guard.xidx_fragments(project_root, &spans, context_scope, context)?;
    let mut n = 0;
    for (slot, f) in slots.iter().zip(frags) {
        let (Some((doc, xml)), Some(h)) = (f, result.pointer_mut(slot)) else { continue };
        let Some(o) = h.as_object_mut() else { continue };
        o.insert("fragment".into(), Value::String(xml.clone()));
        o.insert("context_xml".into(), Value::String(xml.clone()));
        o.insert("context_data".into(), Value::String(xml));
        if let Some(d) = doc {
            o.entry("doc_xml").or_insert(Value::String(d));
        }
        n += 1;
    }
    Ok(n)
}

impl Drop for HotGuard {
    fn drop(&mut self) {
        self.hcm.release(&self.corpus_id);
    }
}

pub fn wrap_pando_server_as_fqs_raw(payload: Value) -> Value {
    // TEITOK Path B unwraps raw.done.result; pando-server returns {ok, result:{…}}.
    let result = payload.get("result").cloned().unwrap_or(payload.clone());
    json!({
        "ok": payload.get("ok").and_then(|v| v.as_bool()).unwrap_or(true),
        "backend": "pando",
        "operation": "query",
        "done": {
            "backend": "pando",
            "operation": "query",
            "result": result,
            "warnings": [],
            "errors": []
        },
        "engine": payload,
    })
}

pub fn pando_query_body(
    query: &str,
    start: u32,
    size: u32,
    window: Option<u32>,
    sentence: bool,
    total: &str,
    extra: &serde_json::Map<String, Value>,
) -> String {
    let mut body = json!({
        "query": query,
        "offset": start,
        "limit": size,
        "context": window.unwrap_or(5),
        "sentence": sentence,
        "total": if total == "async" {
            Value::String("async".into())
        } else if total == "true" {
            Value::Bool(true)
        } else {
            Value::Bool(false)
        },
    });
    if total == "true" {
        body["max_total"] = json!(0);
    }
    if let Some(o) = body.as_object_mut() {
        for (k, v) in extra {
            o.insert(k.clone(), v.clone());
        }
    }
    body.to_string()
}
