//! Native Manatee: libfqs_manatee (manatee/fqs_manatee.cc, a C interface over manatee-open), loaded
//! from `FQS_MANATEE_LIB` (dev/MANATEE-NATIVE.md). No Python, no KonText. Loaded only in the Manatee
//! worker processes (manatee_worker.rs), never in FQS itself: a crash in Manatee (a corpus read
//! wrongly) ends a worker, not FQS (manatee_pool.rs). Corpora stay open once used.
//!
//! The catalogue: `preferred_backend: manatee`, `settings.manatee_registry` (the registry file, or
//! the registry folder with the corpus id as the file name), else `MANATEE_REGISTRY` + the id.

use anyhow::{anyhow, bail, Context, Result};
use libloading::{Library, Symbol};
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_longlong, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type OpenFn = unsafe extern "C" fn(*const c_char, *mut *mut c_char) -> *mut c_void;
type CloseFn = unsafe extern "C" fn(*mut c_void);
type QueryFn = unsafe extern "C" fn(*mut c_void, *const c_char, c_longlong, c_longlong, c_int, *const c_char, *mut *mut c_char) -> *mut c_char;
type FreqFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_longlong, *mut *mut c_char) -> *mut c_char;
type FreeFn = unsafe extern "C" fn(*mut c_char);
type VersionFn = unsafe extern "C" fn() -> *const c_char;
type CheckFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_char) -> *mut c_char;
type KwicFn = unsafe extern "C" fn(*mut c_void, *const c_char, c_longlong, c_longlong, c_int, *const c_char, *const c_char, *mut *mut c_char) -> *mut c_char;
type KwicPageFn = unsafe extern "C" fn(*mut c_void, *const c_char, c_longlong, c_longlong, c_int, *const c_char, *const c_char, c_int, *mut *mut c_char) -> *mut c_char;
type KwicAlignedFn = unsafe extern "C" fn(*mut c_void, *const c_char, c_longlong, c_longlong, c_int, *const c_char, *const c_char, c_int, *const c_char, *mut *mut c_char) -> *mut c_char;
type ExportFn = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, c_longlong, *mut *mut c_char) -> *mut c_char;
type SubcCreateFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *const c_char, *mut *mut c_char) -> *mut c_char;
type OpenSubcFn = unsafe extern "C" fn(*mut c_void, *const c_char, *mut *mut c_char) -> *mut c_void;
type SizesFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_char) -> *mut c_char;

/// An open corpus (a `Corpus *` inside the library).
struct Handle(*mut c_void);
// SAFETY: a handle is only used under its own Mutex.
unsafe impl Send for Handle {}

pub struct ManateeLib {
    _lib: Library,
    pub path: PathBuf,
    pub version: String,
    open: OpenFn,
    close: CloseFn,
    query: QueryFn,
    freq: FreqFn,
    free: FreeFn,
    check: Option<CheckFn>,
    kwic: Option<KwicFn>,
    kwic_page: Option<KwicPageFn>,
    kwic_aligned: Option<KwicAlignedFn>,
    export_jsonl: Option<ExportFn>,
    subc_create: Option<SubcCreateFn>,
    open_subc: Option<OpenSubcFn>,
    sizes: Option<SizesFn>,
    corpora: Mutex<HashMap<String, Arc<Mutex<Handle>>>>,
    /// The subcorpus (.subc) the current request runs under ([`ManateeLib::set_subcorpus`]): the
    /// worker answers one request at a time, and sets it per request.
    subcorpus: Mutex<Option<PathBuf>>,
}

impl Drop for ManateeLib {
    fn drop(&mut self) {
        if let Ok(map) = self.corpora.lock() {
            for h in map.values() {
                if let Ok(h) = h.lock() {
                    unsafe { (self.close)(h.0) };
                }
            }
        }
    }
}

