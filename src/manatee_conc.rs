//! Manatee sessions, worker side (dev/MANATEE-SESSIONS.md): the concordances KonText's operation
//! chain makes, kept in the worker. A request names the query and the chain of operations after it
//! (KonText's own operation strings: `s…` sort, `p…`/`n…`/`P…`/`N…` filters, `r…` sample, `f`
//! shuffle, `D`, `F…`) and what it wants of the result (a page of lines, the size, a frequency
//! distribution, collocations, the ARF, the sizes of a structure attribute's values). The worker
//! starts from the longest chain prefix it still holds, runs the rest with Manatee's own
//! `Concordance` calls (manatee/fqs_manatee_conc.cc), and keeps every step (least recently used out,
//! within a memory budget). A worker that was replaced rebuilds the chain from the request: the
//! chain *is* the session's content, so a lost session costs time, never an answer.
//!
//! Requests ({"op": "conc", …}): corpus, registry, subc?, cql, default_attr?, ops: [..],
//! `want`: "page" {start, size, view, doc_attr?, arf?} | "size" | "arf" | "freq" {crit, limit} |
//! "coll" {attr, sortfn, minfreq, minbgr, fromw, tow, max_items, bgrfns} | "attr_sizes" {attr}.

use crate::manatee_lib::{take, ManateeLib};
use anyhow::{anyhow, bail, Context, Result};
use libloading::Library;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_longlong, c_uint, c_void};

type NewFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *mut *mut c_char) -> *mut c_void;
type ApplyFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char, c_uint, *const c_char, *mut *mut c_char) -> *mut c_void;
type FreeConcFn = unsafe extern "C" fn(*mut c_void);
type ConcStrFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_char) -> *mut c_char;
type PageFn = unsafe extern "C" fn(*mut c_void, *mut c_void, c_longlong, c_longlong, *const c_char, *const c_char, *mut *mut c_char) -> *mut c_char;
type FreqFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char, c_longlong, *mut *mut c_char) -> *mut c_char;
type AttrSizesFn = unsafe extern "C" fn(*mut c_void, *const c_char, *mut *mut c_char) -> *mut c_char;
type CollFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char, *const c_char, c_longlong, c_longlong, c_int, c_int, c_int, *const c_char, *mut *mut c_char) -> *mut c_char;

/// The library's concordance calls (fqs-manatee 0.9).
pub struct ConcFns {
    new: NewFn,
    apply: ApplyFn,
    free_conc: FreeConcFn,
    size: ConcStrFn,
    arf: ConcStrFn,
    page: PageFn,
    freq: FreqFn,
    attr_sizes: AttrSizesFn,
    coll: CollFn,
}

impl ConcFns {
    /// None for a library without them (before 0.9).
    pub fn load(lib: &Library) -> Option<ConcFns> {
        // SAFETY: the C interface of manatee/fqs_manatee_conc.cc
        unsafe {
            Some(ConcFns {
                new: *lib.get::<NewFn>(b"fqsm_conc_new\0").ok()?,
                apply: *lib.get::<ApplyFn>(b"fqsm_conc_apply\0").ok()?,
                free_conc: *lib.get::<FreeConcFn>(b"fqsm_conc_free\0").ok()?,
                size: *lib.get::<ConcStrFn>(b"fqsm_conc_size\0").ok()?,
                arf: *lib.get::<ConcStrFn>(b"fqsm_conc_arf\0").ok()?,
                page: *lib.get::<PageFn>(b"fqsm_conc_page\0").ok()?,
                freq: *lib.get::<FreqFn>(b"fqsm_conc_freq\0").ok()?,
                attr_sizes: *lib.get::<AttrSizesFn>(b"fqsm_attr_value_sizes\0").ok()?,
                coll: *lib.get::<CollFn>(b"fqsm_conc_coll\0").ok()?,
            })
        }
    }
}

/// A stored concordance (owned: freed when it leaves the store).
struct Stored {
    ptr: *mut c_void,
    lines: u64,
}

/// The concordances of this worker, by chain key; least recently used first out.
#[derive(Default)]
pub struct ConcStore {
    map: HashMap<String, Stored>,
    order: VecDeque<String>,
    bytes: u64,
}

// SAFETY: the pointers are only used by the worker's one thread, under ManateeLib's lock
unsafe impl Send for ConcStore {}

/// ~16 bytes per line (Manatee's ConcItem + view), as dev/MANATEE-SESSIONS.md.
const BYTES_PER_LINE: u64 = 16;

fn budget_bytes() -> u64 {
    std::env::var("FQS_MANATEE_SESSION_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|v| *v > 0).unwrap_or(2048) * 1024 * 1024
}

/// A chain longer than this is run but not kept (a result of 33M lines is ~0.5 GB).
fn max_kept_lines() -> u64 {
    std::env::var("FQS_MANATEE_SESSION_MAX_HITS").ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|v| *v > 0).unwrap_or(40_000_000)
}

