//! One status entry per query engine, in the same shape, for `/backends` and (as a summary) `/health`:
//! `{"available", "mode", "version", "error"}`, plus engine details. Monitoring can alert on
//! `available: false`; its `error` says why. File paths only for admins.
//!
//! - pando: the library (`library`) or the CLI (`cli`), from pando_lib's load report;
//! - cqp: the `cqp` binary searches run (CQP_BINARY, else the PATH), as a child process (`process`);
//! - manatee: FQS's native Manatee workers (`native`, FQS_MANATEE_LIB; manatee_pool.rs). KonText's
//!   Python bindings are not this; `/admin/api/backends` lists them apart.

use crate::manatee_pool;
use crate::services::{version_from_cmd, which_bin};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long the cqp check (a process start) is kept.
const CQP_CHECK_TTL: Duration = Duration::from_secs(300);

fn pando(status: &Value, admin: bool) -> Value {
    let mode = status.get("mode").and_then(Value::as_str).unwrap_or("");
    let available = mode == "library" || mode == "cli";
    let mut o = json!({
        "available": available,
        "mode": if mode.is_empty() { Value::Null } else { json!(mode) },
        "version": status.pointer("/engine/version").cloned().unwrap_or(Value::Null),
    });
    if !available {
        const NONE: &str = "neither the pando library nor the CLI was found";
        let detail = status.get("error").and_then(Value::as_str).unwrap_or(NONE);
        o["error"] = json!(if admin { detail } else { NONE });
    }
    if admin {
        o["path"] = status.get("library_path").or_else(|| status.get("cli")).cloned().unwrap_or(Value::Null);
    }
    o
}

/// (binary, version or why not), kept for CQP_CHECK_TTL.
fn cqp_check() -> (Option<String>, Result<String, String>) {
    static KEPT: Mutex<Option<(Instant, Option<String>, Result<String, String>)>> = Mutex::new(None);
    let mut kept = KEPT.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, bin, ver)) = kept.as_ref() {
        if at.elapsed() < CQP_CHECK_TTL {
            return (bin.clone(), ver.clone());
        }
    }
    let env = std::env::var("CQP_BINARY").ok().filter(|s| !s.trim().is_empty());
    let bin = which_bin(&[env.as_deref(), Some("cqp")]);
    let ver = match &bin {
        None => Err(match &env {
            Some(e) => format!("CQP_BINARY ({e}) is not a program"),
            None => "no cqp on the PATH (CQP_BINARY names it)".to_string(),
        }),
        // classic CWB answers -v (a banner with "Version: …"); newer builds also --version
        Some(b) => version_from_cmd(b, &["-v"])
            .or_else(|| version_from_cmd(b, &["--version"]))
            .ok_or_else(|| "cqp does not run (no version from cqp -v)".to_string()),
    };
    *kept = Some((Instant::now(), bin.clone(), ver.clone()));
    (bin, ver)
}

fn cqp(admin: bool) -> Value {
    let (bin, ver) = cqp_check();
    let mut o = json!({ "available": ver.is_ok(), "mode": "process", "version": ver.as_ref().ok() });
    if let Err(e) = &ver {
        // the reasons name paths (CQP_BINARY): only for admins
        o["error"] = json!(if admin { e.as_str() } else { "cqp is not available" });
    }
    if admin {
        o["path"] = json!(bin);
    }
    o
}

fn manatee(admin: bool) -> Value {
    if let Some(pool) = manatee_pool::global() {
        let (workers, running, busy) = pool.worker_counts();
        let mut o = json!({
            "available": true,
            "mode": "native",
            "version": pool.version,
            "workers": workers,
            "workers_running": running,
            "workers_busy": busy,
        });
        if admin {
            o["path"] = json!(pool.lib_path);
        }
        return o;
    }
    let set = std::env::var("FQS_MANATEE_LIB").ok().filter(|s| !s.trim().is_empty());
    json!({
        "available": false,
        "mode": Value::Null,
        "version": Value::Null,
        "error": match set {
            None => "native Manatee is off (FQS_MANATEE_LIB is not set)".to_string(),
            // the reason names the library's path: only for admins
            Some(_) if admin => manatee_pool::start_error().unwrap_or_else(|| "native Manatee did not start".to_string()),
            Some(_) => "native Manatee did not start".to_string(),
        },
    })
}

/// Every engine's entry. `corpora`: current catalogue rows per engine; given, each entry gets its count.
pub fn engines(pando_status: &Value, admin: bool, corpora: Option<&BTreeMap<String, usize>>) -> Value {
    let mut out = Map::new();
    out.insert("pando".into(), pando(pando_status, admin));
    out.insert("cqp".into(), cqp(admin));
    out.insert("manatee".into(), manatee(admin));
    if let Some(counts) = corpora {
        for (name, entry) in out.iter_mut() {
            entry["corpora"] = json!(counts.get(name).copied().unwrap_or(0));
        }
    }
    Value::Object(out)
}

/// `/health`'s summary: whether each engine can run, and how.
pub fn summary(engines: &Value) -> Value {
    let mut out = Map::new();
    if let Some(m) = engines.as_object() {
        for (name, e) in m {
            out.insert(name.clone(), json!({ "available": e["available"], "mode": e["mode"] }));
        }
    }
    Value::Object(out)
}

/// A pando status without file paths (the public `/backends`).
pub fn pando_status_public(status: &Value) -> Value {
    let mut s = status.clone();
    if let Some(o) = s.as_object_mut() {
        for k in ["library_path", "checked", "cli"] {
            o.remove(k);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_and_paths() {
        let lib = json!({"mode": "library", "library_path": "/x/lib.so", "engine": {"version": "0.2.1"}, "error": null});
        let p = pando(&lib, false);
        assert_eq!(p, json!({"available": true, "mode": "library", "version": "0.2.1"}));
        assert_eq!(pando(&lib, true)["path"], "/x/lib.so");
        let none = json!({"mode": "none", "error": "no library"});
        assert_eq!(pando(&none, false)["available"], false);
        assert_eq!(pando(&none, true)["error"], "no library");
        assert_eq!(pando(&none, false)["error"], "neither the pando library nor the CLI was found");
        let mut counts = BTreeMap::new();
        counts.insert("manatee".to_string(), 12);
        let e = engines(&lib, false, Some(&counts));
        assert_eq!(e["manatee"]["corpora"], 12);
        assert_eq!(e["cqp"]["corpora"], 0);
        assert!(e["cqp"].get("path").is_none(), "no paths for non-admins");
        let s = summary(&e);
        assert_eq!(s["pando"], json!({"available": true, "mode": "library"}));
        let pub_ = pando_status_public(&json!({"mode": "library", "library_path": "/x", "checked": ["/x"], "cli": "/y", "api_version": 5}));
        assert_eq!(pub_, json!({"mode": "library", "api_version": 5}));
    }
}
