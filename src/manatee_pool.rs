//! Native Manatee for FQS, out of process: a few worker processes (`fqs manatee-worker`,
//! manatee_worker.rs) that load libfqs_manatee, so that a crash in Manatee (a corpus read wrongly,
//! SIGBUS/SIGSEGV) costs one query, not the server. FQS starts the workers, restarts one that ended,
//! stops a query that runs too long (by ending its worker), and stops querying a corpus that ended a
//! worker twice in a short time (for a while, with a message saying why).
//!
//! FQS_MANATEE_LIB: the library (without it, no native Manatee, as before);
//! FQS_MANATEE_WORKERS: the number of workers (default 2; also how many Manatee queries run at once);
//! FQS_MANATEE_TIMEOUT_SECS: the longest a query may run (default 300).

use crate::manatee_worker::MARK;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Two crashes on a corpus within this time: the corpus is not queried for this long.
const QUARANTINE: Duration = Duration::from_secs(600);
const CRASHES_TO_QUARANTINE: u32 = 2;

struct Worker {
    child: Child,
    stdin: ChildStdin,
    answers: Receiver<String>,
}

pub struct Pool {
    pub lib_path: String,
    pub version: String,
    workers: Vec<Mutex<Option<Worker>>>,
    next: AtomicUsize,
    timeout: Duration,
    /// corpora whose check passed (later workers skip it)
    checked: Mutex<HashSet<String>>,
    /// corpus → (crashes, since)
    crashes: Mutex<HashMap<String, (u32, Instant)>>,
}

/// The pool, started once (None without FQS_MANATEE_LIB, or when a worker cannot load the library:
/// the reason is logged and FQS runs without native Manatee).
pub fn global() -> Option<Arc<Pool>> {
    static POOL: OnceLock<Option<Arc<Pool>>> = OnceLock::new();
    POOL.get_or_init(|| {
        let lib = std::env::var("FQS_MANATEE_LIB").ok().map(|p| p.trim().to_string()).filter(|p| !p.is_empty())?;
        match Pool::start(&lib) {
            Ok(pool) => {
                eprintln!("[fqs] native Manatee: {} ({}), {} worker processes", pool.version, pool.lib_path, pool.workers.len());
                Some(Arc::new(pool))
            }
            Err(e) => {
                eprintln!("[fqs] FQS_MANATEE_LIB: {e:#}; native Manatee is off");
                None
            }
        }
    })
    .clone()
}

fn env_num(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).filter(|v| *v > 0).unwrap_or(default)
}

/// How a worker ended, in words ("signal 10 (SIGBUS)", "exit status 1").
fn ended(child: &mut Child) -> String {
    match child.wait() {
        Ok(status) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    const SIGBUS: i32 = if cfg!(target_os = "linux") { 7 } else { 10 };
                    let name = match sig {
                        4 => " (SIGILL)",
                        6 => " (SIGABRT)",
                        9 => " (SIGKILL)",
                        11 => " (SIGSEGV)",
                        s if s == SIGBUS => " (SIGBUS)",
                        _ => "",
                    };
                    return format!("signal {sig}{name}");
                }
            }
            status.to_string()
        }
        Err(e) => format!("unknown ({e})"),
    }
}