/// The result of the library: its JSON, or its error message.
unsafe fn take(free: FreeFn, out: *mut c_char, err: *mut c_char) -> Result<Value> {
    // SAFETY: `out` / `err` are NULL or NUL-terminated strings from libfqs_manatee, freed with its fqsm_free
    unsafe {
        if out.is_null() {
            let msg = if err.is_null() { "unknown Manatee error".to_string() } else { CStr::from_ptr(err).to_string_lossy().into_owned() };
            if !err.is_null() {
                free(err);
            }
            bail!("Manatee: {msg}");
        }
        let text = CStr::from_ptr(out).to_string_lossy().into_owned();
        free(out);
        serde_json::from_str(&text).context("libfqs_manatee returned invalid JSON")
    }
}

impl ManateeLib {
    pub fn load(path: &Path) -> Result<Self> {
        // Every symbol resolved now (RTLD_NOW), not on first use: a library that misses one (an
        // incomplete libmanatee.so lacks fsa3's fsa_init) is refused here, at start-up, instead of
        // the process exiting on the first query ("symbol lookup error").
        // SAFETY: the C interface of manatee/fqs_manatee.cc
        #[cfg(unix)]
        let lib: Library = unsafe {
            libloading::os::unix::Library::open(Some(path), libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_LOCAL)
        }
        .map(Library::from)
        .with_context(|| format!("failed to load {} (all its symbols must resolve)", path.display()))?;
        #[cfg(not(unix))]
        let lib = unsafe { Library::new(path) }.with_context(|| format!("failed to load {}", path.display()))?;
        unsafe {
            let version: Symbol<VersionFn> = lib.get(b"fqsm_version\0")?;
            let version = CStr::from_ptr(version()).to_string_lossy().into_owned();
            let open = *lib.get::<OpenFn>(b"fqsm_open\0")?;
            let close = *lib.get::<CloseFn>(b"fqsm_close\0")?;
            let query = *lib.get::<QueryFn>(b"fqsm_query\0")?;
            let freq = *lib.get::<FreqFn>(b"fqsm_freq\0")?;
            let free = *lib.get::<FreeFn>(b"fqsm_free\0")?;
            let check = lib.get::<CheckFn>(b"fqsm_check\0").ok().map(|f| *f);   // from fqs-manatee 0.2
            let kwic = lib.get::<KwicFn>(b"fqsm_kwic\0").ok().map(|f| *f);   // from fqs-manatee 0.3
            let kwic_page = lib.get::<KwicPageFn>(b"fqsm_kwic_page\0").ok().map(|f| *f);   // from 0.4
            let kwic_aligned = lib.get::<KwicAlignedFn>(b"fqsm_kwic_aligned\0").ok().map(|f| *f);   // from 0.5
            let export_jsonl = lib.get::<ExportFn>(b"fqsm_export_jsonl\0").ok().map(|f| *f);   // from 0.6
            let subc_create = lib.get::<SubcCreateFn>(b"fqsm_subc_create\0").ok().map(|f| *f);   // from 0.7
            let open_subc = lib.get::<OpenSubcFn>(b"fqsm_open_subc\0").ok().map(|f| *f);   // from 0.7
            let sizes = lib.get::<SizesFn>(b"fqsm_sizes\0").ok().map(|f| *f);   // from 0.7
            Ok(ManateeLib {
                _lib: lib,
                path: path.to_path_buf(),
                version,
                open,
                close,
                query,
                freq,
                free,
                check,
                kwic,
                kwic_page,
                kwic_aligned,
                export_jsonl,
                subc_create,
                open_subc,
                sizes,
                corpora: Mutex::new(HashMap::new()),
                subcorpus: Mutex::new(None),
            })
        }
    }

    /// Whether the library has subcorpora (fqsm_subc_create / fqsm_open_subc, fqs-manatee 0.7).
    pub fn has_subcorpora(&self) -> bool {
        self.subc_create.is_some() && self.open_subc.is_some()
    }

    /// Run the following calls under the subcorpus file `path` (None: the whole corpus).
    pub fn set_subcorpus(&self, path: Option<PathBuf>) {
        if let Ok(mut s) = self.subcorpus.lock() {
            *s = path;
        }
    }

