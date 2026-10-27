//! Manatee totals counted in the background (a query's `"total": "async"`, as KonText asks): the first page
//! comes from a walk only to the end of the page, and the exact total is counted by another worker while
//! the user reads it. `GET /status?corpus=&job=` answers how far the count is, as pando-server's does.
//!
//! One count per query at a time (a second request for a query being counted gets the same job). A
//! finished count stays in the pool's totals (manatee_pool.rs: later pages walk only to their end) and its
//! job is answered for [`TOTAL_TTL`].

use crate::manatee_pool::{total_key, Pool, TOTAL_TTL};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

pub const JOB_PREFIX: &str = "manatee-";

#[derive(Clone)]
enum State {
    Counting,
    Done(u64),
    Failed(String),
}

struct Job {
    state: State,
    since: Instant,
}

#[derive(Default)]
struct Jobs {
    by_id: HashMap<String, Job>,
    /// (corpus, registry, query) → the job counting it
    running: HashMap<(String, String, String), String>,
}

fn jobs() -> &'static Mutex<Jobs> {
    static JOBS: OnceLock<Mutex<Jobs>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(Jobs::default()))
}

fn lock() -> std::sync::MutexGuard<'static, Jobs> {
    jobs().lock().unwrap_or_else(|p| p.into_inner())
}

/// A `kwic` request answered at once: with the total when it is known, else with the hits of the page,
/// `"total_exact": false` and the `job_id` of the count.
pub fn kwic(pool: &Arc<Pool>, corpus: &str, req: Value) -> Result<Value> {
    if pool.known_total(corpus, &req).is_some() {
        return pool.kwic(corpus, req);
    }
    let mut page_req = req.clone();
    page_req["op"] = json!("kwic");
    page_req["count"] = json!(false);
    let mut res = pool.call(corpus, page_req)?;
    res["total_exact"] = json!(false);
    res["job_id"] = json!(start_count(pool, corpus, req));
    Ok(res)
}

/// The job counting this query: the running one, or a new one.
fn start_count(pool: &Arc<Pool>, corpus: &str, req: Value) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let key = total_key(corpus, &req);
    let mut j = lock();
    if let Some(id) = j.running.get(&key) {
        return id.clone();
    }
    j.by_id.retain(|_, job| matches!(job.state, State::Counting) || job.since.elapsed() < TOTAL_TTL);
    let id = format!("{JOB_PREFIX}{}", NEXT.fetch_add(1, Ordering::Relaxed));
    j.by_id.insert(id.clone(), Job { state: State::Counting, since: Instant::now() });
    j.running.insert(key.clone(), id.clone());
    drop(j);
    let (pool, corpus, job) = (Arc::clone(pool), corpus.to_string(), id.clone());
    let mut count_req = req;
    count_req["start"] = json!(0);
    count_req["size"] = json!(0);
    std::thread::spawn(move || {
        // the pool's kwic counts every hit (the total is not known) and keeps the total for later pages
        let state = match pool.kwic(&corpus, count_req) {
            Ok(res) => match res.get("total").and_then(Value::as_u64) {
                Some(total) => State::Done(total),
                None => State::Failed("the Manatee count gave no total".to_string()),
            },
            Err(e) => State::Failed(format!("{e:#}")),
        };
        let mut j = lock();
        j.running.remove(&key);
        j.by_id.insert(job, Job { state, since: Instant::now() });
    });
    id
}

/// `GET /status` for a Manatee job: `{finished, total, count}` (as pando-server's), an error, or None
/// for a job this FQS does not know (finished too long ago, or FQS restarted).
pub fn status(job: &str) -> Option<Value> {
    let j = lock();
    let job = j.by_id.get(job)?;
    Some(match &job.state {
        State::Counting => json!({ "ok": true, "result": { "finished": false, "running_ms": job.since.elapsed().as_millis() as u64 } }),
        State::Done(total) => json!({ "ok": true, "result": { "finished": true, "total": total, "count": total, "total_exact": true } }),
        State::Failed(e) => json!({ "ok": false, "error": e }),
    })
}