impl Worker {
    fn spawn() -> Result<Worker> {
        // the running binary, also after an upgrade replaced the file (Linux: /proc/self/exe), so a
        // worker always speaks this FQS's protocol
        let proc_exe = std::path::Path::new("/proc/self/exe");
        let exe = if proc_exe.exists() { proc_exe.to_path_buf() } else { std::env::current_exe().context("the FQS binary")? };
        let mut child = Command::new(exe)
            .arg("manatee-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("starting a Manatee worker")?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("worker stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("worker stdout"))?;
        let (tx, answers) = mpsc::channel();
        // answers go to the channel; anything else the library prints goes to the log
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                match line.strip_prefix(MARK) {
                    Some(answer) => {
                        if tx.send(answer.to_string()).is_err() {
                            break;
                        }
                    }
                    None => eprintln!("[fqs manatee-worker] {line}"),
                }
            }
        });
        Ok(Worker { child, stdin, answers })
    }

    fn ask(&mut self, req: &Value, timeout: Duration) -> std::result::Result<Value, Ended> {
        let line = format!("{req}\n");
        if self.stdin.write_all(line.as_bytes()).and_then(|_| self.stdin.flush()).is_err() {
            return Err(Ended::Crashed(ended(&mut self.child)));
        }
        match self.answers.recv_timeout(timeout) {
            Ok(answer) => serde_json::from_str(&answer).map_err(|e| Ended::Garbled(e.to_string())),
            Err(RecvTimeoutError::Timeout) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                Err(Ended::TimedOut)
            }
            Err(RecvTimeoutError::Disconnected) => Err(Ended::Crashed(ended(&mut self.child))),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Why a worker did not answer (it is gone afterwards).
enum Ended {
    Crashed(String),
    TimedOut,
    Garbled(String),
}

impl Pool {
    fn start(lib_path: &str) -> Result<Pool> {
        let n = env_num("FQS_MANATEE_WORKERS", 2) as usize;
        let timeout = Duration::from_secs(env_num("FQS_MANATEE_TIMEOUT_SECS", 300));
        // the first worker proves the library loads (all its symbols resolve) before FQS relies on it
        let mut first = Worker::spawn()?;
        let hello = match first.ask(&json!({ "op": "hello" }), Duration::from_secs(30)) {
            Ok(v) => v,
            Err(Ended::Crashed(how)) => bail!("the Manatee worker ended at start ({how})"),
            Err(Ended::TimedOut) => bail!("the Manatee worker did not answer at start"),
            Err(Ended::Garbled(e)) => bail!("the Manatee worker answered garbage at start: {e}"),
        };
        if hello.get("ok").and_then(Value::as_bool) != Some(true) {
            bail!("{}", hello.get("error").and_then(Value::as_str).unwrap_or("the Manatee worker could not load the library"));
        }
        let version = hello.pointer("/result/version").and_then(Value::as_str).unwrap_or("fqs-manatee").to_string();
        let mut workers = vec![Mutex::new(Some(first))];
        workers.extend((1..n.max(1)).map(|_| Mutex::new(None)));   // the others start on first use
        Ok(Pool {
            lib_path: lib_path.to_string(),
            version,
            workers,
            next: AtomicUsize::new(0),
            timeout,
            checked: Mutex::new(HashSet::new()),
            crashes: Mutex::new(HashMap::new()),
        })
    }

    /// A free worker if there is one, else wait for the next in turn.
    fn worker(&self) -> std::sync::MutexGuard<'_, Option<Worker>> {
        let n = self.workers.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        for i in 0..n {
            if let Ok(g) = self.workers[(start + i) % n].try_lock() {
                return g;
            }
        }
        let slot = &self.workers[start % n];
        slot.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn quarantined(&self, corpus: &str) -> Option<Duration> {
        let mut map = self.crashes.lock().unwrap_or_else(|p| p.into_inner());
        let (count, since) = *map.get(corpus)?;
        let age = since.elapsed();
        if age > QUARANTINE {
            map.remove(corpus);
            return None;
        }
        (count >= CRASHES_TO_QUARANTINE).then(|| QUARANTINE - age)
    }

    fn crashed(&self, corpus: &str) {
        let mut map = self.crashes.lock().unwrap_or_else(|p| p.into_inner());
        let e = map.entry(corpus.to_string()).or_insert((0, Instant::now()));
        if e.1.elapsed() > QUARANTINE {
            *e = (0, Instant::now());
        }
        e.0 += 1;
    }

    /// One request for `corpus` ({"op": "query" | "freq", "registry", …}): the worker's result, or an
    /// error that says what happened (a crash or a time-out ends only that worker).
    pub fn call(&self, corpus: &str, mut req: Value) -> Result<Value> {
        if let Some(left) = self.quarantined(corpus) {
            bail!(
                "Manatee corpus '{corpus}' ended a Manatee worker repeatedly; it is not queried for another {} minutes (check the corpus and its registry, e.g. in KonText)",
                left.as_secs() / 60 + 1
            );
        }
        req["corpus"] = json!(corpus);
        if self.checked.lock().map(|c| c.contains(corpus)).unwrap_or(false) {
            req["skip_check"] = json!(true);
        }
        let mut slot = self.worker();
        // a worker that ended while idle is replaced first (not this query's doing, nor its corpus's)
        if let Some(w) = slot.as_mut() {
            if let Ok(Some(status)) = w.child.try_wait() {
                eprintln!("[fqs] Manatee worker ended while idle ({status}); starting a new one");
                *slot = None;
            }
        }
        if slot.is_none() {
            *slot = Some(Worker::spawn()?);
        }
        let worker = slot.as_mut().expect("a worker");
        match worker.ask(&req, self.timeout) {
            Ok(answer) => {
                if answer.get("ok").and_then(Value::as_bool) == Some(true) {
                    if let Ok(mut c) = self.checked.lock() {
                        c.insert(corpus.to_string());
                    }
                    return Ok(answer.get("result").cloned().unwrap_or(Value::Null));
                }
                bail!("{}", answer.get("error").and_then(Value::as_str).unwrap_or("Manatee error"))
            }
            Err(how) => {
                *slot = None;   // a new worker on the next use
                match how {
                    Ended::Crashed(how) => {
                        self.crashed(corpus);
                        eprintln!("[fqs] Manatee worker ended ({how}) on corpus '{corpus}': {}", req.get("cql").and_then(Value::as_str).unwrap_or(""));
                        bail!("Manatee stopped on this query in corpus '{corpus}' ({how}); the corpus may not read as its registry describes. FQS keeps running.")
                    }
                    Ended::TimedOut => bail!("the Manatee query took longer than {} s and was stopped", self.timeout.as_secs()),
                    Ended::Garbled(e) => bail!("the Manatee worker answered garbage ({e})"),
                }
            }
        }
    }
}