impl ConcStore {
    fn touch(&mut self, key: &str) {
        if let Some(i) = self.order.iter().position(|k| k == key) {
            self.order.remove(i);
        }
        self.order.push_back(key.to_string());
    }

    fn get(&mut self, key: &str) -> Option<*mut c_void> {
        let p = self.map.get(key).map(|s| s.ptr)?;
        self.touch(key);
        Some(p)
    }

    fn put(&mut self, fns: &ConcFns, key: String, ptr: *mut c_void, lines: u64) -> bool {
        if lines > max_kept_lines() || self.map.contains_key(&key) {
            return false;
        }
        self.bytes += lines * BYTES_PER_LINE;
        self.map.insert(key.clone(), Stored { ptr, lines });
        self.touch(&key);
        while self.bytes > budget_bytes() && self.order.len() > 1 {
            let Some(old) = self.order.pop_front() else { break };
            if old == key {
                self.order.push_back(old);
                continue;
            }
            if let Some(s) = self.map.remove(&old) {
                self.bytes = self.bytes.saturating_sub(s.lines * BYTES_PER_LINE);
                unsafe { (fns.free_conc)(s.ptr) };
            }
        }
        true
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// A seed for a chain step, the same in every worker (FNV-1a of the key).
fn seed_of(key: &str) -> u32 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in key.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    ((h >> 32) ^ h) as u32 | 1
}

fn cstr(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| anyhow!("a request string contains a NUL byte"))
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn int_of(v: &Value, k: &str, default: i64) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(default)
}

