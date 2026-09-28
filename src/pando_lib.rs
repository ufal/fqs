//! Dynamic loader for libflexicorp_pando (C ABI).
//!
//! api_version >= 2: flexicorp_pando_request
//! api_version >= 3: busy, idle_seconds, build_json (ServerApi embedding contract)

use anyhow::{anyhow, bail, Context, Result};
use libloading::{Library, Symbol};
use serde_json::Value;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_double, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;

type ApiVersionFn = unsafe extern "C" fn() -> c_int;
type BuildStringFn = unsafe extern "C" fn() -> *const c_char;
type BuildJsonFn = unsafe extern "C" fn() -> *const c_char;
type OpenFn = unsafe extern "C" fn(*const c_char, *const c_char, c_int) -> *mut c_void;
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

pub struct PandoLib {
    _lib: Library,
    build_string_fn: BuildStringFn,
    build_json_fn: Option<BuildJsonFn>,
    open: OpenFn,
    close: CloseFn,
    request: RequestFn,
    free: FreeFn,
    last_error: LastErrorFn,
    busy_fn: Option<BusyFn>,
    idle_seconds_fn: Option<IdleSecondsFn>,
    cached_api_version: i32,
}

impl PandoLib {
    pub fn load() -> Result<Arc<Self>> {
        let path = resolve_lib_path().context("could not locate libflexicorp_pando")?;
        // SAFETY: documented C ABI from flexicorp_pando.h
        let lib = unsafe { Library::new(&path) }
            .with_context(|| format!("failed to load {}", path.display()))?;
        unsafe {
            let api_version_fn: Symbol<ApiVersionFn> = lib.get(b"flexicorp_pando_api_version\0")?;
            let build_string_fn: Symbol<BuildStringFn> = lib.get(b"flexicorp_pando_build_string\0")?;
            let open: Symbol<OpenFn> = lib.get(b"flexicorp_pando_open\0")?;
            let close: Symbol<CloseFn> = lib.get(b"flexicorp_pando_close\0")?;
            let request: Symbol<RequestFn> = lib.get(b"flexicorp_pando_request\0")?;
            let free: Symbol<FreeFn> = lib.get(b"flexicorp_pando_free\0")?;
            let last_error: Symbol<LastErrorFn> = lib.get(b"flexicorp_pando_last_error\0")?;
            let api_version_fn = *api_version_fn;
            let ver = api_version_fn();
            if ver < 2 {
                bail!(
                    "libflexicorp_pando at {} reports api_version {} (need >= 2 for flexicorp_pando_request)",
                    path.display(),
                    ver
                );
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
            let idle_seconds_fn = if ver >= 3 {
                lib.get::<IdleSecondsFn>(b"flexicorp_pando_idle_seconds\0")
                    .ok()
                    .map(|s| *s)
            } else {
                None
            };
            if ver >= 3 && (busy_fn.is_none() || idle_seconds_fn.is_none()) {
                bail!(
                    "libflexicorp_pando at {} claims api_version {} but missing busy/idle_seconds",
                    path.display(),
                    ver
                );
            }
            Ok(Arc::new(Self {
                build_string_fn: *build_string_fn,
                build_json_fn,
                open: *open,
                close: *close,
                request: *request,
                free: *free,
                last_error: *last_error,
                busy_fn,
                idle_seconds_fn,
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

fn resolve_lib_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("FLEXICORP_PANDO_LIB") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Ok(pb);
        }
    }
    let names = if cfg!(target_os = "macos") {
        vec!["libflexicorp_pando.dylib"]
    } else if cfg!(target_os = "windows") {
        vec!["flexicorp_pando.dll"]
    } else {
        vec!["libflexicorp_pando.so"]
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for n in &names {
                candidates.push(dir.join(n));
            }
        }
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for n in &names {
        candidates.push(manifest_dir.join("../flexicorp_pando/build").join(n));
        candidates.push(PathBuf::from("/usr/local/lib").join(n));
        candidates.push(PathBuf::from("/usr/lib").join(n));
    }
    for c in candidates {
        if c.is_file() {
            return Ok(c);
        }
    }
    bail!("set FLEXICORP_PANDO_LIB to libflexicorp_pando")
}
