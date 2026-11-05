//! Rows already in the catalogue (`--existing FILE`, from `fqs corpora export-json
//! --include-noncurrent`): an imported row only fills the fields the catalogue still has empty,
//! settings key by key, so what was set by hand (or in the FQS admin) is kept.

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::Path;

/// id → row, from an export (an array, or `{"corpora": [...]}`).
pub(crate) fn read_existing(file: &Path) -> Result<HashMap<String, Value>> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let raw: Value = serde_json::from_str(&text).with_context(|| format!("{} is not JSON", file.display()))?;
    let list = raw.get("corpora").cloned().unwrap_or(raw);
    Ok(list
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| e.get("id").and_then(Value::as_str).map(|id| (id.to_string(), e.clone())))
                .collect()
        })
        .unwrap_or_default())
}

fn empty(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
        _ => false,
    }
}

/// The row to upsert (the id, and what the catalogue lacks), and how many fields were kept.
pub(crate) fn merge_existing(row: &Map<String, Value>, old: &Value) -> (Map<String, Value>, usize) {
    let mut out = Map::new();
    out.insert("id".into(), row.get("id").cloned().unwrap_or(Value::Null));
    let mut kept = 0;
    for (k, v) in row {
        if k == "id" || v.is_null() {
            continue;
        }
        let cur = old.get(k);
        match (k.as_str(), cur, v) {
            ("settings", Some(Value::Object(cur)), Value::Object(new)) => {
                let add: Map<String, Value> = new.iter().filter(|(sk, _)| empty(cur.get(*sk))).map(|(a, b)| (a.clone(), b.clone())).collect();
                kept += new.len() - add.len();
                if !add.is_empty() {
                    let mut merged = cur.clone();
                    merged.extend(add);
                    out.insert(k.clone(), Value::Object(merged));
                }
            }
            _ if empty(cur) => {
                out.insert(k.clone(), v.clone());
            }
            _ => kept += 1,
        }
    }
    (out, kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_empty_fields_filled() {
        let row = json!({"id": "x", "label": "New", "project_url": "http://n", "settings": {"description": "d", "access": ["a"]}});
        let old = json!({"id": "x", "label": "Hand-set", "project_url": "", "settings": {"description": "kept", "fcs": {"enabled": true}}});
        let (m, kept) = merge_existing(row.as_object().unwrap(), &old);
        assert_eq!(m.get("label"), None, "set by hand: kept");
        assert_eq!(m["project_url"], "http://n");
        assert_eq!(m["settings"], json!({"description": "kept", "fcs": {"enabled": true}, "access": ["a"]}));
        assert_eq!(kept, 2);
    }
}
