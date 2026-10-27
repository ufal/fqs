//! Dynamic loader for libflexicorp_pando (C ABI).
//!
//! api_version >= 2: flexicorp_pando_request
//! api_version >= 3: busy, idle_seconds, build_json (ServerApi embedding contract)
//! api_version >= 4: open_opts (server options JSON: limits by tier, sessions, …)
//! api_version >= 5: xidx_fragments (TEITOK XML around hits from the project's xidx)

use anyhow::{anyhow, bail, Context, Result};
use libloading::{Library, Symbol};
use serde_json::{json, Value};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_double, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

type ApiVersionFn = unsafe extern "C" fn() -> c_int;
type BuildStringFn = unsafe extern "C" fn() -> *const c_char;
type BuildJsonFn = unsafe extern "C" fn() -> *const c_char;
type OpenFn = unsafe extern "C" fn(*const c_char, *const c_char, c_int) -> *mut c_void;
type OpenOptsFn = unsafe extern "C" fn(*const c_char, *const c_char, *const c_char) -> *mut c_void;
type CloseFn = unsafe extern "C" fn(*mut c_void);
type RequestFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_int,
) -> *mut c_char;
type FreeFn = unsafe extern "C" fn(*mut c_void);
type LastErrorFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type BusyFn = unsafe extern "C" fn(*mut c_void) -> usize;
type IdleSecondsFn = unsafe extern "C" fn(*mut c_void) -> c_double;
type XidxFragmentsFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const i64,
    usize,
    *const c_char,
    c_int,
) -> *mut c_char;

pub struct PandoLib {
    _lib: Library,
    build_string_fn: BuildStringFn,
    build_json_fn: Option<BuildJsonFn>,
    open: OpenFn,
    open_opts_fn: Option<OpenOptsFn>,
    close: CloseFn,
    request: RequestFn,
    free: FreeFn,
    last_error: LastErrorFn,
    busy_fn: Option<BusyFn>,
    idle_seconds_fn: Option<IdleSecondsFn>,
    xidx_fragments_fn: Option<XidxFragmentsFn>,
    cached_api_version: i32,
}

/// Outcome of looking for libflexicorp_pando at startup (see `PandoLib::load`).
pub struct LoadReport {
    pub lib: Option<Arc<PandoLib>>,
    /// The library in use, or the one that was found but could not be used.
    pub path: Option<PathBuf>,
    /// Why no library is in use.
    pub error: Option<String>,
    /// Every path looked at, with what happened ("not found", the load error, "loaded").
    pub checked: Vec<String>,
}

/// How this FQS reaches pando, set once at startup: shown in /health and the admin backends
/// view, and named in query errors when pando cannot run at all.
static STATUS: OnceLock<Value> = OnceLock::new();

pub fn set_status(status: Value) {
    let _ = STATUS.set(status);
}

/// `{mode: "library"|"cli"|"unavailable", library_path, error, checked, cli}`; `{}` before startup.
pub fn status() -> Value {
    STATUS.get().cloned().unwrap_or_else(|| json!({}))
}

/// Why the library is not in use, for query errors ("library: …").
pub fn unavailable_reason() -> Option<String> {
    let s = STATUS.get()?;
    if s.get("mode").and_then(Value::as_str) == Some("library") {
        return None;
    }
    s.get("error").and_then(Value::as_str).map(str::to_string)
}