    /// The corpus `id`, or under [`ManateeLib::set_subcorpus`] the corpus opened as that subcorpus
    /// (Manatee's SubCorpus; a handle of its own, kept open like the corpus: a re-resolved subcorpus
    /// is written to a new file, so a kept handle never reads a changed one).
    fn handle(&self, id: &str, registry: &Path) -> Result<Arc<Mutex<Handle>>> {
        let subc = self.subcorpus.lock().map_err(|_| anyhow!("manatee subcorpus lock poisoned"))?.clone();
        let Some(subc) = subc else { return self.corpus_handle(id, registry) };
        let open_subc = self.open_subc.ok_or_else(|| anyhow!("{} has no subcorpora: rebuild the library (fqs-manatee 0.7 or later)", self.version))?;
        let key = format!("{id}\u{0}{}", subc.display());
        if let Some(h) = self.corpora.lock().map_err(|_| anyhow!("manatee corpora lock poisoned"))?.get(&key) {
            return Ok(Arc::clone(h));
        }
        let parent = self.corpus_handle(id, registry)?;
        let parent = parent.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let p = CString::new(subc.to_string_lossy().as_bytes())?;
        let mut err: *mut c_char = std::ptr::null_mut();
        let h = unsafe { open_subc(parent.0, p.as_ptr(), &mut err) };
        if h.is_null() {
            let msg = if err.is_null() { "could not open the subcorpus".to_string() } else { unsafe { CStr::from_ptr(err).to_string_lossy().into_owned() } };
            if !err.is_null() {
                unsafe { (self.free)(err) };
            }
            bail!("Manatee: {msg}");
        }
        let h = Arc::new(Mutex::new(Handle(h)));
        self.corpora.lock().map_err(|_| anyhow!("manatee corpora lock poisoned"))?.insert(key, Arc::clone(&h));
        Ok(h)
    }

    /// Write the subcorpus file `path` of corpus `id`: the regions of `structure` matching the
    /// structure-attribute condition `query`, or (`structure` empty) the hits of the token query
    /// `query`. {"written": bool} (false: nothing selected, no file).
    pub fn subc_create(&self, id: &str, registry: &Path, path: &Path, structure: &str, query: &str) -> Result<Value> {
        let f = self.subc_create.ok_or_else(|| anyhow!("{} has no subcorpora: rebuild the library (fqs-manatee 0.7 or later)", self.version))?;
        let h = self.corpus_handle(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let (p, s, q) = (CString::new(path.to_string_lossy().as_bytes())?, CString::new(structure)?, CString::new(query)?);
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            let out = f(h.0, p.as_ptr(), s.as_ptr(), q.as_ptr(), &mut err);
            take(self.free, out, err)
        }
    }

    /// {"size", "search_size"} of corpus `id` (under a subcorpus: search_size is its size).
    pub fn sizes(&self, id: &str, registry: &Path) -> Result<Value> {
        let f = self.sizes.ok_or_else(|| anyhow!("{} cannot report sizes: rebuild the library (fqs-manatee 0.7 or later)", self.version))?;
        let h = self.handle(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            let out = f(h.0, &mut err);
            take(self.free, out, err)
        }
    }

    /// The corpus `id` (registry file `registry`), opened on first use and kept open.
    fn corpus_handle(&self, id: &str, registry: &Path) -> Result<Arc<Mutex<Handle>>> {
        let mut map = self.corpora.lock().map_err(|_| anyhow!("manatee corpora lock poisoned"))?;
        if let Some(h) = map.get(id) {
            return Ok(Arc::clone(h));
        }
        let reg = CString::new(registry.to_string_lossy().as_bytes())?;
        let mut err: *mut c_char = std::ptr::null_mut();
        let h = unsafe { (self.open)(reg.as_ptr(), &mut err) };
        if h.is_null() {
            let msg = if err.is_null() { "could not open".to_string() } else { unsafe { CStr::from_ptr(err).to_string_lossy().into_owned() } };
            if !err.is_null() {
                unsafe { (self.free)(err) };
            }
            bail!("Manatee: {msg}");
        }
        let h = Arc::new(Mutex::new(Handle(h)));
        map.insert(id.to_string(), Arc::clone(&h));
        Ok(h)
    }

