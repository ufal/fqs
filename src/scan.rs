//! Filesystem corpus discovery for the admin scan API.
//!
//! Frontend-agnostic: roots may be TEITOK projects, bare pando indexes, CWB
//! registries, or Manatee/KonText data dirs. Defaults come from env, fqs.json,
//! catalog path parents, and a few well-known locations — no TEITOK-only assumption.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct ScanRootUsed {
    pub path: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanCandidate {
    pub kind: String,
    pub path: String,
    pub suggested_id: String,
    pub label: String,
    pub project_root: String,
    pub preferred_backend: String,
    pub source_kind: String,
    pub settings: Value,
    /// How this relates to the catalog: `new` | `registered` | `alias`
    pub status: String,
    /// Catalog corpus id when registered or alias
    pub matched_corpus_id: Option<String>,
    pub match_reason: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanReport {
    pub ok: bool,
    pub roots: Vec<ScanRootUsed>,
    pub candidates: Vec<ScanCandidate>,
    pub skipped_outside: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct CatalogFingerprint {
    id: String,
    project_root: PathBuf,
    index_paths: Vec<PathBuf>,
}

/// Resolve scan roots.
///
/// Without request paths: env (`FQS_SCAN_ROOTS`) → `fqs.json` → catalog parents → defaults.
/// With request paths: each path must lie under the **configured allowlist**
/// (`FQS_SCAN_ROOTS` and/or `fqs.json` `scan_roots` when set; otherwise the same
/// default/catalog resolution). Request paths never expand the allowlist.
pub fn resolve_scan_roots(
    explicit: Option<&[String]>,
    catalog_project_roots: &[PathBuf],
) -> Vec<ScanRootUsed> {
    let allow = configured_scan_allowlist(catalog_project_roots);
    if let Some(paths) = explicit {
        let mut out: Vec<ScanRootUsed> = Vec::new();
        let mut seen = HashSet::new();
        let allow_canons: Vec<(PathBuf, String)> = allow
            .iter()
            .filter_map(|r| {
                let p = PathBuf::from(&r.path);
                Some((p.canonicalize().unwrap_or(p), r.path.clone()))
            })
            .collect();
        for p in paths {
            let t = p.trim();
            if t.is_empty() {
                continue;
            }
            let req = PathBuf::from(t);
            let req_canon = req.canonicalize().unwrap_or_else(|_| req.clone());
            let under = allow_canons.iter().find(|(a, _)| {
                req_canon.starts_with(a) || a.starts_with(&req_canon)
            });
            if under.is_some() {
                push_scan_root(&mut out, &mut seen, req, "request∩allowlist");
            }
        }
        return out;
    }
    allow
}

/// Roots an admin may scan (and that request roots must intersect).
pub fn configured_scan_allowlist(catalog_project_roots: &[PathBuf]) -> Vec<ScanRootUsed> {
    let mut out: Vec<ScanRootUsed> = Vec::new();
    let mut seen = HashSet::new();
    let mut configured = false;

    if let Ok(env) = std::env::var("FQS_SCAN_ROOTS") {
        for part in env.split(|c| c == ':' || c == ';') {
            let t = part.trim();
            if !t.is_empty() {
                configured = true;
                push_scan_root(&mut out, &mut seen, PathBuf::from(t), "FQS_SCAN_ROOTS");
            }
        }
    }

    if let Ok(s) = fs::read_to_string(fqs_config_path()) {
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            if let Some(arr) = v.get("scan_roots").and_then(|x| x.as_array()) {
                for item in arr {
                    let path = item
                        .as_str()
                        .or_else(|| item.get("path").and_then(|p| p.as_str()))
                        .map(str::trim)
                        .filter(|s| !s.is_empty());
                    if let Some(p) = path {
                        configured = true;
                        push_scan_root(&mut out, &mut seen, PathBuf::from(p), "fqs.json");
                    }
                }
            }
        }
    }

    // Explicit operator allowlist: do not fall through to broad defaults for intersection.
    if configured {
        return out;
    }

    for root in catalog_project_roots {
        if let Some(parent) = root.parent() {
            push_scan_root(
                &mut out,
                &mut seen,
                parent.to_path_buf(),
                "catalog_parent",
            );
        }
        push_scan_root(&mut out, &mut seen, root.clone(), "catalog_root");
    }

    for (p, src) in default_root_candidates() {
        push_scan_root(&mut out, &mut seen, p, src);
    }

    out
}

/// Whether `path` is under any allowlisted scan root (for reporting rejected request roots).
pub fn root_allowed_under(path: &str, allow: &[ScanRootUsed]) -> bool {
    let req = PathBuf::from(path.trim());
    let req_canon = req.canonicalize().unwrap_or(req);
    allow.iter().any(|r| {
        let a = PathBuf::from(&r.path);
        let a_canon = a.canonicalize().unwrap_or(a);
        req_canon.starts_with(&a_canon) || a_canon.starts_with(&req_canon)
    })
}

fn push_scan_root(out: &mut Vec<ScanRootUsed>, seen: &mut HashSet<String>, path: PathBuf, source: &str) {
    let Ok(canon) = path.canonicalize() else {
        if path.is_dir() {
            let key = path.display().to_string();
            if seen.insert(key.clone()) {
                out.push(ScanRootUsed {
                    path: key,
                    source: source.to_string(),
                });
            }
        }
        return;
    };
    if !canon.is_dir() {
        return;
    }
    let key = canon.display().to_string();
    if seen.insert(key.clone()) {
        out.push(ScanRootUsed {
            path: key,
            source: source.to_string(),
        });
    }
}

fn fqs_config_path() -> PathBuf {
    std::env::var("FQS_CONFIG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/fqs/fqs.json"))
}