impl PandoLib {
    /// Looks for the library: `$FLEXICORP_PANDO_LIB` alone when set; else next to the fqs
    /// executable, flexicorp_pando's build folder beside this checkout (development only),
    /// /usr/local/lib, /usr/lib. A library that is found but cannot be used (an old build
    /// without a needed symbol, a wrong file) is reported and the next candidate tried.
    pub fn load() -> LoadReport {
        let mut report = LoadReport { lib: None, path: None, error: None, checked: Vec::new() };
        let explicit = std::env::var("FLEXICORP_PANDO_LIB").ok().filter(|s| !s.trim().is_empty());
        let candidates = match &explicit {
            Some(p) => vec![PathBuf::from(p.trim())],
            None => lib_candidates(),
        };
        for path in candidates {
            if !path.is_file() {
                report.checked.push(format!("{}: not found", path.display()));
                if explicit.is_some() {
                    report.error = Some(format!("FLEXICORP_PANDO_LIB={}: no such file", path.display()));
                }
                continue;
            }
            match Self::load_from(&path) {
                Ok(lib) => {
                    report.checked.push(format!("{}: loaded", path.display()));
                    report.lib = Some(lib);
                    report.path = Some(path);
                    report.error = None;
                    return report;
                }
                Err(e) => {
                    report.checked.push(format!("{}: {e:#}", path.display()));
                    if report.error.is_none() {
                        report.error = Some(format!("{}: {e:#}", path.display()));
                        report.path = Some(path);
                    }
                }
            }
        }
        if report.error.is_none() {
            report.error = Some(
                "libflexicorp_pando not found (put it next to the fqs binary or set FLEXICORP_PANDO_LIB)"
                    .to_string(),
            );
        }
        report
    }

    pub fn load_from(path: &Path) -> Result<Arc<Self>> {
        // SAFETY: documented C ABI from flexicorp_pando.h
        let lib = unsafe { Library::new(path) }.context("cannot be loaded")?;
        unsafe {
            let api_version_fn: ApiVersionFn = required(&lib, "flexicorp_pando_api_version")?;
            let build_string_fn: BuildStringFn = required(&lib, "flexicorp_pando_build_string")?;
            let open: OpenFn = required(&lib, "flexicorp_pando_open")?;
            let close: CloseFn = required(&lib, "flexicorp_pando_close")?;
            let request: RequestFn = required(&lib, "flexicorp_pando_request")?;
            let free: FreeFn = required(&lib, "flexicorp_pando_free")?;
            let last_error: LastErrorFn = required(&lib, "flexicorp_pando_last_error")?;
            let ver = api_version_fn();
            if ver < 2 {
                bail!("reports api_version {ver} (need >= 2 for flexicorp_pando_request): an old build");
            }
            let build_json_fn = if ver >= 3 {
                lib.get::<BuildJsonFn>(b"flexicorp_pando_build_json\0")
                    .ok()
                    .map(|s| *s)
            } else {
                None
            };
            let busy_fn = if ver >= 3 {
                lib.get::<BusyFn>(b"flexicorp_pando_busy\0").ok().map(|s| *s)
            } else {
                None
            };
            let open_opts_fn = if ver >= 4 {
                lib.get::<OpenOptsFn>(b"flexicorp_pando_open_opts\0").ok().map(|s| *s)
            } else {
                None
            };
            let idle_seconds_fn = if ver >= 3 {
                lib.get::<IdleSecondsFn>(b"flexicorp_pando_idle_seconds\0")
                    .ok()
                    .map(|s| *s)
            } else {
                None
            };
            let xidx_fragments_fn = if ver >= 5 {
                lib.get::<XidxFragmentsFn>(b"flexicorp_pando_xidx_fragments\0").ok().map(|s| *s)
            } else {
                None
            };
            if ver >= 3 && (busy_fn.is_none() || idle_seconds_fn.is_none()) {
                bail!("claims api_version {ver} but is missing busy/idle_seconds");
            }
            Ok(Arc::new(Self {
                build_string_fn,
                build_json_fn,
                open,
                open_opts_fn,
                close,
                request,
                free,
                last_error,
                busy_fn,
                idle_seconds_fn,
                xidx_fragments_fn,
                cached_api_version: ver,
                _lib: lib,
            }))
        }
    }

    pub fn api_version(&self) -> i32 {
        self.cached_api_version
    }

