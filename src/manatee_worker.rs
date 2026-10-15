//! A Manatee worker process (`fqs manatee-worker`, started by FQS: manatee_pool.rs). It loads
//! libfqs_manatee (FQS_MANATEE_LIB) and answers one request per line on stdin, one answer per line on
//! stdout, so that a crash in Manatee ends this process and not FQS.
//!
//! Requests: {"op": "hello"} | {"op": "query", "corpus", "registry", "cql", "start", "size",
//! "window", "attr"?, "skip_check"?} | {"op": "freq", "corpus", "registry", "cql", "crit", "limit"}.
//! Answers: {"ok": true, "result"} or {"ok": false, "error", "check"?}, each line starting with
//! [`MARK`]: the library may write to stdout itself, and such lines are not answers.
//!
//! A corpus is checked (fqsm_check) before its first query in this worker, unless FQS already saw it
//! pass ("skip_check").

use crate::manatee_lib::ManateeLib;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// The start of every answer line.
pub const MARK: &str = "\u{1e}FQSM ";

pub fn serve() -> Result<()> {
    let path = std::env::var("FQS_MANATEE_LIB").context("FQS_MANATEE_LIB is not set")?;
    let loaded = ManateeLib::load(Path::new(path.trim()));
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    // corpus id → the outcome of its check in this worker (Err: the problem)
    let mut checked: HashMap<String, std::result::Result<(), String>> = HashMap::new();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let answer = match (&loaded, serde_json::from_str::<Value>(&line)) {
            (Err(e), _) => json!({ "ok": false, "error": format!("{e:#}") }),
            (_, Err(e)) => json!({ "ok": false, "error": format!("bad request: {e}") }),
            (Ok(lib), Ok(req)) => match handle(lib, &req, &mut checked) {
                Ok(result) => json!({ "ok": true, "result": result }),
                Err(Failure::Check(problem)) => json!({ "ok": false, "error": problem, "check": false }),
                Err(Failure::Other(e)) => json!({ "ok": false, "error": format!("{e:#}") }),
            },
        };
        writeln!(out, "{MARK}{answer}")?;
        out.flush()?;
    }
    Ok(())
}

enum Failure {
    Check(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        Failure::Other(e)
    }
}

fn s<'a>(req: &'a Value, key: &str) -> &'a str {
    req.get(key).and_then(Value::as_str).unwrap_or("")
}

fn n(req: &Value, key: &str, default: u64) -> u64 {
    req.get(key).and_then(Value::as_u64).unwrap_or(default)
}

fn handle(lib: &ManateeLib, req: &Value, checked: &mut HashMap<String, std::result::Result<(), String>>) -> std::result::Result<Value, Failure> {
    let op = s(req, "op");
    if op == "hello" {
        return Ok(json!({ "version": lib.version, "path": lib.path.display().to_string() }));
    }
    let corpus = s(req, "corpus");
    let registry = PathBuf::from(s(req, "registry"));
    if corpus.is_empty() || registry.as_os_str().is_empty() {
        return Err(anyhow!("corpus and registry are required").into());
    }
    if !req.get("skip_check").and_then(Value::as_bool).unwrap_or(false) {
        let outcome = match checked.get(corpus) {
            Some(o) => o.clone(),
            None => {
                let v = lib.check(corpus, &registry)?;
                let o = if v.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                    Ok(())
                } else {
                    Err(v.get("problem").and_then(Value::as_str).unwrap_or("the corpus check failed").to_string())
                };
                checked.insert(corpus.to_string(), o.clone());
                o
            }
        };
        if let Err(problem) = outcome {
            return Err(Failure::Check(format!("Manatee corpus '{corpus}' does not read as its registry describes: {problem}")));
        }
    }
    match op {
        "query" => {
            let attr = req.get("attr").and_then(Value::as_str).filter(|a| !a.is_empty());
            Ok(lib.query(corpus, &registry, s(req, "cql"), n(req, "start", 0), n(req, "size", 20), n(req, "window", 5) as u32, attr)?)
        }
        "freq" => Ok(lib.freq(corpus, &registry, s(req, "cql"), s(req, "crit"), n(req, "limit", 0))?),
        other => Err(anyhow!("unknown op '{other}'").into()),
    }
}
