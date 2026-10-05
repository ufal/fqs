//! Frontend modules: what FQS can do *for* a frontend (KonText, and later Korp, CQPweb,
//! NoSketch Engine, …): find the corpora it lacks, and publish a corpus to it — the
//! frontend's own configuration files, written safely.
//!
//! Core FQS (services.rs) only knows the interface below. A module gets the frontend's
//! fqs.json entry (or one it discovers on this machine) and the catalogue rows; to add a
//! frontend, implement [`FrontendModule`] in a file next to `kontext.rs` and list it in
//! [`modules`].

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub mod kontext;

/// One catalogue row, as frontend modules see it.
pub struct CatalogCorpus<'a> {
    pub id: &'a str,
    pub label: &'a str,
    pub preferred_backend: &'a str,
    pub is_current: bool,
    pub http_policy_mode: &'a str,
    pub interface_preference: Option<&'a str>,
    pub source_kind: &'a str,
    pub supports_xml: bool,
    pub project_root: Option<&'a str>,
    pub project_url: Option<&'a str>,
    pub settings: &'a Value,
    pub capabilities: &'a Value,
}

/// Publishing one catalogue corpus to a frontend.
pub struct PublishRequest<'a> {
    /// the frontend's name for the corpus (KonText: corplist ident); None: the module's suggestion
    pub name: Option<&'a str>,
    pub corpus_id: &'a str,
    pub label: &'a str,
    pub description: Option<&'a str>,
    pub language: Option<&'a str>,
    /// the corpus's Pando index (corpus.info), when it has one
    pub index_dir: Option<PathBuf>,
    /// where the frontend reaches this FQS (unless its fqs.json entry says `fqs_url`)
    pub fqs_url: &'a str,
    /// module-specific options from the request body (`options`)
    pub options: &'a Value,
}

/// What a frontend module implements. Reports are JSON so that the admin UI can show any
/// frontend the same way:
///
/// - coverage: `{frontend_id, kind, label, url, steps: [[key, label], …], missing: [{id,
///   label, preferred_backend, suggested_name, steps: {key: true|false|null}, …}], hints,
///   files: [[label, path], …], publishable, restartable}`
/// - publish: `{ok, name, steps: [{key, label, status, path?, message?}], complete,
///   restart_needed, restartable, catalog_settings: {block: {key: value}}}` — FQS merges
///   `catalog_settings` into the corpus's settings (e.g. KonText's name for it).
pub trait FrontendModule: Sync {
    fn kind(&self) -> &'static str;
    fn label(&self) -> &'static str;
    /// A frontend of this kind found on this machine when fqs.json lists none.
    fn discover(&self, _corpora: &[CatalogCorpus<'_>]) -> Option<Value> {
        None
    }
    /// Its processes, for the admin's frontend card.
    fn processes(&self) -> Vec<Value> {
        Vec::new()
    }
    fn coverage(&self, frontend_id: &str, cfg: &Value, corpora: &[CatalogCorpus<'_>]) -> Value;
    fn publish(&self, frontend_id: &str, cfg: Option<&Value>, req: &PublishRequest<'_>) -> Result<Value, String>;
}

/// The frontend modules FQS has.
static MODULES: &[&dyn FrontendModule] = &[&kontext::KONTEXT];

pub fn modules() -> &'static [&'static dyn FrontendModule] {
    MODULES
}

pub fn module_for(kind: &str) -> Option<&'static dyn FrontendModule> {
    let k = kind.trim().to_ascii_lowercase();
    modules().iter().copied().find(|m| m.kind() == k)
}

/// A publish step for reports.
pub(crate) fn step(key: &str, label: &str, mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.insert("key".into(), json!(key));
        o.insert("label".into(), json!(label));
    }
    v
}

// ---- Publishing corpora to KonText ------------------------------------------------
//
// A corpus shows up and opens in KonText (kontext-pando) when:
//   1. KonText's corplist.xml lists it (<corpus ident="…"/>);
//   2. kontext-pando's pando_corpora.json maps that ident to a Pando backend (here: this
//      FQS, `"backend": "fqs"`), else KonText only knows Manatee corpora;
//   3. Manatee has a registry file for the ident (kontext-pando still opens a Manatee
//      corpus object for metadata) — FQS only checks this, it cannot build one;
//   4. KonText has been restarted (it reads its corplist at start-up), and users have
//      access to the corpus in KonText's auth.
// FQS edits only the two files of 1 and 2, and only at paths that come from fqs.json, the
// environment, or KonText's own config.xml — never from the request.

pub(crate) fn cfg_str<'a>(cfg: Option<&'a Value>, keys: &[&str]) -> Option<&'a str> {
    let cfg = cfg?;
    keys.iter().find_map(|k| {
        cfg.get(*k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    })
}