fn default_root_candidates() -> Vec<(PathBuf, &'static str)> {
    let mut v = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let push = |v: &mut Vec<(PathBuf, &'static str)>, p: PathBuf, src: &'static str| {
        if p.is_dir() {
            v.push((p, src));
        }
    };
    push(&mut v, PathBuf::from("/srv/teitok"), "default");
    push(&mut v, PathBuf::from("/srv/corpora"), "default");
    push(&mut v, PathBuf::from("/var/lib/manatee"), "default");
    push(&mut v, PathBuf::from("/var/lib/manatee/corpora"), "default");
    push(&mut v, PathBuf::from("/usr/local/share/manatee"), "default");
    push(&mut v, PathBuf::from("/data/pando"), "default");
    push(&mut v, PathBuf::from("/data/corpora"), "default");
    if let Some(h) = &home {
        push(&mut v, h.join("corpora"), "default");
    }
    if let Ok(reg) = std::env::var("CWB_REGISTRY") {
        let p = PathBuf::from(reg.trim());
        if p.is_dir() {
            v.push((p.clone(), "CWB_REGISTRY"));
            if let Some(parent) = p.parent() {
                push(&mut v, parent.to_path_buf(), "CWB_REGISTRY_parent");
            }
        }
    }
    if let Ok(reg) = std::env::var("MANATEE_REGISTRY") {
        let p = PathBuf::from(reg.trim());
        if p.is_dir() {
            v.push((p, "MANATEE_REGISTRY"));
        }
    }
    v
}

pub fn scan_filesystem(
    roots: &[ScanRootUsed],
    catalog: &[CatalogFingerprint],
    max_depth: u32,
    max_candidates: usize,
) -> ScanReport {
    let mut candidates: Vec<ScanCandidate> = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut skipped_outside = 0usize;
    let mut truncated = false;

    let root_canons: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| PathBuf::from(&r.path).canonicalize().ok())
        .collect();

    for root in roots {
        let root_path = PathBuf::from(&root.path);
        walk(
            &root_path,
            &root_path,
            0,
            max_depth,
            &root_canons,
            &mut skipped_outside,
            &mut seen_paths,
            &mut candidates,
            max_candidates,
            &mut truncated,
        );
        if truncated {
            break;
        }
    }

    for c in &mut candidates {
        match_catalog(c, catalog);
    }

    candidates.sort_by(|a, b| {
        status_rank(&a.status)
            .cmp(&status_rank(&b.status))
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.suggested_id.cmp(&b.suggested_id))
    });

    ScanReport {
        ok: true,
        roots: roots.to_vec(),
        candidates,
        skipped_outside,
        truncated,
    }
}