/// The KWIC line spec of a page ({left, right, attrs, ctxattrs, structs, refs}) as the library reads it.
fn view_spec(view: &Value) -> String {
    ["left", "right", "attrs", "ctxattrs", "structs", "refs"]
        .iter()
        .map(|k| view.get(*k).and_then(Value::as_str).unwrap_or("").replace('\u{1f}', ""))
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

impl ManateeLib {
    /// Whether this library has corpus info and context (fqs-manatee 0.9).
    pub fn has_info(&self) -> bool {
        self.info.is_some()
    }

    /// Whether this library has stored concordances (fqs-manatee 0.9).
    pub fn has_conc(&self) -> bool {
        self.conc.is_some()
    }

    /// One "conc" request (see the module doc): the concordance of the chain, then `want` of it.
    pub fn conc_run(&self, id: &str, registry: &std::path::Path, req: &Value) -> Result<Value> {
        let fns = self.conc.as_ref().ok_or_else(|| anyhow!("{} has no stored concordances: rebuild the library (fqs-manatee 0.9 or later)", self.version))?;
        let h = self.handle_for(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let cql = str_of(req, "cql");
        if cql.trim().is_empty() {
            bail!("an empty query");
        }
        let default_attr = str_of(req, "default_attr").trim();
        let ops: Vec<&str> = req.get("ops").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).filter(|o| !o.is_empty()).collect()).unwrap_or_default();
        let subc = self.current_subcorpus_key();
        let base = format!("{id}\u{0}{}\u{0}{subc}\u{0}{default_attr}\u{0}{cql}", registry.display());
        let key_at = |n: usize| -> String { if n == 0 { base.clone() } else { format!("{base}\u{0}{}", ops[..n].join("\u{0}")) } };
        let mut store = self.concs.lock().map_err(|_| anyhow!("manatee concordance store lock poisoned"))?;

        // the longest prefix of the chain we hold
        let mut have = ops.len() + 1;
        let mut ptr = std::ptr::null_mut();
        for n in (0..=ops.len()).rev() {
            if let Some(p) = store.get(&key_at(n)) {
                have = n;
                ptr = p;
                break;
            }
        }
        let da = cstr(default_attr)?;
        let mut err: *mut c_char = std::ptr::null_mut();
        let mut owned = false;   // `ptr` is a concordance this request made and nobody stores
        if have > ops.len() {
            let q = cstr(cql)?;
            ptr = unsafe { (fns.new)(h.0, q.as_ptr(), da.as_ptr(), &mut err) };
            if ptr.is_null() {
                return Err(self.conc_err(err));
            }
            have = 0;
            let lines = self.conc_lines(fns, ptr);
            owned = !store.put(fns, key_at(0), ptr, lines);
        }
        for n in have + 1..=ops.len() {
            let key = key_at(n);
            let op = cstr(ops[n - 1])?;
            let next = unsafe { (fns.apply)(h.0, ptr, op.as_ptr(), seed_of(&key), da.as_ptr(), &mut err) };
            if next.is_null() {
                if owned {
                    unsafe { (fns.free_conc)(ptr) };
                }
                return Err(self.conc_err(err));
            }
            let lines = self.conc_lines(fns, next);
            if owned {
                unsafe { (fns.free_conc)(ptr) };   // an unstored step is not needed once the next exists
            }
            owned = !store.put(fns, key, next, lines);
            ptr = next;
        }
        let result = self.conc_want(fns, h.0, ptr, req);
        if owned {
            unsafe { (fns.free_conc)(ptr) };
        }
        let mut result = result?;
        if let Some(o) = result.as_object_mut() {
            o.insert("kept".into(), json!(store.len()));
            o.insert("reused_steps".into(), json!(have));
        }
        Ok(result)
    }

    fn conc_err(&self, err: *mut c_char) -> anyhow::Error {
        use std::ffi::CStr;
        let msg = if err.is_null() { "unknown Manatee error".to_string() } else { unsafe { CStr::from_ptr(err).to_string_lossy().into_owned() } };
        if !err.is_null() {
            unsafe { (self.free_fn())(err) };
        }
        anyhow!("Manatee: {msg}")
    }

    fn conc_lines(&self, fns: &ConcFns, ptr: *mut c_void) -> u64 {
        let mut err: *mut c_char = std::ptr::null_mut();
        let v = unsafe { take(self.free_fn(), (fns.size)(ptr, &mut err), err) };
        v.ok().and_then(|v| v.get("size").and_then(Value::as_u64)).unwrap_or(0)
    }

    fn conc_want(&self, fns: &ConcFns, corp: *mut c_void, ptr: *mut c_void, req: &Value) -> Result<Value> {
        let want = req.get("want").cloned().unwrap_or_else(|| json!({}));
        let kind = match &want {
            Value::String(s) => s.as_str(),
            v => v.get("kind").and_then(Value::as_str).unwrap_or("size"),
        };
        let free = self.free_fn();
        let mut err: *mut c_char = std::ptr::null_mut();
        match kind {
            "size" => unsafe { take(free, (fns.size)(ptr, &mut err), err) },
            "arf" => unsafe { take(free, (fns.arf)(ptr, &mut err), err) },
            "page" => {
                let view = cstr(&req.get("view").filter(|v| v.is_object()).map(view_spec).unwrap_or_default())?;
                let doc = cstr(str_of(req, "doc_attr"))?;
                let mut page = unsafe { take(free, (fns.page)(corp, ptr, int_of(req, "start", 0), int_of(req, "size", 20), view.as_ptr(), doc.as_ptr(), &mut err), err)? };
                let mut err2: *mut c_char = std::ptr::null_mut();
                let size = unsafe { take(free, (fns.size)(ptr, &mut err2), err2)? };
                if let (Some(p), Some(s)) = (page.as_object_mut(), size.as_object()) {
                    for k in ["fullsize", "finished"] {
                        if let Some(v) = s.get(k) {
                            p.insert(k.into(), v.clone());
                        }
                    }
                }
                if req.get("arf").and_then(Value::as_bool).unwrap_or(false) {
                    let mut err3: *mut c_char = std::ptr::null_mut();
                    let a = unsafe { take(free, (fns.arf)(ptr, &mut err3), err3)? };
                    if let Some(p) = page.as_object_mut() {
                        p.insert("arf".into(), a.get("arf").cloned().unwrap_or(Value::Null));
                    }
                }
                Ok(page)
            }
            "freq" => {
                let crit = cstr(str_of(req, "crit"))?;
                unsafe { take(free, (fns.freq)(corp, ptr, crit.as_ptr(), int_of(req, "limit", 0), &mut err), err) }
            }
            "coll" => {
                let attr = cstr(str_of(req, "attr"))?;
                let sortfn = cstr(str_of(req, "sortfn"))?;
                let bgr = cstr(str_of(req, "bgrfns"))?;
                unsafe {
                    take(
                        free,
                        (fns.coll)(corp, ptr, attr.as_ptr(), sortfn.as_ptr(), int_of(req, "minfreq", 5), int_of(req, "minbgr", 3), int_of(req, "fromw", -5) as c_int, int_of(req, "tow", 5) as c_int, int_of(req, "max_items", 0) as c_int, bgr.as_ptr(), &mut err),
                        err,
                    )
                }
            }
            other => bail!("unknown conc request '{other}'"),
        }
        .with_context(|| format!("Manatee concordance request '{kind}'"))
    }

    /// The sizes in positions of the values of a structure attribute (`attr_sizes`), for the norms of a
    /// frequency by text type; needs no concordance.
    pub fn attr_value_sizes(&self, id: &str, registry: &std::path::Path, attr: &str) -> Result<Value> {
        let fns = self.conc.as_ref().ok_or_else(|| anyhow!("{} has no stored concordances: rebuild the library (fqs-manatee 0.9 or later)", self.version))?;
        let h = self.handle_for(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let a = cstr(attr)?;
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe { take(self.free_fn(), (fns.attr_sizes)(h.0, a.as_ptr(), &mut err), err) }
    }
}