pub(crate) fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}


pub(crate) fn push_unique_path(v: &mut Vec<(PathBuf, &'static str)>, p: PathBuf, src: &'static str) {
    if !v.iter().any(|(x, _)| x == &p) {
        v.push((p, src));
    }
}


/// Byte ranges of <!-- … --> comments.
pub(crate) fn xml_comment_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while from < text.len() {
        let Some(s) = text[from..].find("<!--") else { break };
        let start = from + s;
        let end = text[start + 4..]
            .find("-->")
            .map(|e| start + 4 + e + 3)
            .unwrap_or(text.len());
        out.push((start, end));
        from = end;
    }
    out
}


pub(crate) fn in_ranges(r: &[(usize, usize)], pos: usize) -> bool {
    r.iter().any(|(a, b)| pos >= *a && pos < *b)
}


pub(crate) fn attr_value_from_tag<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(&format!("{name}=")) {
        let idx = from + rel;
        from = idx + 1;
        // a whole attribute name, not the end of another (e.g. "corpus_ident=")
        if idx > 0 && !lower[..idx].ends_with(char::is_whitespace) {
            continue;
        }
        let rest = tag[idx + name.len() + 1..].trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let inner = &rest[1..];
        let end = inner.find(quote)?;
        return Some(&inner[..end]);
    }
    None
}


pub(crate) fn xml_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}


/// Serialises FQS's own edits of frontend configuration files.
pub(crate) static FILES_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());


/// Replace a configuration file safely: through symlinks to the real file, with a
/// backup, keeping its mode and (when possible) owner, atomically when its directory
/// is writable, else in place.
pub(crate) fn rewrite_config_file(path: &Path, text: &str) -> Result<Value, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let target = path
        .canonicalize()
        .map_err(|e| format!("Cannot resolve {}: {e}", path.display()))?;
    let meta = fs::metadata(&target).map_err(|e| format!("Cannot stat {}: {e}", target.display()))?;
    let writable = fs::OpenOptions::new().write(true).open(&target).is_ok();
    let dir = target.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // backup: next to the file, else in FQS's own backup folder
    let mut backup = None;
    let mut bdirs = vec![dir.clone()];
    if let Some(d) = env_path("FQS_BACKUP_DIR") {
        bdirs.push(d);
    }
    bdirs.push(PathBuf::from("/var/lib/fqs/backups"));
    for d in bdirs {
        let b = d.join(format!("{name}.fqs-bak-{stamp}"));
        if (d.is_dir() || fs::create_dir_all(&d).is_ok()) && fs::copy(&target, &b).is_ok() {
            backup = Some(b);
            break;
        }
    }

    let fail_hint = |e: &dyn std::fmt::Display| {
        format!(
            "Cannot write {}: {e}. FQS needs write access to this file (best also to its folder, for an atomic replace).",
            target.display()
        )
    };

    let tmp = dir.join(format!(".{name}.fqs-tmp-{}", std::process::id()));
    if fs::write(&tmp, text).is_ok() {
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(meta.mode() & 0o7777));
        let _ = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()));
        let owner_kept = fs::metadata(&tmp).map(|m| m.uid() == meta.uid()).unwrap_or(false);
        if owner_kept || !writable {
            return match fs::rename(&tmp, &target) {
                Ok(()) => Ok(json!({
                    "path": target.display().to_string(),
                    "backup": backup.map(|b| b.display().to_string()),
                    "method": "replace",
                    "owner_kept": owner_kept,
                })),
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    Err(fail_hint(&e))
                }
            };
        }
        // replacing would change the owner: write the file itself instead
        let _ = fs::remove_file(&tmp);
    }
    if !writable {
        return Err(fail_hint(&"permission denied"));
    }
    fs::write(&target, text).map_err(|e| fail_hint(&e))?;
    Ok(json!({
        "path": target.display().to_string(),
        "backup": backup.map(|b| b.display().to_string()),
        "method": "in_place",
        "owner_kept": true,
    }))
}