fn status_rank(s: &str) -> u8 {
    match s {
        "new" => 0,
        "alias" => 1,
        _ => 2,
    }
}

pub fn catalog_fingerprints(
    entries: &[(String, PathBuf, Value)],
) -> Vec<CatalogFingerprint> {
    entries
        .iter()
        .map(|(id, project_root, settings)| {
            let mut index_paths = Vec::new();
            for k in ["index_path", "index_dir", "pando_index"] {
                if let Some(p) = settings.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
                {
                    index_paths.push(normalize_existing(PathBuf::from(p)));
                }
            }
            let root = normalize_existing(project_root.clone());
            let pando = root.join("pando");
            if pando.is_dir() {
                index_paths.push(normalize_existing(pando));
            }
            index_paths.push(root.clone());
            if looks_like_pando_index(&root) {
                index_paths.push(root.clone());
            }
            CatalogFingerprint {
                id: id.clone(),
                project_root: root,
                index_paths,
            }
        })
        .collect()
}

fn normalize_existing(p: PathBuf) -> PathBuf {
    p.canonicalize().unwrap_or(p)
}

fn path_under_any_root(path: &Path, roots: &[PathBuf]) -> bool {
    let Ok(canon) = path.canonicalize() else {
        return roots.iter().any(|r| path.starts_with(r));
    };
    roots.iter().any(|r| canon.starts_with(r))
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: u32,
    max_depth: u32,
    root_canons: &[PathBuf],
    skipped_outside: &mut usize,
    seen_paths: &mut HashSet<String>,
    candidates: &mut Vec<ScanCandidate>,
    max_candidates: usize,
    truncated: &mut bool,
) {
    if *truncated || candidates.len() >= max_candidates {
        *truncated = true;
        return;
    }
    if !path_under_any_root(dir, root_canons) && depth > 0 {
        *skipped_outside += 1;
        return;
    }

    if let Some(c) = classify_dir(dir) {
        let key = c.path.clone();
        if seen_paths.insert(key) {
            candidates.push(c);
            if candidates.len() >= max_candidates {
                *truncated = true;
                return;
            }
        }
        // Still descend into TEITOK-like trees to find nested pando/cqp, but skip
        // descending into a bare pando index (leaf).
        if looks_like_pando_index(dir) && !looks_like_project_bundle(dir) {
            return;
        }
    }

    if depth >= max_depth {
        return;
    }
    let rd = match fs::read_dir(dir) {
        Ok(x) => x,
        Err(_) => return,
    };
    for ent in rd.flatten() {
        let p = ent.path();
        if !p.is_dir() {
            continue;
        }
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" || name == "target" || name == "tmp" {
            continue;
        }
        walk(
            root,
            &p,
            depth + 1,
            max_depth,
            root_canons,
            skipped_outside,
            seen_paths,
            candidates,
            max_candidates,
            truncated,
        );
        if *truncated {
            return;
        }
    }
}

