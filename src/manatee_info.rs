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

type WordlistFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *const c_char, c_int, *const c_char, f64, *const c_char, *const c_char, *mut *mut c_char) -> *mut c_char;

type StructureFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_longlong, c_int, *mut *mut c_char) -> *mut c_char;

type KeywordsFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char, *const c_char, c_int, *const c_char, f64, f64, c_int, c_int, c_int, *mut *mut c_char) -> *mut c_char;

type RegionAtFn = unsafe extern "C" fn(*mut c_void, *const c_char, c_longlong, *mut *mut c_char) -> *mut c_char;

pub struct InfoFns {
    region_at: RegionAtFn,
    keywords: KeywordsFn,
    structure: StructureFn,
    info: InfoFn,
    context: ContextFn,
    wordlist: WordlistFn,
}

impl InfoFns {
    pub fn load(lib: &Library) -> Option<InfoFns> {
        // SAFETY: the C interface of manatee/fqs_manatee_info.cc
        unsafe {
            Some(InfoFns {
                info: *lib.get::<InfoFn>(b"fqsm_corpus_info\0").ok()?,
                context: *lib.get::<ContextFn>(b"fqsm_context\0").ok()?,
                wordlist: *lib.get::<WordlistFn>(b"fqsm_wordlist\0").ok()?,
                structure: *lib.get::<StructureFn>(b"fqsm_structure\0").ok()?,
                keywords: *lib.get::<KeywordsFn>(b"fqsm_keywords\0").ok()?,
                region_at: *lib.get::<RegionAtFn>(b"fqsm_region_at\0").ok()?,
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

    /// The regions of structure `name` with the values of its attributes `attrs` (see fqsm_structure).
    pub fn structure(&self, id: &str, registry: &Path, name: &str, attrs: &str, max_regions: i64, ranges: bool) -> Result<Value> {
        let f = self.info_fns()?;
        let h = self.handle_for(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let (n, a) = (CString::new(name)?, CString::new(attrs)?);
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe { take(self.free_fn(), (f.structure)(h.0, n.as_ptr(), a.as_ptr(), max_regions as c_longlong, ranges as c_int, &mut err), err) }
    }

    /// The region of structure `name` containing `pos` (see fqsm_region_at).
    pub fn region_at(&self, id: &str, registry: &Path, name: &str, pos: i64) -> Result<Value> {
        let f = self.info_fns()?;
        let h = self.handle_for(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let n = CString::new(name)?;
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe { take(self.free_fn(), (f.region_at)(h.0, n.as_ptr(), pos as c_longlong, &mut err), err) }
    }

    /// Keywords of the corpus `id` against the corpus `ref_id` (see fqsm_keywords): the request's `attr`,
    /// `pattern`, `include_nonwords`, `nums`, `minfreq` (of the word list), `simple_n`, `max_items`,
    /// `kw_minfreq`, `kw_maxfreq`. (Whole corpora: a subcorpus is one per request.)
    pub fn keywords(&self, id: &str, registry: &Path, ref_id: &str, ref_registry: &Path, req: &Value) -> Result<Value> {
        let f = self.info_fns()?;
        let h = self.handle_for(id, registry)?;
        let rh = self.handle_for(ref_id, ref_registry)?;
        let s = |k: &str| req.get(k).and_then(Value::as_str).unwrap_or("");
        let (attr, pat, nums) = (CString::new(s("attr"))?, CString::new(s("pattern"))?, CString::new(if s("nums").is_empty() { "frq" } else { s("nums") })?);
        let n = |k: &str, d: i64| req.get(k).and_then(Value::as_i64).unwrap_or(d);
        let mut err: *mut c_char = std::ptr::null_mut();
        let include = req.get("include_nonwords").and_then(Value::as_bool).unwrap_or(false);
        let args = (req.get("minfreq").and_then(Value::as_f64).unwrap_or(0.0), req.get("simple_n").and_then(Value::as_f64).unwrap_or(1.0));
        // one corpus against itself: one lock
        if std::sync::Arc::ptr_eq(&h, &rh) {
            let g = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
            return unsafe { take(self.free_fn(), (f.keywords)(g.0, g.0, attr.as_ptr(), pat.as_ptr(), include as c_int, nums.as_ptr(), args.0, args.1, n("max_items", 100) as c_int, n("kw_minfreq", 0) as c_int, n("kw_maxfreq", 0) as c_int, &mut err), err) };
        }
        let g = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let rg = rh.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        unsafe { take(self.free_fn(), (f.keywords)(g.0, rg.0, attr.as_ptr(), pat.as_ptr(), include as c_int, nums.as_ptr(), args.0, args.1, n("max_items", 100) as c_int, n("kw_minfreq", 0) as c_int, n("kw_maxfreq", 0) as c_int, &mut err), err) }
    }

    /// A word list (see fqsm_wordlist): the request's `attr`, `pattern`, `nonword`, `nums`, `minfreq`,
    /// `pfilter`, `nfilter` (lists of words or newline-separated text).
    pub fn wordlist(&self, id: &str, registry: &Path, req: &Value) -> Result<Value> {
        let f = self.info_fns()?;
        let h = self.handle_for(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let s = |k: &str| req.get(k).and_then(Value::as_str).unwrap_or("");
        let words = |k: &str| -> String {
            match req.get(k) {
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(|w| w.replace('\n', " ")).collect::<Vec<_>>().join("\n"),
                Some(Value::String(t)) => t.clone(),
                _ => String::new(),
            }
        };
        let nums = if s("nums").is_empty() { "frq" } else { s("nums") };
        let (attr, pat, nonword, nums, pf, nf) = (CString::new(s("attr"))?, CString::new(s("pattern"))?, CString::new(s("nonword"))?, CString::new(nums)?, CString::new(words("pfilter"))?, CString::new(words("nfilter"))?);
        let leave_out = req.get("include_nonwords").and_then(Value::as_bool) == Some(false);
        let minfreq = req.get("minfreq").and_then(Value::as_f64).unwrap_or(0.0);
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe { take(self.free_fn(), (f.wordlist)(h.0, attr.as_ptr(), pat.as_ptr(), nonword.as_ptr(), leave_out as c_int, nums.as_ptr(), minfreq, pf.as_ptr(), nf.as_ptr(), &mut err), err) }
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