// ---- Manatee registry "shell" for kontext-pando -------------------------------------
//
// KonText opens every corpus as a Manatee corpus (registry file + data folder), also when
// kontext-pando sends its queries to Pando. For a Pando corpus the Manatee side only has to
// open and describe the corpus: its attributes and structures. FQS writes that registry from
// the Pando index's corpus.info, a one-token vertical with the same columns and structures,
// and encodes it with Manatee's `encodevert` (the "shell"); concordances, frequencies and
// text types then come from Pando through FQS.

/// What corpus.info of a Pando index says about the corpus.
#[derive(Debug, Clone, Default)]
pub struct PandoCorpusInfo {
    #[allow(dead_code)] // for modules that need the token count
    pub size: Option<i64>,
    pub positional: Vec<String>,
    pub structural: Vec<String>,
    /// (structure, attribute), from region_attrs like `s_id`, `del_tok_id`
    pub struct_attrs: Vec<(String, String)>,
    pub zerowidth: Vec<String>,
    pub multivalue: Vec<String>,
    pub index_id: Option<String>,
}


pub fn read_pando_corpus_info(index_dir: &Path) -> Option<PandoCorpusInfo> {
    let text = fs::read_to_string(index_dir.join("corpus.info")).ok()?;
    let mut kv: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            kv.insert(k.trim(), v.trim());
        }
    }
    let list = |k: &str| -> Vec<String> {
        kv.get(k)
            .map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
            .unwrap_or_default()
    };
    let structural = list("structural");
    let mut struct_attrs = Vec::new();
    for ra in list("region_attrs") {
        // the longest structure name that prefixes "<struct>_<attr>"
        let best = structural
            .iter()
            .filter(|s| ra.len() > s.len() + 1 && ra.starts_with(s.as_str()) && ra.as_bytes()[s.len()] == b'_')
            .max_by_key(|s| s.len());
        if let Some(s) = best {
            struct_attrs.push((s.clone(), ra[s.len() + 1..].to_string()));
        }
    }
    Some(PandoCorpusInfo {
        size: kv.get("size").and_then(|v| v.parse().ok()),
        positional: list("positional"),
        structural,
        struct_attrs,
        zerowidth: list("zerowidth"),
        multivalue: list("kv_pipe"),
        index_id: kv.get("index_id").map(|s| s.to_string()),
    })
}