fn classify_dir(dir: &Path) -> Option<ScanCandidate> {
    let mut notes = Vec::new();
    let bundle = looks_like_project_bundle(dir);
    let pando = looks_like_pando_index(dir) || dir.join("pando").is_dir() && looks_like_pando_index(&dir.join("pando"));
    let cqp = dir.join("cqp").is_dir() || looks_like_cqp_registry_dir(dir);
    let manatee = looks_like_manatee_corpus_dir(dir) || dir.join("manatee").is_dir();

    if !bundle && !pando && !cqp && !manatee && !looks_like_pando_index(dir) {
        // Registry file directory: many small corpus descriptors
        if looks_like_cqp_registry_dir(dir) {
            return Some(candidate_for_registry(dir));
        }
        return None;
    }

    let id = suggested_id(dir);
    let mut settings = json!({});
    let (kind, backend, source_kind, project_root) = if bundle {
        notes.push("project-style tree (xml/Scripts/index or multi-backend dirs)".into());
        if dir.join("pando").is_dir() {
            settings
                .as_object_mut()
                .unwrap()
                .insert("index_dir".into(), json!(dir.join("pando").display().to_string()));
        }
        (
            if dir.join("pando").is_dir() {
                "project+pando"
            } else if dir.join("cqp").is_dir() {
                "project+cqp"
            } else if dir.join("manatee").is_dir() {
                "project+manatee"
            } else {
                "project"
            },
            if dir.join("pando").is_dir() {
                "pando"
            } else if dir.join("cqp").is_dir() {
                "cqp"
            } else {
                "auto"
            },
            "filesystem_scan",
            dir.to_path_buf(),
        )
    } else if looks_like_pando_index(dir) {
        notes.push("pando index (corpus.info)".into());
        settings
            .as_object_mut()
            .unwrap()
            .insert("index_dir".into(), json!(dir.display().to_string()));
        let project = dir
            .parent()
            .filter(|p| looks_like_project_bundle(p))
            .unwrap_or(dir)
            .to_path_buf();
        ("pando_index", "pando", "pando_index", project)
    } else if looks_like_manatee_corpus_dir(dir) {
        notes.push("manatee-like corpus data dir".into());
        ("manatee_data", "auto", "manatee", dir.to_path_buf())
    } else if cqp {
        notes.push("cqp / cwb layout".into());
        ("cqp", "cqp", "cqp", dir.to_path_buf())
    } else {
        return None;
    };

    Some(ScanCandidate {
        kind: kind.to_string(),
        path: dir.display().to_string(),
        suggested_id: id.clone(),
        label: id.replace('_', " "),
        project_root: project_root.display().to_string(),
        preferred_backend: backend.to_string(),
        source_kind: source_kind.to_string(),
        settings,
        status: "new".into(),
        matched_corpus_id: None,
        match_reason: None,
        notes,
    })
}

fn candidate_for_registry(dir: &Path) -> ScanCandidate {
    let id = suggested_id(dir);
    ScanCandidate {
        kind: "cwb_registry".into(),
        path: dir.display().to_string(),
        suggested_id: id.clone(),
        label: format!("CWB registry {}", id),
        project_root: dir.display().to_string(),
        preferred_backend: "cqp".into(),
        source_kind: "cwb_registry".into(),
        settings: json!({"registry_hint": dir.display().to_string()}),
        status: "new".into(),
        matched_corpus_id: None,
        match_reason: None,
        notes: vec!["directory looks like a CWB/CQP registry".into()],
    }
}

fn suggested_id(dir: &Path) -> String {
    let name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("corpus")
        .to_string();
    if name == "pando" || name == "cqp" || name == "manatee" || name == "data" {
        if let Some(parent) = dir.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()) {
            return sanitize_id(parent);
        }
    }
    sanitize_id(&name)
}

fn sanitize_id(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c.to_ascii_lowercase());
        } else if c == ' ' || c == '.' {
            out.push('_');
        }
    }
    if out.is_empty() {
        "corpus".into()
    } else {
        out
    }
}

fn looks_like_pando_index(dir: &Path) -> bool {
    dir.join("corpus.info").is_file()
}

fn looks_like_project_bundle(dir: &Path) -> bool {
    let markers = [
        "xmlfiles",
        "Scripts",
        "index.php",
        "cqpsettings.xml",
        "Pages",
    ];
    let has_marker = markers.iter().any(|m| dir.join(m).exists());
    let backends = ["pando", "cqp", "manatee", "xidx"]
        .iter()
        .filter(|m| dir.join(*m).is_dir())
        .count();
    has_marker || backends >= 2
}

