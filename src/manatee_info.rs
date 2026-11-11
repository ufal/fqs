//! Native Manatee: what KonText reads of a corpus beside concordances (fqs-manatee 0.9,
//! manatee/fqs_manatee_info.cc): the corpus details (sizes, values per attribute, regions per
//! structure) and the context of one token with the structure attributes at it.

use crate::manatee_lib::{take, ManateeLib};
use anyhow::{anyhow, Result};
use libloading::Library;
use serde_json::Value;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_longlong, c_void};
use std::path::Path;

type InfoFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_char) -> *mut c_char;
type ContextFn = unsafe extern "C" fn(*mut c_void, c_longlong, c_int, c_int, c_int, *const c_char, *const c_char, *mut *mut c_char) -> *mut c_char;

pub struct InfoFns {
    info: InfoFn,
    context: ContextFn,
}

impl InfoFns {
    pub fn load(lib: &Library) -> Option<InfoFns> {
        // SAFETY: the C interface of manatee/fqs_manatee_info.cc
        unsafe {
            Some(InfoFns {
                info: *lib.get::<InfoFn>(b"fqsm_corpus_info\0").ok()?,
                context: *lib.get::<ContextFn>(b"fqsm_context\0").ok()?,
            })
        }
    }
}

impl ManateeLib {
    fn info_fns(&self) -> Result<&InfoFns> {
        self.info.as_ref().ok_or_else(|| anyhow!("{} has no corpus info: rebuild the library (fqs-manatee 0.9 or later)", self.version))
    }

    /// {"size", "search_size", "attributes": [{name, vocab}], "structures": [{name, regions, attrs}]}
    pub fn corpus_info(&self, id: &str, registry: &Path) -> Result<Value> {
        let f = self.info_fns()?;
        let h = self.handle_for(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe { take(self.free_fn(), (f.info)(h.0, &mut err), err) }
    }

    /// {"pos", "left", "match", "right", "doc_id"?, "regions"?}: see fqsm_context.
    #[allow(clippy::too_many_arguments)]
    pub fn context(&self, id: &str, registry: &Path, pos: i64, left: i64, right: i64, regions: bool, attr: &str, doc_attr: &str) -> Result<Value> {
        let f = self.info_fns()?;
        let h = self.handle_for(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let (a, d) = (CString::new(attr)?, CString::new(doc_attr)?);
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe { take(self.free_fn(), (f.context)(h.0, pos, left as c_int, right as c_int, regions as c_int, a.as_ptr(), d.as_ptr(), &mut err), err) }
    }
}

/// `/info` for a Manatee corpus: the registry's lists (registry_info.rs) with the numbers from
/// Manatee (`size`, `tokens`, `vocab` per attribute, `regions` per structure). Without native
/// Manatee the registry's answer stays as it is.
pub async fn add_native_info(result: &mut Value, corpus_id: &str, registry: &Path) {
    let Some(pool) = crate::manatee_pool::global() else { return };
    let (id, reg) = (corpus_id.to_string(), registry.to_path_buf());
    let native = tokio::task::spawn_blocking(move || pool.call(&id, serde_json::json!({ "op": "info", "registry": reg }))).await;
    let Ok(Ok(native)) = native else { return };
    if let Some(size) = native.get("size").filter(|s| s.is_number()) {
        result["size"] = size.clone();
        result["tokens"] = size.clone();
    }
    let vocab = |list: &Value| -> std::collections::HashMap<String, Value> {
        list.as_array().map(|a| a.iter().filter_map(|x| Some((x.get("name")?.as_str()?.to_string(), x.clone()))).collect()).unwrap_or_default()
    };
    let attrs = vocab(&native["attributes"]);
    if let Some(list) = result.get_mut("attributes").and_then(Value::as_array_mut) {
        for a in list {
            if let Some(v) = a.get("name").and_then(Value::as_str).and_then(|n| attrs.get(n)).and_then(|x| x.get("vocab")) {
                a["vocab"] = v.clone();
            }
        }
    }
    let structs = vocab(&native["structures"]);
    if let Some(list) = result.get_mut("structures").and_then(Value::as_array_mut) {
        for s in list {
            if let Some(n) = s.get("name").and_then(Value::as_str).and_then(|n| structs.get(n)) {
                if let Some(r) = n.get("regions") {
                    s["regions"] = r.clone();
                }
            }
        }
    }
}

/// `GET /context` for a Manatee corpus, in the shape pando answers: `left`, `match`, `right` (words
/// joined by spaces), `doc_id`, `regions` (`regions=1`).
pub async fn http_context(
    corpus: &crate::CorpusEntry,
    pos: &Option<String>,
    left: &Option<String>,
    right: &Option<String>,
    regions: &Option<String>,
) -> Result<axum::Json<Value>, (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;
    let pool = crate::manatee_pool::global().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "native Manatee is not available (FQS_MANATEE_LIB)".to_string()))?;
    let registry = crate::manatee_lib::registry_file(&corpus.settings, &corpus.id).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no Manatee registry for '{}'", corpus.id)))?;
    let num = |v: &Option<String>, d: i64| v.as_deref().and_then(|s| s.trim().parse::<i64>().ok()).unwrap_or(d);
    let doc_attr = corpus.settings.pointer("/fcs/doc_attr").and_then(Value::as_str).map(crate::fcs::engine::manatee_struct_attr).or_else(|| crate::manatee_lib::fullref(&registry)).unwrap_or_default();
    let req = serde_json::json!({
        "op": "context", "registry": registry, "pos": num(pos, 0), "left": num(left, 20), "right": num(right, 20),
        "regions": matches!(regions.as_deref(), Some("1") | Some("true") | Some("yes")), "doc_attr": doc_attr,
    });
    let id = corpus.id.clone();
    let mut answer = tokio::task::spawn_blocking(move || pool.call(&id, req))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("worker join: {e}")))?
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    answer["ok"] = serde_json::json!(true);
    answer["operation"] = serde_json::json!("context");
    Ok(axum::Json(answer))
}