    pub fn build_string(&self) -> String {
        unsafe {
            let p = (self.build_string_fn)();
            if p.is_null() {
                return String::new();
            }
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }

    pub fn build_json(&self) -> Option<Value> {
        let f = self.build_json_fn?;
        unsafe {
            let p = f();
            if p.is_null() {
                return None;
            }
            let s = CStr::from_ptr(p).to_string_lossy();
            serde_json::from_str(&s).ok()
        }
    }

    pub fn open(&self, index_dir: &Path, preload: bool) -> Result<*mut c_void> {
        let dir = CString::new(index_dir.to_string_lossy().as_bytes())
            .map_err(|_| anyhow!("index_dir contains NUL"))?;
        unsafe {
            let ctx = (self.open)(std::ptr::null(), dir.as_ptr(), if preload { 1 } else { 0 });
            if ctx.is_null() {
                let err = self.last_error_ptr(std::ptr::null_mut());
                bail!("flexicorp_pando_open failed: {err}");
            }
            Ok(ctx)
        }
    }

    /// Whether the library takes server options (limits by tier, sessions, …).
    pub fn has_open_opts(&self) -> bool {
        self.open_opts_fn.is_some()
    }

    /// Open with pando server options (api_version >= 4); falls back to `open`
    /// (options ignored) with an older library.
    pub fn open_with(&self, index_dir: &Path, preload: bool, options_json: Option<&str>) -> Result<*mut c_void> {
        let (Some(f), Some(opts)) = (self.open_opts_fn, options_json) else {
            return self.open(index_dir, preload);
        };
        let mut v: Value = serde_json::from_str(opts).unwrap_or_else(|_| Value::Object(Default::default()));
        if preload {
            v["preload"] = Value::Bool(true);
        }
        let dir = CString::new(index_dir.to_string_lossy().as_bytes())
            .map_err(|_| anyhow!("index_dir contains NUL"))?;
        let opts_c = CString::new(v.to_string())?;
        unsafe {
            let ctx = f(std::ptr::null(), dir.as_ptr(), opts_c.as_ptr());
            if ctx.is_null() {
                let err = self.last_error_ptr(std::ptr::null_mut());
                bail!("flexicorp_pando_open_opts failed: {err}");
            }
            Ok(ctx)
        }
    }

    pub fn close(&self, ctx: *mut c_void) {
        if !ctx.is_null() {
            unsafe { (self.close)(ctx) }
        }
    }

    /// In-flight requests + background totals (api_version >= 3).
    pub fn busy(&self, ctx: *mut c_void) -> usize {
        match self.busy_fn {
            Some(f) => unsafe { f(ctx) },
            None => 0,
        }
    }

    pub fn idle_seconds(&self, ctx: *mut c_void) -> f64 {
        match self.idle_seconds_fn {
            Some(f) => unsafe { f(ctx) },
            None => f64::MAX,
        }
    }

    pub fn request(
        &self,
        ctx: *mut c_void,
        method: &str,
        path: &str,
        query: &str,
        body: &str,
    ) -> Result<(i32, Value)> {
        let method_c = CString::new(method)?;
        let path_c = CString::new(path)?;
        let query_c = CString::new(query)?;
        let body_c = CString::new(body)?;
        let mut status: c_int = 500;
        unsafe {
            let ptr = (self.request)(
                ctx,
                method_c.as_ptr(),
                path_c.as_ptr(),
                query_c.as_ptr(),
                body_c.as_ptr(),
                &mut status,
            );
            if ptr.is_null() {
                let err = self.last_error_ptr(ctx);
                bail!("flexicorp_pando_request returned null: {err}");
            }
            let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
            (self.free)(ptr.cast());
            let val: Value = serde_json::from_str(&s)
                .with_context(|| format!("pando response is not JSON (status {status}): {s}"))?;
            Ok((status as i32, val))
        }
    }

    /// The project's XML around each `(start, end)` corpus span, from its xidx: one entry per
    /// span, `None` where xidx has none (or the library cannot do it).
    pub fn xidx_fragments(
        &self,
        ctx: *mut c_void,
        project_root: &Path,
        spans: &[(i64, i64)],
        context_scope: &str,
        context: i32,
    ) -> Result<Vec<Option<(Option<String>, String)>>> {
        let Some(f) = self.xidx_fragments_fn else {
            return Ok(vec![None; spans.len()]);
        };
        let root_c = CString::new(project_root.to_string_lossy().as_bytes())?;
        let scope_c = CString::new(context_scope)?;
        let flat: Vec<i64> = spans.iter().flat_map(|(a, b)| [*a, *b]).collect();
        unsafe {
            let ptr = f(ctx, root_c.as_ptr(), flat.as_ptr(), spans.len(), scope_c.as_ptr(), context as c_int);
            if ptr.is_null() {
                let err = self.last_error_ptr(ctx);
                bail!("flexicorp_pando_xidx_fragments returned null: {err}");
            }
            let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
            (self.free)(ptr.cast());
            let v: Value = serde_json::from_str(&s).context("xidx fragments are not JSON")?;
            let arr = v.get("fragments").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut out = Vec::with_capacity(spans.len());
            for i in 0..spans.len() {
                out.push(arr.get(i).and_then(|f| {
                    let frag = f.get("fragment")?.as_str()?.to_string();
                    let doc = f.get("doc_id").and_then(Value::as_str).map(str::to_string);
                    Some((doc, frag))
                }));
            }
            Ok(out)
        }
    }

    fn last_error_ptr(&self, ctx: *mut c_void) -> String {
        unsafe {
            let p = (self.last_error)(ctx);
            if p.is_null() {
                return "(no error string)".to_string();
            }
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// A symbol the library must have; its absence means an old or foreign build.
unsafe fn required<T: Copy>(lib: &Library, name: &str) -> Result<T> {
    let sym: Symbol<T> = unsafe { lib.get(format!("{name}\0").as_bytes()) }
        .map_err(|e| anyhow!("missing symbol {name}, an old or incompatible build ({e})"))?;
    Ok(*sym)
}

/// flexicorp_pando's build folder in a flexitools checkout next to this crate (development only).
pub fn dev_build_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../flexencoder/flexicorp_pando/build")
}

/// Where to look when FLEXICORP_PANDO_LIB is not set. The build-tree path is compiled in and
/// so only exists on the machine fqs was built on; releases rely on the executable's folder
/// (install.sh puts the library there) or FLEXICORP_PANDO_LIB.
fn lib_candidates() -> Vec<PathBuf> {
    let name = if cfg!(target_os = "macos") {
        "libflexicorp_pando.dylib"
    } else if cfg!(target_os = "windows") {
        "flexicorp_pando.dll"
    } else {
        "libflexicorp_pando.so"
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(name));
        }
    }
    candidates.push(dev_build_dir().join(name));
    candidates.push(PathBuf::from("/usr/local/lib").join(name));
    candidates.push(PathBuf::from("/usr/lib").join(name));
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_is_no_library_is_reported_not_skipped() {
        let dir = std::env::temp_dir().join(format!("fqs-pando-lib-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("libflexicorp_pando.so");
        std::fs::write(&fake, b"not a library").unwrap();
        let err = PandoLib::load_from(&fake).err().expect("a text file must not load");
        assert!(format!("{err:#}").contains("cannot be loaded"), "{err:#}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reason_only_when_the_library_is_not_in_use() {
        // STATUS is process-wide, so this test is the only one that sets it
        assert_eq!(unavailable_reason(), None);
        set_status(json!({"mode": "cli", "error": "x: missing symbol flexicorp_pando_build_string"}));
        assert_eq!(unavailable_reason().as_deref(), Some("x: missing symbol flexicorp_pando_build_string"));
        assert_eq!(status()["mode"], "cli");
    }
}
