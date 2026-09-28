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

use crate::pando_lib::PandoLib;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct HotCorpusConfig {
    pub max_warm: usize,
    pub idle_ttl: Duration,
}

impl Default for HotCorpusConfig {
    fn default() -> Self {
        Self {
            max_warm: 8,
            idle_ttl: Duration::from_secs(30 * 60),
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
}

// SAFETY: ServerApi handles are concurrent-safe for queries; we only move ctx
// pointers under the HCM mutex and call into the lib from spawn_blocking.
unsafe impl Send for WarmEntry {}

pub struct HotCorpusManager {
    lib: Arc<PandoLib>,
    cfg: HotCorpusConfig,
    inner: Mutex<HashMap<String, WarmEntry>>,
}

impl HotCorpusManager {
    pub fn new(lib: Arc<PandoLib>, cfg: HotCorpusConfig) -> Arc<Self> {
        Arc::new(Self {
            lib,
            cfg,
            inner: Mutex::new(HashMap::new()),
        })
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
                })
            })
            .collect();
        let mut out = json!({
            "backend": "pando",
            "engine_build": self.lib.build_string(),
            "api_version": self.lib.api_version(),
            "max_warm": self.cfg.max_warm,
            "idle_ttl_secs": self.cfg.idle_ttl.as_secs(),
            "warm": entries,
        });
        if let Some(bj) = self.lib.build_json() {
            out["engine"] = bj;
        }
        out
    }

    /// Borrow a warm handle for one request. Caller must `release` when done.
    pub fn acquire(&self, corpus_id: &str, index_dir: &Path, preload: bool) -> Result<()> {
        let mut g = self.inner.lock().expect("hcm lock");
        if let Some(e) = g.get_mut(corpus_id) {
            e.last_used = Instant::now();
            e.refs += 1;
            return Ok(());
        }
        // Make room only for a genuinely new corpus (never inline in release()).
        self.make_room_locked(&mut g);
        let ctx = self.lib.open(index_dir, preload)?;
        g.insert(
            corpus_id.to_string(),
            WarmEntry {
                corpus_id: corpus_id.to_string(),
                index_dir: index_dir.to_path_buf(),
                ctx,
                last_used: Instant::now(),
                refs: 1,
            },
        );
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

    /// Evict down to `max_warm`, choosing each victim without touching the
    /// engine: the least-recently-used unreferenced entry (FQS-local
    /// `last_used` only). `busy()` is checked once per entry actually removed —
    /// not once per warm-but-unreferenced entry regardless of whether room is
    /// needed, which is what the old code did on every acquire *and* release.
    /// If the current LRU candidate turns out to still be doing background
    /// work, this stops rather than searching for a second one: acquire() may
    /// then briefly exceed `max_warm` (harmless — a busy corpus would refuse
    /// eviction under any ordering) and the next acquire or sweep tries again.
    fn make_room_locked(&self, g: &mut HashMap<String, WarmEntry>) {
        let max = self.cfg.max_warm.max(1);
        while g.len() >= max {
            let victim = g
                .iter()
                .filter(|(_, e)| e.refs == 0)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(id, _)| id.clone());
            let Some(id) = victim else { break };
            let busy = match g.get(&id) {
                Some(e) => self.lib.busy(e.ctx),
                None => break,
            };
            if busy != 0 {
                break;
            }
            if let Some(e) = g.remove(&id) {
                self.lib.close(e.ctx);
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
            for (_, e) in g.drain() {
                self.lib.close(e.ctx);
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
    ) -> Result<Self> {
        hcm.acquire(corpus_id, index_dir, preload)?;
        Ok(Self {
            hcm,
            corpus_id: corpus_id.to_string(),
        })
    }

    pub fn request(&self, method: &str, path: &str, query: &str, body: &str) -> Result<(i32, Value)> {
        self.hcm
            .request(&self.corpus_id, method, path, query, body)
    }
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
    body.to_string()
}
