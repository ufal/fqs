//! Parallel Manatee corpora in `/query` (dev/MANATEE-NATIVE.md, "Parallel corpora"). A Manatee
//! parallel corpus is one corpus per language, each naming the others in its registry (`ALIGNED`);
//! libfqs_manatee (0.5) gives per hit the aligned segment of each (fqs_manatee_aligned.cc). This
//! module turns the request's `aligned` into the worker's, and the library's per-hit entries into the
//! answer's. The filter (`[…] within CORPUS: […]`, KonText's "aligned with") is Manatee CQL as it is
//! and needs nothing here.
//!
//! Request: `"aligned": true | "*"` (all the corpus's aligned corpora) | `"par_en"` / `"a,b"` /
//! `["a", "b"]` (registry names, as in `ALIGNED` and in `within NAME:`).
//! Answer (additive): `result.aligned_corpora: [names]`, and per hit `aligned: [{"corpus", "pos",
//! "end", "text", "tokens": [{"pos", "word", …}], "match"?: {"pos", "end"}}]`, one entry per aligned
//! corpus in that order; `pos`/`end` null, `text` "" and `tokens` empty when the hit has no
//! counterpart there.

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

/// The worker's `aligned` for a request's (None: not asked). Anything but true, a name list or `*` is
/// refused, so a typo does not silently give no aligned segments.
pub fn request(v: Option<&Value>) -> Result<Option<Value>> {
    let Some(v) = v else { return Ok(None) };
    let ok_name = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "_-./*".contains(c));
    Ok(match v {
        Value::Null | Value::Bool(false) => None,
        Value::Bool(true) => Some(json!("*")),
        Value::String(s) => {
            let names: Vec<&str> = s.split(',').map(str::trim).filter(|x| !x.is_empty()).collect();
            if names.is_empty() {
                None
            } else if let Some(bad) = names.iter().find(|n| !ok_name(n)) {
                bail!("aligned: '{bad}' is not a corpus name (the registry names of the aligned corpora, or true for all)");
            } else {
                Some(json!(names.join(",")))
            }
        }
        Value::Array(a) => {
            let mut names = Vec::new();
            for x in a {
                match x.as_str().map(str::trim) {
                    Some(n) if ok_name(n) => names.push(n.to_string()),
                    _ => bail!("aligned: {x} is not a corpus name (the registry names of the aligned corpora, or true for all)"),
                }
            }
            (!names.is_empty()).then(|| json!(names.join(",")))
        }
        other => bail!("aligned: expected true, a corpus name or a list of names, not {other}"),
    })
}

/// A hit's aligned segments in the answer's shape, from the library's (`attrs`: the names of the
/// token values, `word` first as in the hit's own tokens).
pub fn hit(lib_hit: &Value, attrs: &[String]) -> Option<Value> {
    let entries = lib_hit.get("aligned")?.as_array()?;
    Some(Value::Array(
        entries
            .iter()
            .map(|e| {
                let pos = e.get("pos").and_then(Value::as_i64);
                let toks: Vec<Value> = e
                    .get("tokens")
                    .and_then(Value::as_array)
                    .map(|ts| {
                        ts.iter()
                            .enumerate()
                            .map(|(i, t)| {
                                let vals = t.as_array().cloned().unwrap_or_default();
                                let mut o = Map::new();
                                o.insert("pos".into(), json!(pos.unwrap_or(0) + i as i64));
                                o.insert("word".into(), vals.first().cloned().unwrap_or(json!("")));
                                for (name, v) in attrs.iter().zip(vals.iter()).skip(1) {
                                    o.insert(name.clone(), v.clone());
                                }
                                Value::Object(o)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let text = toks.iter().filter_map(|t| t.get("word").and_then(Value::as_str)).collect::<Vec<_>>().join(" ");
                let mut out = json!({
                    "corpus": e.get("corpus").cloned().unwrap_or(Value::Null),
                    "pos": e.get("pos").cloned().unwrap_or(Value::Null),
                    "end": e.get("end").cloned().unwrap_or(Value::Null),
                    "text": text,
                    "tokens": toks,
                });
                if let Some(m) = e.get("match").filter(|m| m.is_object()) {
                    out["match"] = m.clone();
                }
                out
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_values() {
        assert_eq!(request(None).unwrap(), None);
        assert_eq!(request(Some(&json!(false))).unwrap(), None);
        assert_eq!(request(Some(&json!(true))).unwrap(), Some(json!("*")));
        assert_eq!(request(Some(&json!("par_en, par_de"))).unwrap(), Some(json!("par_en,par_de")));
        assert_eq!(request(Some(&json!(["par_en"]))).unwrap(), Some(json!("par_en")));
        assert!(request(Some(&json!("par en"))).is_err());
        assert!(request(Some(&json!([1]))).is_err());
        assert!(request(Some(&json!(3))).is_err());
    }

    #[test]
    fn hit_shape() {
        let lib = json!({"pos": 17, "end": 18, "aligned": [
            {"corpus": "par_en", "pos": 24, "end": 28, "tokens": [["The", "the"], ["dog", "dog"], ["sleeps", "sleep"], [".", "."]], "match": {"pos": 26, "end": 27}},
            {"corpus": "par_de", "pos": null, "end": null, "tokens": []}
        ]});
        let a = hit(&lib, &["word".to_string(), "lemma".to_string()]).unwrap();
        assert_eq!(a[0]["text"], "The dog sleeps .");
        assert_eq!(a[0]["tokens"][2], json!({"pos": 26, "word": "sleeps", "lemma": "sleep"}));
        assert_eq!(a[0]["match"], json!({"pos": 26, "end": 27}));
        assert_eq!(a[1], json!({"corpus": "par_de", "pos": null, "end": null, "text": "", "tokens": []}));
        assert_eq!(hit(&json!({"pos": 1}), &[]), None);
    }
}
