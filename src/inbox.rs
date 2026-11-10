//! `fqs corpora inbox`: catalogue rows that a development server (LINDAT: teitok-dev) left in a
//! folder on the shared disk, loaded by the FQS that serves the corpora, which itself only reads
//! that disk. Each `*.json` file (an object or an array, as `upsert-json` takes) is loaded once per
//! content: the catalogue database remembers its name and SHA-256 (table `inbox_applied`), with
//! the outcome, so a refused file is not tried again until it changes. Writers write `NAME.json.part`
//! and rename it, so a half-written file is never read. Every path in a row must lie under one of
//! the allowed roots, so an inbox file cannot point FQS at other files on the server.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

/// The paths a row may name: where FQS reads the corpus from.
const PATH_FIELDS: &[&str] = &["project_root"];
const SETTINGS_PATH_FIELDS: &[&str] = &["index_dir", "index_path", "pando_index", "registry_hint", "manatee_registry", "teitok_project_root"];

pub fn run(
    conn: &Connection,
    dir: &Path,
    allow: &[PathBuf],
    dry_run: bool,
    upsert: impl Fn(&Connection, &str) -> Result<Value>,
) -> Result<Value> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS inbox_applied (
            name TEXT NOT NULL, sha256 TEXT NOT NULL, applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            ok INTEGER NOT NULL, message TEXT, PRIMARY KEY (name, sha256))",
    )?;
    let allow: Vec<PathBuf> = allow.iter().map(|a| normalize(a)).collect();
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .with_context(|| format!("reading the inbox {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".json") && !n.starts_with('.'))
        .collect();
    names.sort();
    let (mut loaded, mut refused, mut seen) = (Vec::new(), Vec::new(), 0usize);
    for name in names {
        let bytes = match std::fs::read(dir.join(&name)) {
            Ok(b) => b,
            Err(e) => {
                refused.push(json!({"file": name, "error": e.to_string()}));
                continue;
            }
        };
        let sha = hex(&Sha256::digest(&bytes));
        let done: Option<i64> = conn
            .query_row("SELECT ok FROM inbox_applied WHERE name = ?1 AND sha256 = ?2", params![name, sha], |r| r.get(0))
            .optional()?;
        if done.is_some() {
            seen += 1;
            continue;
        }
        let outcome = std::str::from_utf8(&bytes)
            .map_err(|e| anyhow::anyhow!("not UTF-8: {e}"))
            .and_then(|text| serde_json::from_str::<Value>(text).map(|v| (text, v)).map_err(|e| anyhow::anyhow!("not JSON: {e}")))
            .and_then(|(text, v)| check_paths(&v, &allow).map(|_| (text, v)))
            .and_then(|(text, v)| if dry_run { Ok(json!({"would_load": ids(&v)})) } else { upsert(conn, text) });
        match outcome {
            Ok(out) => {
                if !dry_run {
                    conn.execute("INSERT INTO inbox_applied (name, sha256, ok, message) VALUES (?1, ?2, 1, NULL)", params![name, sha])?;
                }
                loaded.push(json!({"file": name, "result": out}));
            }
            Err(e) => {
                if !dry_run {
                    conn.execute("INSERT INTO inbox_applied (name, sha256, ok, message) VALUES (?1, ?2, 0, ?3)", params![name, sha, e.to_string()])?;
                }
                refused.push(json!({"file": name, "error": e.to_string()}));
            }
        }
    }
    Ok(json!({"ok": refused.is_empty(), "operation": "corpora_inbox", "dry_run": dry_run,
        "loaded": loaded, "refused": refused, "already_loaded": seen}))
}

fn ids(v: &Value) -> Vec<Value> {
    rows(v).iter().filter_map(|r| r.get("id").cloned()).collect()
}

fn rows(v: &Value) -> Vec<&Value> {
    match v.get("corpora").unwrap_or(v) {
        Value::Array(a) => a.iter().collect(),
        o @ Value::Object(_) => vec![o],
        _ => Vec::new(),
    }
}

/// Every path of every row under an allowed root.
fn check_paths(v: &Value, allow: &[PathBuf]) -> Result<()> {
    let list = rows(v);
    if list.is_empty() {
        anyhow::bail!("no rows (an object or an array of objects)");
    }
    for r in list {
        let id = r.get("id").and_then(Value::as_str).unwrap_or("?");
        let settings = r.get("settings");
        let paths = PATH_FIELDS
            .iter()
            .filter_map(|k| r.get(*k))
            .chain(SETTINGS_PATH_FIELDS.iter().filter_map(|k| settings.and_then(|s| s.get(*k))))
            .filter_map(Value::as_str)
            .filter(|p| !p.trim().is_empty());
        for p in paths {
            let n = normalize(Path::new(p));
            if !n.is_absolute() || !allow.iter().any(|a| n.starts_with(a)) {
                anyhow::bail!("{id}: {p} is not under an allowed root");
            }
        }
    }
    Ok(())
}

/// `.` and `..` resolved without the file system (the path may not exist yet).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsert_ok(_: &Connection, t: &str) -> Result<Value> {
        Ok(json!({"ids": ids(&serde_json::from_str::<Value>(t)?)}))
    }

    #[test]
    fn loads_once_refuses_outside_paths() {
        let dir = std::env::temp_dir().join(format!("fqs-inbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        let allow = vec![PathBuf::from("/opt/DATA-TEITOK")];
        std::fs::write(dir.join("a.json"), r#"[{"id":"ud219","project_root":"/opt/DATA-TEITOK/teitok-main/ud219","settings":{"index_dir":"/opt/DATA-TEITOK/teitok-main/ud219/pando"}}]"#).unwrap();
        std::fs::write(dir.join("b.json"), r#"{"id":"evil","project_root":"/opt/DATA-TEITOK/../../etc"}"#).unwrap();
        std::fs::write(dir.join("c.json.part"), "half").unwrap();
        std::fs::write(dir.join("d.json"), "not json").unwrap();
        let out = run(&conn, &dir, &allow, false, upsert_ok).unwrap();
        assert_eq!(out["loaded"].as_array().unwrap().len(), 1, "{out}");
        assert_eq!(out["loaded"][0]["file"], "a.json");
        let refused: Vec<&str> = out["refused"].as_array().unwrap().iter().map(|r| r["file"].as_str().unwrap()).collect();
        assert_eq!(refused, vec!["b.json", "d.json"], "outside path and bad JSON refused, .part not read");
        // a second run: nothing new
        let out = run(&conn, &dir, &allow, false, upsert_ok).unwrap();
        assert_eq!((out["loaded"].as_array().unwrap().len(), out["already_loaded"].as_u64()), (0, Some(3)));
        // a changed file is loaded again
        std::fs::write(dir.join("a.json"), r#"{"id":"ud219","project_root":"/opt/DATA-TEITOK/teitok-main/ud219","label":"UD 2.19"}"#).unwrap();
        let out = run(&conn, &dir, &allow, false, upsert_ok).unwrap();
        assert_eq!(out["loaded"].as_array().unwrap().len(), 1);
        // dry run records nothing
        std::fs::write(dir.join("e.json"), r#"{"id":"x","project_root":"/opt/DATA-TEITOK/teitok-main/x"}"#).unwrap();
        let out = run(&conn, &dir, &allow, true, upsert_ok).unwrap();
        assert_eq!(out["loaded"][0]["result"]["would_load"][0], "x");
        let out = run(&conn, &dir, &allow, false, upsert_ok).unwrap();
        assert_eq!(out["loaded"][0]["file"], "e.json");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