    /// {"ok", "checked"} or {"ok": false, "problem"}: is the corpus readable as its registry
    /// describes it (the token text agrees with the index)? Without fqsm_check (an older library): ok.
    /// Whether the library has the corpus check (fqs-manatee 0.2 and later).
    pub fn has_check(&self) -> bool {
        self.check.is_some()
    }

    pub fn check(&self, id: &str, registry: &Path) -> Result<Value> {
        let Some(check) = self.check else {
            return Ok(serde_json::json!({ "ok": true, "checked": 0 }));
        };
        let h = self.handle(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            let out = check(h.0, &mut err);
            take(self.free, out, err)
        }
    }

    /// The corpus written as pando JSONL v2 to `out` ("-": stdout); `dynamic`: dynamic attributes too;
    /// `limit` > 0: only the first positions. Answers the library's summary (manatee/fqs_manatee_export.cc).
    pub fn export_jsonl(&self, id: &str, registry: &Path, out: &str, dynamic: bool, limit: u64) -> Result<Value> {
        let Some(export) = self.export_jsonl else {
            bail!("{} ({}) cannot export: fqs-manatee 0.6 or newer is needed (manatee/build.sh)", self.path.display(), self.version);
        };
        let h = self.handle(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let o = CString::new(out)?;
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            let res = export(h.0, o.as_ptr(), dynamic as c_int, limit as c_longlong, &mut err);
            take(self.free, res, err)
        }
    }

    /// {"total", "hits": [{"pos", "end", "left", "kwic", "right"}]}: hits [start, start+size) with
    /// `window` tokens of context of `attr` (default: the corpus's default attribute).
    pub fn query(&self, id: &str, registry: &Path, cql: &str, start: u64, size: u64, window: u32, attr: Option<&str>) -> Result<Value> {
        let h = self.handle(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let q = CString::new(cql)?;
        let a = CString::new(attr.unwrap_or(""))?;
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            let out = (self.query)(h.0, q.as_ptr(), start as c_longlong, size as c_longlong, window as c_int, a.as_ptr(), &mut err);
            take(self.free, out, err)
        }
    }

    /// Whether the library gives aligned segments (fqsm_kwic_aligned, fqs-manatee 0.5 and later).
    pub fn has_aligned(&self) -> bool {
        self.kwic_aligned.is_some()
    }

    /// {"total", "counted", "attrs", "hits": [{"pos", "end", "doc"?, "left", "kwic", "right"}]}: per
    /// token the values of `attrs` (empty: the default attribute), `doc_attr` ("doc.id") at the hit.
    /// `count_all` false: the result is walked only to the end of the page ("counted": false; a
    /// library before 0.4 always counts). `aligned` (a parallel corpus: registry names of aligned
    /// corpora, comma-separated, `*` all): also "aligned_corpora" and per hit "aligned" (0.5).
    #[allow(clippy::too_many_arguments)]
    pub fn kwic(&self, id: &str, registry: &Path, cql: &str, start: u64, size: u64, window: u32, attrs: &[String], doc_attr: Option<&str>, count_all: bool, aligned: Option<&str>) -> Result<Value> {
        let kwic = self.kwic.ok_or_else(|| anyhow!("{} has no fqsm_kwic: rebuild the library (fqs-manatee 0.3 or later)", self.version))?;
        let aligned_fn = match aligned {
            Some(_) => Some(self.kwic_aligned.ok_or_else(|| anyhow!("{} gives no aligned segments: rebuild the library (fqs-manatee 0.5 or later)", self.version))?),
            None => None,
        };
        let h = self.handle(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let q = CString::new(cql)?;
        let a = CString::new(attrs.join(","))?;
        let d = CString::new(doc_attr.unwrap_or(""))?;
        let al = CString::new(aligned.unwrap_or(""))?;
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            if let Some(f) = aligned_fn {
                let out = f(h.0, q.as_ptr(), start as c_longlong, size as c_longlong, window as c_int, a.as_ptr(), d.as_ptr(), count_all as c_int, al.as_ptr(), &mut err);
                return take(self.free, out, err);
            }
            let out = match self.kwic_page {
                Some(page) => page(h.0, q.as_ptr(), start as c_longlong, size as c_longlong, window as c_int, a.as_ptr(), d.as_ptr(), count_all as c_int, &mut err),
                None => kwic(h.0, q.as_ptr(), start as c_longlong, size as c_longlong, window as c_int, a.as_ptr(), d.as_ptr(), &mut err),
            };
            take(self.free, out, err)
        }
    }