/// The Pando index folder of a catalogue row (as FQS resolves it for queries).
pub fn pando_index_dir_for(settings: &Value, project_root: Option<&str>) -> Option<PathBuf> {
    let from_settings = ["index_path", "index_dir", "pando_index"]
        .iter()
        .find_map(|k| settings.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let mut p = match from_settings {
        Some(p) => p,
        None => {
            let root = PathBuf::from(project_root.map(str::trim).filter(|s| !s.is_empty())?);
            if root.join("pando").is_dir() { root.join("pando") } else { root }
        }
    };
    if p.is_file() {
        p = p.parent()?.to_path_buf();
    }
    p.join("corpus.info").is_file().then_some(p)
}


/// Language name for the registry's LANGUAGE (Manatee and KonText use English names).
pub(crate) fn language_name(code: &str) -> Option<&'static str> {
    let names: &[(&str, &str)] = &[
        ("ar", "Arabic"), ("bg", "Bulgarian"), ("ca", "Catalan"), ("cs", "Czech"), ("cy", "Welsh"),
        ("da", "Danish"), ("de", "German"), ("el", "Greek"), ("en", "English"), ("es", "Spanish"),
        ("et", "Estonian"), ("eu", "Basque"), ("fa", "Persian"), ("fi", "Finnish"), ("fr", "French"),
        ("ga", "Irish"), ("gl", "Galician"), ("he", "Hebrew"), ("hi", "Hindi"), ("hr", "Croatian"),
        ("hu", "Hungarian"), ("hy", "Armenian"), ("is", "Icelandic"), ("it", "Italian"), ("ja", "Japanese"),
        ("ka", "Georgian"), ("ko", "Korean"), ("la", "Latin"), ("lt", "Lithuanian"), ("lv", "Latvian"),
        ("mt", "Maltese"), ("nl", "Dutch"), ("no", "Norwegian"), ("pl", "Polish"), ("pt", "Portuguese"),
        ("ro", "Romanian"), ("ru", "Russian"), ("sk", "Slovak"), ("sl", "Slovenian"), ("sq", "Albanian"),
        ("sr", "Serbian"), ("sv", "Swedish"), ("ta", "Tamil"), ("tr", "Turkish"), ("uk", "Ukrainian"),
        ("ur", "Urdu"), ("vi", "Vietnamese"), ("zh", "Chinese"),
    ];
    let c = code.trim().to_ascii_lowercase();
    let c = c.split(['-', '_']).next().unwrap_or("");
    names.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}


/// Corpora a frontend can be given: TEITOK projects, and anything FQS can query (a
/// frontend module that sends its queries to FQS can then serve them).
pub(crate) fn corpus_servable_through_fqs(c: &CatalogCorpus<'_>) -> bool {
    if c.http_policy_mode.trim().eq_ignore_ascii_case("disabled") {
        return false;
    }
    crate::services::corpus_is_teitok_listable(
        c.interface_preference,
        c.source_kind,
        c.supports_xml,
        c.project_root,
        c.project_url,
        c.settings,
        c.capabilities,
    ) || corpus_looks_pando_servable(c)
        || corpus_can_serve_fcs(c)
}


pub(crate) fn corpus_looks_pando_servable(c: &CatalogCorpus<'_>) -> bool {
    let b = c.preferred_backend.trim().to_ascii_lowercase();
    if b == "pando" {
        return true;
    }
    if let Some(arr) = c.settings.get("available_backends").and_then(|v| v.as_array()) {
        if arr.iter().any(|v| v.as_str() == Some("pando")) {
            return true;
        }
    }
    if c.settings
        .get("query_backend")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("pando"))
    {
        return true;
    }
    if let Some(root) = c.project_root.map(str::trim).filter(|s| !s.is_empty()) {
        let p = Path::new(root);
        if p.join("pando").is_dir() || p.join("Resources").join("pando").is_dir() {
            return true;
        }
    }
    false
}


pub(crate) fn corpus_can_serve_fcs(c: &CatalogCorpus<'_>) -> bool {
    if c.http_policy_mode.trim().eq_ignore_ascii_case("disabled") {
        return false;
    }
    let b = c.preferred_backend.trim().to_ascii_lowercase();
    if b == "pando" || b == "cqp" || b == "cwb" {
        return true;
    }
    if let Some(arr) = c.settings.get("available_backends").and_then(|v| v.as_array()) {
        return arr
            .iter()
            .any(|v| matches!(v.as_str(), Some("pando") | Some("cqp") | Some("cwb")));
    }
    corpus_looks_pando_servable(c)
}

