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
    corpora: Mutex<HashMap<String, Arc<Mutex<Handle>>>>,
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
            Ok(ManateeLib { _lib: lib, path: path.to_path_buf(), version, open, close, query, freq, free, check, corpora: Mutex::new(HashMap::new()) })
        }
    }

    /// The corpus `id` (registry file `registry`), opened on first use and kept open.
    fn handle(&self, id: &str, registry: &Path) -> Result<Arc<Mutex<Handle>>> {
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
        let _ = std::fs::remove_dir_all(&d);
    }
}