    /// {"rows": [{"value", "count"}], "groups"}: frequencies by a Manatee criterion ("word 0", …).
    pub fn freq(&self, id: &str, registry: &Path, cql: &str, crit: &str, limit: u64) -> Result<Value> {
        let h = self.handle(id, registry)?;
        let h = h.lock().map_err(|_| anyhow!("manatee corpus lock poisoned"))?;
        let q = CString::new(cql)?;
        let c = CString::new(crit)?;
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            let out = (self.freq)(h.0, q.as_ptr(), c.as_ptr(), limit as c_longlong, &mut err);
            take(self.free, out, err)
        }
    }
}

/// A Manatee corpus's registry file: settings.manatee_registry (a file, or a folder + the corpus
/// name), else MANATEE_REGISTRY + the name. The name: settings.corpus_name, else the corpus id.
pub fn registry_file(settings: &Value, corpus_id: &str) -> Option<PathBuf> {
    let name = settings.get("corpus_name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(corpus_id);
    let hint = settings
        .get("manatee_registry")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("MANATEE_REGISTRY").ok().map(|s| PathBuf::from(s.split(':').next().unwrap_or("").trim())));
    let p = hint?;
    if p.is_file() {
        return Some(p);
    }
    let f = p.join(name);
    f.is_file().then_some(f)
}

/// A Manatee corpus's data folder: the `PATH` line of its registry file.
pub fn data_dir(registry_file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(registry_file).ok()?;
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("PATH")?;
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let dir = rest.trim().trim_matches('"').trim();
        (!dir.is_empty()).then(|| PathBuf::from(dir))
    })
}

/// The structure attribute naming a hit's document: the first of the registry's `FULLREF`
/// ("doc.id", the reference KonText shows), if any.
pub fn fullref(registry_file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(registry_file).ok()?;
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("FULLREF")?;
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let first = rest.trim().trim_matches('"').split(',').next()?.trim().to_string();
        (first.contains('.')).then_some(first)
    })
}

/// The attributes the warm-up reads first in a Manatee data folder.
pub const MANATEE_PRIORITY: &[&str] = &["word.", "lemma.", "lc."];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn registry_and_data_dir() {
        let d = std::env::temp_dir().join(format!("fqs-manatee-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("susanne"), "NAME \"Susanne\"\nPATH  \"/data/manatee/susanne\"\nENCODING utf-8\n").unwrap();
        std::fs::write(d.join("bare"), "PATHNAME x\nPATH /data/bare/\n").unwrap();
        let folder = json!({ "manatee_registry": d.to_string_lossy() });
        assert_eq!(registry_file(&folder, "susanne"), Some(d.join("susanne")), "a registry folder + the id");
        let file = json!({ "manatee_registry": d.join("susanne").to_string_lossy() });
        assert_eq!(registry_file(&file, "other"), Some(d.join("susanne")), "a registry file as it is");
        let named = json!({ "manatee_registry": d.to_string_lossy(), "corpus_name": "bare" });
        assert_eq!(registry_file(&named, "x"), Some(d.join("bare")), "corpus_name over the id");
        assert_eq!(registry_file(&folder, "missing"), None);
        assert_eq!(data_dir(&d.join("susanne")), Some(PathBuf::from("/data/manatee/susanne")));
        assert_eq!(data_dir(&d.join("bare")), Some(PathBuf::from("/data/bare/")), "PATHNAME is not PATH");
        std::fs::write(d.join("refs"), "PATH /x\nFULLREF \"doc.id,doc.title\"\n").unwrap();
        assert_eq!(fullref(&d.join("refs")), Some("doc.id".to_string()));
        assert_eq!(fullref(&d.join("susanne")), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