fn looks_like_cqp_registry_dir(dir: &Path) -> bool {
    // Heuristic: several extensionless or `.info` registry files, no corpus.info
    if looks_like_pando_index(dir) || looks_like_project_bundle(dir) {
        return false;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    let mut n = 0;
    for ent in rd.flatten().take(40) {
        let p = ent.path();
        if p.is_file() {
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            // CWB registry entries are often named CORPUSNAME with no extension
            if !name.contains('.') || name.ends_with(".info") {
                if fs::read_to_string(&p)
                    .map(|s| s.contains("NAME=") || s.contains("HOME=") || s.contains("INFO "))
                    .unwrap_or(false)
                {
                    n += 1;
                }
            }
        }
    }
    n >= 2
}

fn looks_like_manatee_corpus_dir(dir: &Path) -> bool {
    // Typical Manatee vertical/indexed corpus: .corpus or sizes + lexicon files
    dir.join(".corpus").is_file()
        || (dir.join("word.lex").is_file() && dir.join("word.lex.s").is_file())
        || (dir.join("attr.sizes").is_file())
}

fn match_catalog(c: &mut ScanCandidate, catalog: &[CatalogFingerprint]) {
    let path = normalize_existing(PathBuf::from(&c.path));
    let project = normalize_existing(PathBuf::from(&c.project_root));
    let index = c
        .settings
        .get("index_dir")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .map(normalize_existing);

    for entry in catalog {
        // Exact same project root or index path → registered (same catalog path)
        if paths_equal(&project, &entry.project_root) || paths_equal(&path, &entry.project_root) {
            c.status = "registered".into();
            c.matched_corpus_id = Some(entry.id.clone());
            c.match_reason = Some("same project_root".into());
            return;
        }
        for ip in &entry.index_paths {
            if paths_equal(&path, ip) || index.as_ref().is_some_and(|i| paths_equal(i, ip)) {
                if paths_equal(&project, &entry.project_root) {
                    c.status = "registered".into();
                    c.match_reason = Some("same index path".into());
                } else {
                    c.status = "alias".into();
                    c.match_reason = Some(format!(
                        "same index data as '{}' (different project_root path)",
                        entry.id
                    ));
                }
                c.matched_corpus_id = Some(entry.id.clone());
                return;
            }
        }
        // Same suggested id but different path → likely alias / duplicate registration candidate
        if c.suggested_id == entry.id && !paths_equal(&project, &entry.project_root) {
            c.status = "alias".into();
            c.matched_corpus_id = Some(entry.id.clone());
            c.match_reason = Some(format!(
                "suggested id matches catalog id '{}' but paths differ",
                entry.id
            ));
            return;
        }
    }
    c.status = "new".into();
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Build a catalog-ready upsert object from a scan candidate.
pub fn candidate_to_entry_json(c: &ScanCandidate) -> Value {
    json!({
        "id": c.suggested_id,
        "label": c.label,
        "project_root": c.project_root,
        "preferred_backend": c.preferred_backend,
        "source_kind": c.source_kind,
        "environment": "live",
        "http_policy_mode": "public_query",
        "http_allowed_operations": ["query", "catalog"],
        "interfaces": ["query"],
        "settings": c.settings,
        "is_current": true,
        "supports_xml": c.kind.starts_with("project"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_pando_index() {
        let dir = tempfile_dir();
        fs::write(dir.join("corpus.info"), "test\n").unwrap();
        assert!(looks_like_pando_index(&dir));
        let c = classify_dir(&dir).expect("candidate");
        assert_eq!(c.preferred_backend, "pando");
    }

    #[test]
    fn alias_when_same_index_different_root() {
        let idx = tempfile_dir();
        fs::write(idx.join("corpus.info"), "x\n").unwrap();
        let catalog = vec![CatalogFingerprint {
            id: "demo".into(),
            project_root: PathBuf::from("/other/demo"),
            index_paths: vec![idx.canonicalize().unwrap()],
        }];
        let mut c = classify_dir(&idx).unwrap();
        match_catalog(&mut c, &catalog);
        assert_eq!(c.status, "alias");
        assert_eq!(c.matched_corpus_id.as_deref(), Some("demo"));
    }

    fn tempfile_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("fqs-scan-test-{}", std::process::id()));
        p.push(format!("{}", rand_suffix()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn rand_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
    }
}
