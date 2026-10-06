//! KonText (with kontext-pando) as an FQS frontend module.
//!
//! A corpus shows up and opens in KonText when:
//!   1. KonText's corplist.xml lists it (`<corpus ident="…"/>`);
//!   2. kontext-pando's pando_corpora.json maps that ident to a Pando backend (here: this
//!      FQS, `"backend": "fqs"`), else KonText only knows Manatee corpora;
//!   3. Manatee has a registry file for it: kontext-pando still opens a Manatee corpus to
//!      describe it. FQS builds this "shell" from the Pando index (see below);
//!   4. KonText was restarted (it reads its corplist at start-up), and users have access
//!      to it in KonText's auth.
//! The module writes only files at paths from fqs.json, the environment, or KonText's own
//! config.xml / install folder — never from the request.

use super::*;
use std::process::Command;

pub struct Kontext;
pub static KONTEXT: Kontext = Kontext;

/// KonText processes (Sanic or gunicorn) on this machine, for the KonText card.
pub fn discover_kontext_processes() -> Vec<Value> {
    let Ok(out) = Command::new("pgrep").args(["-af", "gunicorn|sanic|kontext"]).output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let me = std::process::id().to_string();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let mut parts = line.splitn(2, char::is_whitespace);
            let pid = parts.next().unwrap_or("").to_string();
            let cmd = parts.next().unwrap_or("").trim().to_string();
            let lower = cmd.to_ascii_lowercase();
            if pid == me {
                return None;
            }
            // only server processes (python, gunicorn, sanic, workers), not shells or
            // editors that happen to mention kontext
            let exe = lower.split_whitespace().next().unwrap_or("");
            let exe = exe.rsplit('/').next().unwrap_or(exe);
            let server_exe = exe.starts_with("python")
                || matches!(exe, "gunicorn" | "sanic" | "uvicorn" | "celery" | "rq" | "hypercorn");
            if !server_exe {
                return None;
            }
            let server = if lower.contains("gunicorn") {
                "gunicorn"
            } else if lower.contains("sanic") || lower.contains("public/app.py") {
                "sanic"
            } else if lower.contains("celery") || lower.contains("rq ") || lower.contains("bgcalc") {
                "worker"
            } else {
                "other"
            };
            Some(json!({"pid": pid, "cmd": cmd, "server": server}))
        })
        .take(20)
        .collect()
}


/// KonText install roots: the usual places, plus those of running KonText processes;
/// a process-derived root only counts when it has conf/config.xml.
fn kontext_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for r in [
        "/opt/kontext",
        "/opt/kontext/installation",
        "/opt/kontext-pando",
        "/var/www/kontext",
        "/usr/local/share/kontext",
    ] {
        let p = PathBuf::from(r);
        if p.join("conf").is_dir() && !roots.contains(&p) {
            roots.push(p);
        }
    }
    for p in discover_kontext_processes() {
        let cmd = p.get("cmd").and_then(Value::as_str).unwrap_or("");
        for token in cmd.split_whitespace() {
            for marker in ["/conf/", "/public/", "/venv/", "/lib/"] {
                if let Some(idx) = token.find(marker) {
                    let root = PathBuf::from(&token[..idx]);
                    if root.join("conf").join("config.xml").is_file() && !roots.contains(&root) {
                        roots.push(root);
                    }
                }
            }
        }
    }
    roots
}


fn kontext_corplist_candidates() -> Vec<(PathBuf, &'static str)> {
    let mut out = Vec::new();
    for root in kontext_roots() {
        let conf = root.join("conf");
        if let Some(p) = corplist_path_from_kontext_config(&conf.join("config.xml")) {
            push_unique_path(&mut out, p, "config.xml");
        }
        push_unique_path(&mut out, conf.join("corplist.xml"), "discovered");
    }
    out
}


/// Resolve a KonText corplist.xml path: fqs.json → env → KonText config.xml / install.
fn resolve_kontext_corplist(cfg: Option<&Value>) -> (Option<PathBuf>, Option<&'static str>) {
    if let Some(p) = cfg_str(cfg, &["corplist", "corplist_path"]) {
        return (Some(PathBuf::from(p)), Some("fqs.json"));
    }
    if let Some(p) = env_path("FQS_KONTEXT_CORPLIST") {
        return (Some(p), Some("env"));
    }
    for (path, src) in kontext_corplist_candidates() {
        if path.is_file() {
            return (Some(path), Some(src));
        }
    }
    (None, None)
}


fn corplist_path_from_kontext_config(config_xml: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(config_xml).ok()?;
    let comments = xml_comment_ranges(&text);
    // Prefer <file …>…corplist.xml</file> (lindat / tree_corparch).
    let lower = text.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find("<file") {
        let start = search_from + rel;
        let after = &text[start..];
        let Some(close) = after.find("</file>") else {
            break;
        };
        if !in_ranges(&comments, start) {
            let inner = &after[..close];
            if let Some(gt) = inner.find('>') {
                let path = inner[gt + 1..].trim();
                if path.to_ascii_lowercase().contains("corplist") && !path.is_empty() {
                    let p = PathBuf::from(path);
                    let p = if p.is_relative() {
                        config_xml.parent().map(|d| d.join(&p)).unwrap_or(p)
                    } else {
                        p
                    };
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
        search_from = start + close.max(1);
    }
    None
}


/// kontext-pando's pando_corpora.json: fqs.json → env → where kontext-pando looks.
fn resolve_pando_corpora(cfg: Option<&Value>, corplist: Option<&Path>) -> (Option<PathBuf>, Option<&'static str>) {
    if let Some(p) = cfg_str(cfg, &["pando_corpora", "pando_corpora_config"]) {
        return (Some(PathBuf::from(p)), Some("fqs.json"));
    }
    if let Some(p) = env_path("PANDO_CORPORA_CONFIG") {
        return (Some(p), Some("env"));
    }
    // same order as kontext-pando's lib/pando_corpora.py
    let mut cands = vec![
        PathBuf::from("/opt/kontext-pando/conf/pando_corpora.json"),
        PathBuf::from("/opt/kontext/conf/pando_corpora.json"),
    ];
    if let Some(dir) = corplist.and_then(|p| p.canonicalize().ok()).and_then(|p| p.parent().map(Path::to_path_buf)) {
        cands.push(dir.join("pando_corpora.json"));
    }
    for p in cands {
        if p.is_file() {
            return (Some(p), Some("discovered"));
        }
    }
    (None, None)
}


/// Manatee registry directory, to check that KonText can open a corpus.
fn resolve_manatee_registry(cfg: Option<&Value>) -> Option<PathBuf> {
    if let Some(p) = cfg_str(cfg, &["registry", "manatee_registry"]) {
        return Some(PathBuf::from(p));
    }
    if let Some(p) = env_path("MANATEE_REGISTRY") {
        return Some(p);
    }
    ["/var/lib/manatee/registry", "/usr/local/share/manatee/registry", "/opt/manatee/registry", "/corpora/registry"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_dir())
}


fn registry_has(dir: &Path, ident: &str) -> bool {
    dir.join(ident).is_file() || dir.join(ident.to_ascii_lowercase()).is_file()
}


/// A corpus name KonText (and Manatee registry file names) can live with.
pub fn valid_kontext_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}


/// The corpora this module offers to KonText: those with a Pando index (kontext-pando
/// sends their queries to FQS). Not CWB or Manatee corpora: KonText serves Manatee
/// corpora without FQS, and a corpus without an index is not ready for any frontend —
/// building indexes is not what publishing does.
fn kontext_listable(c: &CatalogCorpus<'_>) -> bool {
    !c.http_policy_mode.trim().eq_ignore_ascii_case("disabled")
        && pando_index_dir_for(c.settings, c.project_root).is_some()
}

/// A KonText corpus name made from an FQS corpus id.
fn kontext_name_for_id(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' { ch.to_ascii_lowercase() } else { '_' })
        .collect();
    if s.chars().next().is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        s
    } else {
        format!("c_{s}")
    }
}

fn suggested_kontext_ident(c: &CatalogCorpus<'_>) -> String {
    if let Some(n) = c
        .settings
        .get("kontext")
        .and_then(|k| k.get("corpname").or_else(|| k.get("corpus")))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return n.to_string();
    }
    kontext_name_for_id(c.id)
}


fn read_kontext_corplist_idents(path: &Path) -> Result<Vec<String>, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("Cannot read corplist {}: {}", path.display(), e))?;
    Ok(parse_kontext_corplist_idents(&text))
}


/// `ident`s of the <corpus> elements, leaving out commented-out ones.
fn parse_kontext_corplist_idents(text: &str) -> Vec<String> {
    let comments = xml_comment_ranges(text);
    let mut idents = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find("<corpus") {
        let start = search_from + rel;
        let after = &text[start..];
        let end_rel = after.find('>').unwrap_or(after.len());
        let next = after[7..].chars().next().unwrap_or('>');
        if !in_ranges(&comments, start) && (next.is_whitespace() || next == '/' || next == '>') {
            let tag = &after[..end_rel];
            if let Some(id) = attr_value_from_tag(tag, "ident").or_else(|| attr_value_from_tag(tag, "id")) {
                let id = id.trim();
                if !id.is_empty() && !idents.iter().any(|x: &String| x == id) {
                    idents.push(id.to_string());
                }
            }
        }
        search_from = start + end_rel.max(1);
    }
    idents
}


/// Options for a new `<corpus>` element in corplist.xml.
struct CorplistCorpusSpec<'a> {
    ident: &'a str,
    sentence_struct: &'a str,
    /// TEITOK link-back: keyword + token_connect provider (XML fragment + document view).
    teitok: bool,
    /// `keyboard_lang` when `teitok` (ISO-ish code; default `en`).
    keyboard_lang: Option<&'a str>,
}

/// The corplist with one more <corpus>, inserted before the last </corplist> that is
/// not inside a comment, indented like the corpora already there; checked to be
/// well-formed XML.
fn corplist_with_corpus(text: &str, spec: &CorplistCorpusSpec<'_>) -> Result<String, String> {
    roxmltree::Document::parse(text)
        .map_err(|e| format!("corplist.xml is not well-formed XML as it is ({e}); not changing it"))?;
    let comments = xml_comment_ranges(text);
    let mut idx = None;
    let mut from = 0;
    while let Some(rel) = text[from..].find("</corplist>") {
        let at = from + rel;
        if !in_ranges(&comments, at) {
            idx = Some(at);
        }
        from = at + 1;
    }
    let idx = idx.ok_or("corplist.xml has no </corplist> closing tag")?;
    // indentation: that of the closing tag's line plus 4, or of the last corpus
    let line_start = text[..idx].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let closing_indent: String = text[line_start..idx].chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let closing_on_own_line = text[line_start..idx].trim().is_empty();
    let indent = format!("{closing_indent}    ");
    let block = corplist_corpus_xml(spec, &indent);
    let mut out = String::with_capacity(text.len() + block.len() + 16);
    if closing_on_own_line {
        out.push_str(&text[..line_start]);
        out.push_str(&block);
        if !block.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&text[line_start..]);
    } else {
        out.push_str(&text[..idx]);
        out.push_str(block.trim_start());
        out.push_str(&text[idx..]);
    }
    roxmltree::Document::parse(&out).map_err(|e| format!("adding the corpus would break corplist.xml ({e})"))?;
    Ok(out)
}

/// XML for one `<corpus>`: self-closing for plain Pando, or TEITOK token_connect block.
fn corplist_corpus_xml(spec: &CorplistCorpusSpec<'_>, indent: &str) -> String {
    let ident = xml_escape_attr(spec.ident);
    let ss = xml_escape_attr(spec.sentence_struct);
    if !spec.teitok {
        return format!("{indent}<corpus ident=\"{ident}\" sentence_struct=\"{ss}\"/>\n");
    }
    let lang = spec
        .keyboard_lang
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| valid_kontext_ident(s) || s.len() <= 8)
        .unwrap_or("en");
    let lang = xml_escape_attr(lang);
    // features + num_tag_pos match existing LINDAT TEITOK corplist entries (taghelper / UI).
    let child = format!("{indent}    ");
    format!(
        "{indent}<corpus ident=\"{ident}\" sentence_struct=\"{ss}\" num_tag_pos=\"16\" keyboard_lang=\"{lang}\" features=\"morphology,syntax\">\n\
{child}<metadata>\n\
{child}        <keywords>\n\
{child}                <item>teitok</item>\n\
{child}        </keywords>\n\
{child}</metadata>\n\
{child}<token_connect>\n\
{child}        <provider is_kwic_view=\"false\">TEITOK</provider>\n\
{child}</token_connect>\n\
{indent}</corpus>\n"
    )
}


/// pando_corpora.json with an entry for `ident`, or None when unchanged.
/// When the corpus is already listed, still merges TEITOK link fields
/// (`teitok_crp_path` / `teitok_crp_server` / `label` / `url` / `backend` / `fqs_corpus`)
/// so republish can refresh them without deleting the entry.
fn pando_corpora_with(text: &str, ident: &str, entry: Value) -> Result<Option<String>, String> {
    let mut v: Value = if text.trim().is_empty() {
        json!({ "corpora": {} })
    } else {
        serde_json::from_str(text).map_err(|e| format!("pando_corpora.json is not valid JSON ({e}); not changing it"))?
    };
    // kontext-pando reads {"corpora": {...}} or the map itself
    let map = if v.get("corpora").is_some() {
        v.get_mut("corpora").and_then(Value::as_object_mut)
    } else {
        v.as_object_mut()
    }
    .ok_or("pando_corpora.json: expected an object of corpora")?;
    if let Some((key, existing)) = map
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case(ident))
    {
        let Some(dst) = existing.as_object_mut() else {
            return Ok(None);
        };
        let Some(src) = entry.as_object() else {
            return Ok(None);
        };
        let mut changed = false;
        for field in ["teitok_crp_path", "teitok_crp_server", "label", "url", "backend", "fqs_corpus"] {
            if let Some(val) = src.get(field) {
                if dst.get(field) != Some(val) {
                    dst.insert(field.to_string(), val.clone());
                    changed = true;
                }
            }
        }
        let _ = key;
        if !changed {
            return Ok(None);
        }
        return Ok(Some(serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"));
    }
    map.insert(ident.to_string(), entry);
    Ok(Some(serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"))
}


fn pando_corpora_idents(path: &Path) -> Option<std::collections::HashSet<String>> {
    let v: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let map = v.get("corpora").unwrap_or(&v).as_object()?;
    Some(map.keys().map(|k| k.to_ascii_lowercase()).collect())
}


fn manatee_name_ok(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}


fn registry_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace(['\n', '\r'], " "))
}


/// The shell's positional attributes, in vertical-column order: the word form first.
fn shell_attributes(info: &PandoCorpusInfo) -> Vec<String> {
    let mut attrs: Vec<String> = info.positional.iter().filter(|a| manatee_name_ok(a)).cloned().collect();
    let first = ["word", "form"].iter().find(|w| attrs.iter().any(|a| a == *w)).map(|w| w.to_string());
    if let Some(f) = first {
        attrs.retain(|a| *a != f);
        attrs.insert(0, f);
    }
    if let Some(i) = attrs.iter().position(|a| a == "lemma") {
        if i > 1 {
            let l = attrs.remove(i);
            attrs.insert(1, l);
        }
    }
    attrs
}


/// Structures for the shell, outermost first (zero-width ones, like deletions, left out).
fn shell_structures(info: &PandoCorpusInfo) -> Vec<String> {
    let order = ["doc", "text", "div", "chapter", "p", "u", "s"];
    let mut s: Vec<String> = info
        .structural
        .iter()
        .filter(|s| manatee_name_ok(s) && !info.zerowidth.contains(s))
        .cloned()
        .collect();
    s.sort_by_key(|x| order.iter().position(|o| o == x).unwrap_or(order.len() - 1));
    s
}


/// The marker line FQS puts in registries it writes (to update them, and only them).
fn registry_marker(corpus_id: &str, index_id: Option<&str>, encoded: bool) -> String {
    format!(
        "# fqs: corpus={corpus_id} index_id={}{}",
        index_id.unwrap_or("-"),
        if encoded { "" } else { " encoded=no" }
    )
}


pub struct ShellPaths {
    pub registry: PathBuf,
    pub data: PathBuf,
    pub vertical: PathBuf,
}


/// Registry file, data folder and vertical for `ident`: next to each other under the
/// registry folder's parent (/var/lib/manatee/{registry,data,vert}), unless fqs.json
/// says otherwise (frontends[].manatee_data, manatee_vert).
fn shell_paths(cfg: Option<&Value>, registry_dir: &Path, ident: &str) -> ShellPaths {
    let base = registry_dir.parent().map(Path::to_path_buf).unwrap_or_else(|| registry_dir.to_path_buf());
    let data_root = cfg_str(cfg, &["manatee_data"]).map(PathBuf::from).unwrap_or_else(|| base.join("data"));
    let vert_root = cfg_str(cfg, &["manatee_vert"]).map(PathBuf::from).unwrap_or_else(|| base.join("vert"));
    ShellPaths {
        registry: registry_dir.join(ident),
        data: data_root.join(ident),
        vertical: vert_root.join(format!("{ident}.vert")),
    }
}


pub struct ShellSpec<'a> {
    /// whether the one-token shell gets encoded (Manatee data), see create_manatee_shell
    pub encoded: bool,
    pub ident: &'a str,
    pub corpus_id: &'a str,
    pub label: &'a str,
    pub description: Option<&'a str>,
    pub language: Option<&'a str>,
    pub info: &'a PandoCorpusInfo,
    /// TEITOK base path for KonText token_connect (`crp.path`), e.g. `/teitok/migrantstories/`.
    pub crp_path: Option<&'a str>,
    /// Optional host for `crp.server` (informational; providers_conf often hardcodes server).
    pub crp_server: Option<&'a str>,
}

/// From a TEITOK `project_url`, the `(server, path/)` pair for Manatee `STRUCTURE crp`
/// and KonText's `{crp[path]}index.php?action=context…` provider.
pub fn teitok_crp_from_project_url(url: &str) -> Option<(String, String)> {
    let raw = url.trim();
    if raw.is_empty() {
        return None;
    }
    let no_hash = raw.split('#').next().unwrap_or(raw);
    let no_query = no_hash.split('?').next().unwrap_or(no_hash);
    let (server, path_part) = if let Some(rest) = no_query.strip_prefix("https://").or_else(|| no_query.strip_prefix("http://")) {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = host.split('@').next_back().unwrap_or(host); // drop userinfo if any
        let host = host.split(':').next().unwrap_or(host); // drop port for crp.server label
        (host.to_string(), format!("/{path}"))
    } else if no_query.starts_with('/') {
        (String::new(), no_query.to_string())
    } else {
        return None;
    };
    let mut path = path_part;
    if let Some(i) = path.to_ascii_lowercase().rfind("/index.php") {
        path.truncate(i + 1);
    } else if !path.ends_with('/') {
        path.push('/');
    }
    if path == "/" && server.is_empty() {
        return None;
    }
    Some((server, path))
}


pub fn manatee_registry_text(spec: &ShellSpec<'_>, paths: &ShellPaths) -> String {
    let info = spec.info;
    let attrs = shell_attributes(info);
    let structs = shell_structures(info);
    let mut o = String::new();
    o.push_str("# Manatee registry shell, written by FQS for kontext-pando: queries, frequencies and\n");
    o.push_str("# text types of this corpus come from Pando through FQS; Manatee only opens it.\n");
    o.push_str(&registry_marker(spec.corpus_id, info.index_id.as_deref(), spec.encoded));
    o.push('\n');
    o.push_str(&format!("NAME {}\n", registry_quote(spec.label)));
    let info_text = spec
        .description
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} (Pando index, served through FQS)", spec.label));
    o.push_str(&format!("INFO {}\n", registry_quote(&info_text)));
    o.push_str(&format!("PATH {}\n", registry_quote(&format!("{}/", paths.data.display()))));
    o.push_str(&format!("VERTICAL {}\n", registry_quote(&paths.vertical.display().to_string())));
    o.push_str("ENCODING \"UTF-8\"\n");
    if let Some(l) = spec.language.and_then(language_name) {
        o.push_str(&format!("LANGUAGE {}\n", registry_quote(l)));
    }
    if let Some(a) = attrs.first() {
        o.push_str(&format!("DEFAULTATTR {a}\n"));
    }
    let doc = ["doc", "text"].iter().find(|d| structs.iter().any(|s| s == *d));
    if let Some(d) = doc {
        if info.struct_attrs.iter().any(|(s, a)| s == *d && a == "id") {
            o.push_str(&format!("FULLREF \"{d}.id\"\n"));
            o.push_str(&format!("SHORTREF \"={d}.id\"\n"));
        }
        o.push_str(&format!("DOCSTRUCTURE {d}\n"));
    }
    o.push_str("MAXCONTEXT 100\nMAXDETAIL 100\n\n");
    for a in &attrs {
        if info.multivalue.contains(a) {
            o.push_str(&format!("ATTRIBUTE {a} {{\n    MULTIVALUE y\n    MULTISEP \"|\"\n}}\n"));
        } else {
            o.push_str(&format!("ATTRIBUTE {a}\n"));
        }
    }
    // TEITOK / KonText token_connect: STRUCTURE crp { path, server } — same as flexicorp Manatee writer.
    if spec.crp_path.is_some() && !structs.iter().any(|s| s == "crp") {
        o.push_str("STRUCTURE crp {\n    ATTRIBUTE path\n    ATTRIBUTE server\n}\n");
    }
    for s in &structs {
        let sattrs: Vec<&String> = info
            .struct_attrs
            .iter()
            .filter(|(st, a)| st == s && manatee_name_ok(a))
            .map(|(_, a)| a)
            .collect();
        if sattrs.is_empty() {
            o.push_str(&format!("STRUCTURE {s}\n"));
        } else {
            o.push_str(&format!("STRUCTURE {s} {{\n"));
            for a in sattrs {
                o.push_str(&format!("    ATTRIBUTE {a}\n"));
            }
            o.push_str("}\n");
        }
    }
    o
}


/// One document with one sentence of one token, with every column and structure.
pub fn manatee_shell_vertical(spec: &ShellSpec<'_>) -> String {
    let info = spec.info;
    let attrs = shell_attributes(info);
    let structs = shell_structures(info);
    let mut o = String::new();
    if let Some(path) = spec.crp_path.map(str::trim).filter(|s| !s.is_empty()) {
        let server = spec.crp_server.unwrap_or("");
        o.push_str(&format!(
            "<crp path=\"{}\" server=\"{}\">\n",
            xml_escape_attr(path),
            xml_escape_attr(server)
        ));
    }
    for s in &structs {
        let sattrs: Vec<String> = info
            .struct_attrs
            .iter()
            .filter(|(st, a)| st == s && manatee_name_ok(a))
            .map(|(_, a)| format!(" {a}=\"shell\""))
            .collect();
        o.push_str(&format!("<{s}{}>\n", sattrs.concat()));
    }
    o.push_str(&vec!["_"; attrs.len().max(1)].join("\t"));
    o.push('\n');
    for s in structs.iter().rev() {
        o.push_str(&format!("</{s}>\n"));
    }
    if spec.crp_path.map(str::trim).filter(|s| !s.is_empty()).is_some() {
        o.push_str("</crp>\n");
    }
    o
}


fn find_encodevert(cfg: Option<&Value>) -> Option<PathBuf> {
    if let Some(p) = cfg_str(cfg, &["encodevert"]) {
        return Some(PathBuf::from(p));
    }
    let mut dirs: Vec<PathBuf> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    for d in ["/usr/bin", "/usr/local/bin", "/opt/manatee/bin", "/opt/kontext/venv/bin"] {
        dirs.push(PathBuf::from(d));
    }
    dirs.into_iter().map(|d| d.join("encodevert")).find(|p| p.is_file())
}


/// State of the registry file for `ident`: "ok" (present, and up to date if FQS wrote
/// it), "unencoded" (FQS wrote it without Manatee data), "outdated" (written by FQS for
/// an older index), "missing".
pub fn registry_state(dir: &Path, ident: &str, index_id: Option<&str>) -> &'static str {
    let p = if dir.join(ident).is_file() { dir.join(ident) } else { dir.join(ident.to_ascii_lowercase()) };
    let Ok(text) = fs::read_to_string(&p) else {
        return "missing";
    };
    match text.lines().find(|l| l.starts_with("# fqs: corpus=")) {
        // not written by FQS: whoever made it maintains it
        None => "ok",
        Some(line) => {
            let have = line.split("index_id=").nth(1).and_then(|v| v.split_whitespace().next());
            match (have, index_id) {
                (Some(h), Some(want)) if h != want => "outdated",
                _ if line.contains("encoded=no") => "unencoded",
                _ => "ok",
            }
        }
    }
}


/// Write (or update, when FQS wrote it) the Manatee registry shell of a Pando corpus.
/// Never touches a registry FQS did not write.
///
/// The registry is what KonText needs: it opens every corpus with manatee.Corpus(registry)
/// and refuses one whose PATH folder does not exist. kontext-pando sends concordances,
/// frequencies, text types and the corpus info to Pando, so for those the Manatee data is
/// never read. Some KonText functions still read Manatee data directly (word list,
/// keywords, collocations, ...): with `encodevert` available FQS encodes a one-token
/// vertical so that they find a (tiny, empty-looking) corpus instead of missing files;
/// without it the registry and an empty data folder are written, and those functions fail
/// for this corpus (they do not work on Pando corpora either way).
pub fn create_manatee_shell(cfg: Option<&Value>, registry_dir: &Path, spec: &ShellSpec<'_>) -> Value {
    if !valid_kontext_ident(spec.ident) {
        return json!({ "status": "error", "message": "invalid corpus name" });
    }
    let paths = shell_paths(cfg, registry_dir, spec.ident);
    let state = registry_state(registry_dir, spec.ident, spec.info.index_id.as_deref());
    let encodevert = find_encodevert(cfg);
    if state == "ok" || (state == "unencoded" && encodevert.is_none()) {
        return json!({ "status": "ok", "path": paths.registry.display().to_string(), "encoded": state == "ok" });
    }
    // past this point the registry is missing, FQS wrote it for an older index, or it can
    // now be encoded
    let Some(encodevert) = encodevert else {
        return write_unencoded_shell(registry_dir, &paths, spec, state);
    };
    let spec = &ShellSpec { encoded: true, ..*spec };
    let text = manatee_registry_text(spec, &paths);
    let vert = manatee_shell_vertical(spec);
    let _guard = FILES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let step = || -> Result<Value, String> {
        for d in [Some(registry_dir), paths.data.parent(), paths.vertical.parent()].into_iter().flatten() {
            fs::create_dir_all(d).map_err(|e| format!("Cannot create {}: {e}", d.display()))?;
        }
        fs::write(&paths.vertical, &vert).map_err(|e| format!("Cannot write {}: {e}", paths.vertical.display()))?;
        // encode into a fresh folder next to the old one, then swap
        let tmp_data = paths.data.with_extension(format!("fqs-new-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_data);
        fs::create_dir_all(&tmp_data).map_err(|e| format!("Cannot create {}: {e}", tmp_data.display()))?;
        let tmp_reg = registry_dir.join(format!(".{}.fqs-tmp", spec.ident));
        let tmp_text = text.replace(
            &format!("PATH {}", registry_quote(&format!("{}/", paths.data.display()))),
            &format!("PATH {}", registry_quote(&format!("{}/", tmp_data.display()))),
        );
        fs::write(&tmp_reg, &tmp_text).map_err(|e| format!("Cannot write in {}: {e}", registry_dir.display()))?;
        let out = Command::new(&encodevert)
            .args(["-c"])
            .arg(&tmp_reg)
            .arg("-p")
            .arg(&tmp_data)
            .arg(&paths.vertical)
            .output()
            .map_err(|e| format!("Cannot run {}: {e}", encodevert.display()));
        let _ = fs::remove_file(&tmp_reg);
        let out = out?;
        if !out.status.success() {
            let _ = fs::remove_dir_all(&tmp_data);
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "encodevert failed: {}",
                err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim()
            ));
        }
        if paths.data.exists() {
            let old = paths.data.with_extension(format!("fqs-old-{}", std::process::id()));
            fs::rename(&paths.data, &old).map_err(|e| format!("Cannot replace {}: {e}", paths.data.display()))?;
            let _ = fs::remove_dir_all(&old);
        }
        fs::rename(&tmp_data, &paths.data).map_err(|e| format!("Cannot move the encoded data to {}: {e}", paths.data.display()))?;
        let reg_tmp = registry_dir.join(format!(".{}.fqs-new", spec.ident));
        fs::write(&reg_tmp, &text).map_err(|e| format!("Cannot write in {}: {e}", registry_dir.display()))?;
        fs::rename(&reg_tmp, &paths.registry).map_err(|e| format!("Cannot write {}: {e}", paths.registry.display()))?;
        Ok(json!({
            "status": if state == "missing" { "added" } else { "updated" },
            "path": paths.registry.display().to_string(),
            "data": paths.data.display().to_string(),
            "vertical": paths.vertical.display().to_string(),
            "encoded": true,
        }))
    };
    step().unwrap_or_else(|e| json!({ "status": "error", "path": paths.registry.display().to_string(), "message": e }))
}

/// The registry without Manatee data: an empty PATH folder (and the one-token vertical,
/// so that `encodevert` can still be run later; FQS does so itself once it finds it).
fn write_unencoded_shell(registry_dir: &Path, paths: &ShellPaths, spec: &ShellSpec<'_>, state: &str) -> Value {
    let spec = &ShellSpec { encoded: false, ..*spec };
    let text = manatee_registry_text(spec, paths);
    let vert = manatee_shell_vertical(spec);
    let _guard = FILES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let step = || -> Result<Value, String> {
        for d in [Some(registry_dir), Some(paths.data.as_path()), paths.vertical.parent()].into_iter().flatten() {
            fs::create_dir_all(d).map_err(|e| format!("Cannot create {}: {e}", d.display()))?;
        }
        fs::write(&paths.vertical, &vert).map_err(|e| format!("Cannot write {}: {e}", paths.vertical.display()))?;
        let reg_tmp = registry_dir.join(format!(".{}.fqs-new", spec.ident));
        fs::write(&reg_tmp, &text).map_err(|e| format!("Cannot write in {}: {e}", registry_dir.display()))?;
        fs::rename(&reg_tmp, &paths.registry).map_err(|e| format!("Cannot write {}: {e}", paths.registry.display()))?;
        Ok(json!({
            "status": if state == "missing" { "added" } else { "updated" },
            "path": paths.registry.display().to_string(),
            "data": paths.data.display().to_string(),
            "vertical": paths.vertical.display().to_string(),
            "encoded": false,
            "message": "Registry written without Manatee data (encodevert not found): searching works through Pando; KonText's word list, keywords and collocations do not work for this corpus.",
        }))
    };
    step().unwrap_or_else(|e| json!({ "status": "error", "path": paths.registry.display().to_string(), "message": e }))
}


/// Add a corpus to KonText: corplist.xml and (kontext-pando) pando_corpora.json, and
/// report what else KonText needs (Manatee registry, restart).
fn publish_kontext(cfg: Option<&Value>, req: &PublishRequest<'_>) -> Result<Value, String> {
    if !req.index_dir.as_deref().is_some_and(|d| d.join("corpus.info").is_file()) {
        return Err(format!(
            "'{}' has no Pando index: only Pando corpora are published to KonText from FQS (index the corpus with Pando first; Manatee corpora are configured in KonText itself)",
            req.corpus_id
        ));
    }
    let suggested = kontext_name_for_id(req.corpus_id);
    let ident = req.name.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&suggested);
    if !valid_kontext_ident(ident) {
        return Err(format!(
            "invalid KonText corpus name '{ident}': use letters, digits, '_', '-' and '.'"
        ));
    }
    let ss = req
        .options
        .get("sentence_struct")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("s");
    if !valid_kontext_ident(ss) {
        return Err("invalid sentence_struct".into());
    }
    let (path, source) = resolve_kontext_corplist(cfg);
    let path = path.ok_or_else(|| {
        "No KonText corplist.xml found. Set frontends[].corplist in fqs.json or FQS_KONTEXT_CORPLIST."
            .to_string()
    })?;
    let (pando_path, pando_source) = resolve_pando_corpora(cfg, Some(&path));

    let _guard = FILES_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // compute both changes before writing either
    let text = fs::read_to_string(&path).map_err(|e| format!("Cannot read {}: {}", path.display(), e))?;
    let present = parse_kontext_corplist_idents(&text)
        .iter()
        .any(|i| i.eq_ignore_ascii_case(ident));
    let new_corplist = if present {
        None
    } else {
        let keyboard_lang = req.language.and_then(|l| {
            let t = l.trim();
            if t.len() >= 2 && t.as_bytes()[..2].iter().all(u8::is_ascii_alphabetic) {
                Some(t[..2].to_ascii_lowercase())
            } else {
                None
            }
        });
        Some(corplist_with_corpus(
            &text,
            &CorplistCorpusSpec {
                ident,
                sentence_struct: ss,
                teitok: req.teitok,
                keyboard_lang: keyboard_lang.as_deref().or(Some("en")),
            },
        )?)
    };

    let mut pando_step = json!({ "status": "not_found",
        "message": "No pando_corpora.json found (set frontends[].pando_corpora in fqs.json): without it KonText cannot send queries for this corpus to Pando." });
    let mut new_pando = None;
    if let Some(pp) = &pando_path {
        {
            let cid = req.corpus_id;
            {
                let fqs_url = cfg_str(cfg, &["fqs_url"]).unwrap_or(req.fqs_url);
                let mut entry = json!({ "url": fqs_url.trim_end_matches('/'), "backend": "fqs", "fqs_corpus": cid });
                if !req.label.trim().is_empty() {
                    entry["label"] = json!(req.label.trim());
                }
                if req.teitok {
                    if let Some((server, path)) = req.project_url.and_then(teitok_crp_from_project_url) {
                        entry["teitok_crp_path"] = json!(path);
                        if !server.is_empty() {
                            entry["teitok_crp_server"] = json!(server);
                        }
                    }
                }
                // no "size": kontext-pando then asks FQS (/info), which stays right after a reindex
                let ptext = fs::read_to_string(pp).map_err(|e| format!("Cannot read {}: {e}", pp.display()))?;
                match pando_corpora_with(&ptext, ident, entry)? {
                    None => pando_step = json!({ "status": "present", "path": pp.display().to_string() }),
                    Some(t) => new_pando = Some(t),
                }
            }
        }
    }

    let mut corplist_step = json!({ "status": "present", "path": path.display().to_string(), "source": source });
    if let Some(t) = new_corplist {
        let w = rewrite_config_file(&path, &t)?;
        corplist_step = json!({ "status": "added", "path": w["path"], "backup": w["backup"],
            "method": w["method"], "owner_kept": w["owner_kept"], "source": source });
    }
    if let (Some(pp), Some(t)) = (&pando_path, new_pando) {
        pando_step = match rewrite_config_file(pp, &t) {
            Ok(w) => json!({ "status": "added", "path": w["path"], "backup": w["backup"],
                "method": w["method"], "source": pando_source }),
            Err(e) => json!({ "status": "error", "path": pp.display().to_string(), "message": e }),
        };
    }

    drop(_guard);
    // Manatee registry: build the shell from the Pando index when it is missing (or FQS
    // made it for an older index)
    let info = req.index_dir.as_deref().and_then(read_pando_corpus_info);
    let crp = req
        .project_url
        .and_then(teitok_crp_from_project_url)
        .or_else(|| {
            // options.teitok_crp_path override (admin / CLI)
            let path = req.options.get("teitok_crp_path").and_then(Value::as_str)?;
            let server = req
                .options
                .get("teitok_crp_server")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some((server, path.to_string()))
        });
    let (crp_server, crp_path) = match &crp {
        Some((s, p)) => (Some(s.as_str()), Some(p.as_str())),
        None => (None, None),
    };
    let registry_step = match resolve_manatee_registry(cfg) {
        None => json!({ "status": "unknown",
            "message": "Manatee registry folder not found (set frontends[].registry in fqs.json) — make sure it has a file for this corpus." }),
        Some(dir) => match &info {
            Some(info) => create_manatee_shell(
                cfg,
                &dir,
                &ShellSpec {
                    encoded: true,
                    ident,
                    corpus_id: req.corpus_id,
                    label: if req.label.trim().is_empty() { ident } else { req.label.trim() },
                    description: req.description,
                    language: req.language,
                    info,
                    crp_path: if req.teitok { crp_path } else { None },
                    crp_server: if req.teitok { crp_server } else { None },
                },
            ),
            _ if registry_has(&dir, ident) => json!({ "status": "ok", "path": dir.join(ident).display().to_string() }),
            _ => json!({ "status": "missing", "path": dir.join(ident).display().to_string(),
                "message": "KonText also needs a Manatee registry file for this corpus; FQS builds one only from the corpus's Pando index (corpus.info), which it could not find." }),
        },
    };

    let changed = corplist_step["status"] == "added"
        || pando_step["status"] == "added"
        || registry_step["status"] == "added"
        || registry_step["status"] == "updated";
    let complete = pando_step["status"] != "not_found"
        && pando_step["status"] != "error"
        && pando_step["status"] != "skipped"
        && registry_step["status"] != "missing"
        && registry_step["status"] != "error";
    let public_url = cfg_str(cfg, &["public_url"]);
    let mut kontext_settings = json!({ "corpname": ident });
    if let Some(u) = public_url {
        kontext_settings["public_url"] = json!(u);
    }
    Ok(json!({
        "ok": true,
        "name": ident,
        "steps": [
            step("corplist", "Corpus list", corplist_step.clone()),
            step("pando_corpora", "Pando entry", pando_step.clone()),
            step("registry", "Manatee registry", registry_step.clone()),
        ],
        "catalog_settings": { "kontext": kontext_settings },
        // KonText-specific keys, also for older admin UI builds
        "ident": ident,
        "already_present": present,
        "corplist_path": path.display().to_string(),
        "corplist_source": source,
        "corplist": corplist_step,
        "pando_corpora": pando_step,
        "registry": registry_step,
        "complete": complete,
        "restart_needed": changed,
        "restartable": cfg.and_then(|c| c.get("restart")).is_some()
            || crate::services::default_restart_block("kontext").is_some(),
        "public_url": public_url,
    }))
}


fn kontext_coverage_for_frontend(frontend_id: &str, cfg: &Value, corpora: &[CatalogCorpus<'_>]) -> Value {
    let frontend_url = cfg_str(Some(cfg), &["public_url", "url", "base"]);
    let (corplist_path, path_source) = resolve_kontext_corplist(Some(cfg));
    let (pando_path, pando_source) = resolve_pando_corpora(Some(cfg), corplist_path.as_deref());
    let registry = resolve_manatee_registry(Some(cfg));

    let (idents, setup_hint) = match &corplist_path {
        Some(path) => match read_kontext_corplist_idents(path) {
            Ok(ids) => (ids, None),
            Err(e) => (Vec::new(), Some(e)),
        },
        None => (
            Vec::new(),
            Some("No KonText corplist.xml found (set frontends[].corplist in fqs.json or FQS_KONTEXT_CORPLIST).".to_string()),
        ),
    };
    // the real files (corplist.xml may be a symlink)
    let real = |p: &Option<PathBuf>| p.as_ref().map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()));
    let mut checks = WriteChecks::default();
    let corplist_writable = real(&corplist_path).is_some_and(|p| checks.check(&p, false));
    let pando_writable = real(&pando_path).is_some_and(|p| checks.check(&p, false));
    let have_corplist = corplist_path.is_some() && setup_hint.is_none();
    let ident_set: std::collections::HashSet<String> = idents.iter().map(|s| s.to_ascii_lowercase()).collect();
    let pando_set = pando_path.as_deref().and_then(pando_corpora_idents);

    // A corpus is missing when it is not in the corplist, or when the files that are
    // known (pando_corpora.json, Manatee registry) lack it.
    let mut missing = Vec::new();
    if have_corplist {
        for c in corpora {
            if !c.is_current || !kontext_listable(c) {
                continue;
            }
            let ident = suggested_kontext_ident(c);
            let key = ident.to_ascii_lowercase();
            let in_corplist = ident_set.contains(&key);
            let in_pando = pando_set.as_ref().map(|s| s.contains(&key));
            // registry: present, and up to date with the Pando index when FQS made it
            let index_id = pando_index_dir_for(c.settings, c.project_root)
                .and_then(|d| read_pando_corpus_info(&d))
                .and_then(|i| i.index_id);
            let registry_st = registry.as_deref().map(|d| registry_state(d, &ident, index_id.as_deref()));
            let in_registry = registry_st.map(|st| st == "ok" || st == "unencoded");
            if in_corplist && in_pando != Some(false) && in_registry != Some(false) {
                continue;
            }
            missing.push(json!({
                "id": c.id,
                "label": c.label,
                "preferred_backend": c.preferred_backend,
                "project_url": c.project_url,
                "suggested_name": ident,
                "steps": { "corplist": in_corplist, "pando_corpora": in_pando, "registry": in_registry },
                "registry_outdated": registry_st == Some("outdated"),
                // KonText-specific keys, also for older admin UI builds
                "suggested_ident": ident,
                "in_corplist": in_corplist,
                "in_pando_corpora": in_pando,
                "in_registry": in_registry,
                "reason": if !in_corplist { "not_in_corplist" } else if in_pando == Some(false) { "not_in_pando_corpora" } else if registry_st == Some("outdated") { "registry_outdated" } else { "no_registry" },
                "frontend_url": frontend_url,
            }));
        }
    }

    let mut hints = Vec::new();
    if let Some(h) = &setup_hint {
        hints.push(h.clone());
    }
    if have_corplist && pando_path.is_none() {
        hints.push("No pando_corpora.json found: KonText can only serve Pando corpora with kontext-pando; set frontends[].pando_corpora in fqs.json if it is installed.".into());
    }

    if registry.is_none() && have_corplist {
        hints.push("Manatee registry folder not found (set frontends[].registry): FQS cannot check or build the registry files KonText needs.".into());
    }
    // registry files, their data and verticals: all three folders
    if let (Some(reg), true) = (&registry, have_corplist) {
        let paths = shell_paths(Some(cfg), reg, "x");
        let dirs = [Some(reg.clone()), paths.data.parent().map(Path::to_path_buf), paths.vertical.parent().map(Path::to_path_buf)];
        for d in dirs.into_iter().flatten() {
            if !d.is_dir() {
                hints.push(format!("{} does not exist: create it (writable for FQS) for the Manatee registry files.", d.display()));
            } else {
                checks.check(&d, true);
            }
        }
    }
    if have_corplist || pando_path.is_some() {
        hints.extend(checks.hints());
    }
    if registry.is_some() && have_corplist && find_encodevert(Some(cfg)).is_none() {
        hints.push("Manatee's encodevert not found (set frontends[].encodevert): registry files are written without Manatee data, which is enough for searching through Pando; KonText's word list, keywords and collocations then fail for those corpora.".into());
    }
    let mut files = vec![];
    if let Some(p) = &corplist_path {
        files.push(json!(["Corpus list", p.display().to_string()]));
    }
    if let Some(p) = &pando_path {
        files.push(json!(["Pando corpora", p.display().to_string()]));
    }
    if let Some(p) = &registry {
        files.push(json!(["Manatee registry", p.display().to_string()]));
    }

    let publishable = have_corplist && corplist_writable && (pando_path.is_none() || pando_writable);
    json!({
        "frontend_id": frontend_id,
        "kind": "kontext",
        "label": cfg_str(Some(cfg), &["label"]).unwrap_or("KonText"),
        "steps": [["corplist", "List"], ["pando_corpora", "Pando"], ["registry", "Registry"]],
        "files": files,
        "publishable": publishable,
        "url": frontend_url,
        "corplist_path": corplist_path.as_ref().map(|p| p.display().to_string()),
        "corplist_source": path_source,
        "pando_corpora_path": pando_path.as_ref().map(|p| p.display().to_string()),
        "pando_corpora_source": pando_source,
        "registry_path": registry.as_ref().map(|p| p.display().to_string()),
        "configured": true,
        // first problem, for older admin UI builds; all of them in `hints`
        "setup_hint": hints.first().cloned(),
        "hints": hints,
        "idents": idents,
        "missing": missing,
        "appendable": publishable,
        "restartable": cfg.get("restart").is_some()
            || crate::services::default_restart_block("kontext").is_some(),
    })
}


impl FrontendModule for Kontext {
    fn kind(&self) -> &'static str {
        "kontext"
    }
    fn label(&self) -> &'static str {
        "KonText"
    }
    /// A KonText install on this machine, or one the catalogue names.
    fn discover(&self, corpora: &[CatalogCorpus<'_>]) -> Option<Value> {
        let url = corpora.iter().find_map(|c| {
            c.settings
                .get("kontext")
                .and_then(|k| k.get("public_url").or_else(|| k.get("url")).or_else(|| k.get("base")))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        });
        let found = resolve_kontext_corplist(None).0.is_some();
        if url.is_none() && !found {
            return None;
        }
        let mut cfg = serde_json::Map::new();
        cfg.insert("kind".into(), json!("kontext"));
        if let Some(u) = url {
            // the catalogue's address is the public one: publishing records it with the
            // corpus (settings.kontext.public_url), which puts the corpus on the KonText card
            cfg.insert("url".into(), json!(u));
            cfg.insert("public_url".into(), json!(u));
        }
        Some(Value::Object(cfg))
    }
    fn processes(&self) -> Vec<Value> {
        discover_kontext_processes()
    }
    fn write_paths(&self, cfg: &Value) -> Vec<(PathBuf, bool)> {
        let mut out = Vec::new();
        let real = |p: PathBuf| p.canonicalize().unwrap_or(p);
        let (corplist, _) = resolve_kontext_corplist(Some(cfg));
        if let Some(c) = &corplist {
            out.push((real(c.clone()), false));
        }
        if let (Some(p), _) = resolve_pando_corpora(Some(cfg), corplist.as_deref()) {
            out.push((real(p), false));
        }
        if let Some(reg) = resolve_manatee_registry(Some(cfg)) {
            let paths = shell_paths(Some(cfg), &reg, "x");
            out.push((reg.clone(), true));
            for d in [paths.data.parent(), paths.vertical.parent()].into_iter().flatten() {
                out.push((d.to_path_buf(), true));
            }
        }
        out
    }
    fn coverage(&self, frontend_id: &str, cfg: &Value, corpora: &[CatalogCorpus<'_>]) -> Value {
        kontext_coverage_for_frontend(frontend_id, cfg, corpora)
    }
    fn publish(&self, _frontend_id: &str, cfg: Option<&Value>, req: &PublishRequest<'_>) -> Result<Value, String> {
        publish_kontext(cfg, req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn corplist_idents_parse() {
        let xml = r#"<?xml version="1.0"?>
<kontext><corplist>
  <corpus ident="ud_pando" sentence_struct="s"/>
  <corpus ident="SUSANNE"/>
</corplist></kontext>"#;
        let ids = parse_kontext_corplist_idents(xml);
        assert!(ids.iter().any(|i| i == "ud_pando"));
        assert!(ids.iter().any(|i| i == "SUSANNE"));
    }

    #[test]
    fn corplist_path_from_config_xml() {
        let dir = std::env::temp_dir().join(format!(
            "fqs-corplist-cfg-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let corplist = dir.join("corplist.xml");
        fs::write(
            &corplist,
            r#"<?xml version="1.0"?><corplist name="root">
  <corpus ident="ud_pando" sentence_struct="s"/>
</corplist>"#,
        )
        .unwrap();
        let config = dir.join("config.xml");
        fs::write(
            &config,
            format!(
                r#"<kontext><plugins><corparch>
            <file extension-by="lindat">{}</file>
            <root_elm_path extension-by="lindat">/corplist</root_elm_path>
        </corparch></plugins></kontext>"#,
                corplist.display()
            ),
        )
        .unwrap();
        let found = corplist_path_from_kontext_config(&config).unwrap();
        assert_eq!(found, corplist);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn coverage_lists_missing_teitok_when_corplist_configured() {
        let dir = std::env::temp_dir().join(format!(
            "fqs-coverage-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let corplist = dir.join("corplist.xml");
        fs::write(
            &corplist,
            r#"<?xml version="1.0"?><corplist name="root">
  <corpus ident="ud_pando" sentence_struct="s"/>
</corplist>"#,
        )
        .unwrap();

        // a TEITOK project with a Pando index, and one with only CWB (not offered)
        let pando_root = dir.join("migrantstories");
        fs::create_dir_all(pando_root.join("pando")).unwrap();
        fs::write(pando_root.join("pando/corpus.info"), "size=1\npositional=word\n").unwrap();
        let cwb_root = dir.join("cwbonly");
        fs::create_dir_all(cwb_root.join("cqp")).unwrap();
        let (pr, cr) = (pando_root.display().to_string(), cwb_root.display().to_string());
        let settings = json!({});
        let caps = json!({});
        let row = |id: &'static str, root: &'static str| CatalogCorpus {
            id,
            label: id,
            preferred_backend: "auto",
            is_current: true,
            http_policy_mode: "public_query",
            interface_preference: Some("teitok"),
            source_kind: "teitok",
            supports_xml: true,
            project_root: Some(root),
            project_url: Some("https://example/teitok/x/index.php"),
            settings: &settings,
            capabilities: &caps,
        };
        let pr: &'static str = Box::leak(pr.into_boxed_str());
        let cr: &'static str = Box::leak(cr.into_boxed_str());
        let rows = [row("migrantstories", pr), row("cwbonly", cr)];
        let cfg = json!({
            "kind": "kontext",
            "corplist": corplist.display().to_string(),
        });
        let report = KONTEXT.coverage("kontext", &cfg, &rows);
        let missing = report["missing"].as_array().unwrap();
        assert!(
            missing.iter().any(|m| m["id"] == "migrantstories"),
            "expected migrantstories in missing: {report}"
        );
        assert!(!missing.iter().any(|m| m["id"] == "cwbonly"), "a corpus without a Pando index is not offered: {report}");
        assert_eq!(report["appendable"], true);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kontext_ident_validation() {
        for ok in ["ud_pando", "migrantstories", "syn2020-4.0", "_x"] {
            assert!(valid_kontext_ident(ok), "{ok}");
        }
        for bad in ["", "x&y", "with space", "../x", "a/b", "a\"b", "-x", "a<b"] {
            assert!(!valid_kontext_ident(bad), "{bad}");
        }
    }

    #[test]
    fn corplist_insert_keeps_xml_well_formed() {
        let text = "<kontext>\n    <corplist name=\"root\">\n        <corpus ident=\"a\"/>\n    </corplist>\n    <!-- </corplist> -->\n</kontext>\n";
        let out = corplist_with_corpus(
            text,
            &CorplistCorpusSpec {
                ident: "b",
                sentence_struct: "s",
                teitok: false,
                keyboard_lang: None,
            },
        )
        .unwrap();
        assert!(out.contains("        <corpus ident=\"b\" sentence_struct=\"s\"/>\n    </corplist>"), "{out}");
        assert!(roxmltree::Document::parse(&out).is_ok());
        assert_eq!(parse_kontext_corplist_idents(&out), vec!["a", "b"]);
        // a broken corplist is left alone
        assert!(corplist_with_corpus(
            "<corplist><corpus ident=\"a\"></corplist>",
            &CorplistCorpusSpec {
                ident: "b",
                sentence_struct: "s",
                teitok: false,
                keyboard_lang: None,
            },
        )
        .is_err());
    }

    #[test]
    fn corplist_teitok_adds_token_connect() {
        let text = "<corplist name=\"root\">\n    <corpus ident=\"a\"/>\n</corplist>\n";
        let out = corplist_with_corpus(
            text,
            &CorplistCorpusSpec {
                ident: "tt_simplecorp_46",
                sentence_struct: "s",
                teitok: true,
                keyboard_lang: Some("en"),
            },
        )
        .unwrap();
        assert!(out.contains("ident=\"tt_simplecorp_46\""), "{out}");
        assert!(out.contains("num_tag_pos=\"16\""), "{out}");
        assert!(out.contains("keyboard_lang=\"en\""), "{out}");
        assert!(out.contains("features=\"morphology,syntax\""), "{out}");
        assert!(out.contains("<item>teitok</item>"), "{out}");
        assert!(
            out.contains("<provider is_kwic_view=\"false\">TEITOK</provider>"),
            "{out}"
        );
        assert!(roxmltree::Document::parse(&out).is_ok());
        assert_eq!(
            parse_kontext_corplist_idents(&out),
            vec!["a", "tt_simplecorp_46"]
        );
    }

    #[test]
    fn commented_corpora_do_not_count() {
        let text = "<corplist>\n<!-- <corpus ident=\"old\"/> -->\n<corpus ident=\"a\"/>\n<corpusgroup ident=\"g\"/>\n<corpus corpus_ident=\"x\" ident=\"c\"/>\n</corplist>";
        assert_eq!(parse_kontext_corplist_idents(text), vec!["a", "c"]);
    }

    #[test]
    fn rewrite_follows_symlinks_and_keeps_a_backup() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let d = std::env::temp_dir().join(format!("fqs-rewrite-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let real = d.join("real.xml");
        fs::write(&real, "old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
        let link = d.join("corplist.xml");
        symlink(&real, &link).unwrap();
        let w = rewrite_config_file(&link, "new").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o640);
        let backup = PathBuf::from(w["backup"].as_str().unwrap());
        assert_eq!(fs::read_to_string(backup).unwrap(), "old");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn pando_corpora_entry_added_once() {
        let t = pando_corpora_with("{\"corpora\": {\"UD_pando\": {\"url\": \"http://x\"}}}", "b",
            json!({"url": "http://127.0.0.1:8787", "backend": "fqs", "fqs_corpus": "b"})).unwrap().unwrap();
        let v: Value = serde_json::from_str(&t).unwrap();
        assert_eq!(v["corpora"]["b"]["backend"], "fqs");
        assert!(pando_corpora_with(&t, "ud_PANDO", json!({})).unwrap().is_none());
        assert!(pando_corpora_with("not json", "b", json!({})).is_err());
    }

    const UD_INFO: &str = "size=7493\npositional=deprel,feats,form,id,lemma,upos,word,xpos\nstructural=del,s,text\nregion_attrs=s_id,del_tok_id,del_id,text_id\ndefault_within=text\nzerowidth=del\nkv_pipe=feats\nhead_attrs=upos,deprel,lemma\nindex_id=20261004T114036536Z-1306bb72\n";

    fn ud_info() -> PandoCorpusInfo {
        // one folder per call: tests run in parallel
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("fqs-ci-{}-{n}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("corpus.info"), UD_INFO).unwrap();
        let info = read_pando_corpus_info(&d).unwrap();
        let _ = fs::remove_dir_all(&d);
        info
    }

    #[test]
    fn registry_shell_from_corpus_info() {
        let info = ud_info();
        assert_eq!(info.struct_attrs, vec![
            ("s".to_string(), "id".to_string()),
            ("del".to_string(), "tok_id".to_string()),
            ("del".to_string(), "id".to_string()),
            ("text".to_string(), "id".to_string()),
        ]);
        let paths = ShellPaths {
            registry: PathBuf::from("/var/lib/manatee/registry/ud-demo"),
            data: PathBuf::from("/var/lib/manatee/data/ud-demo"),
            vertical: PathBuf::from("/var/lib/manatee/vert/ud-demo.vert"),
        };
        let spec = ShellSpec { encoded: true, ident: "ud-demo", corpus_id: "ud-demo", label: "UD demo \"EWT\"",
            description: None, language: Some("en"), info: &info, crp_path: None, crp_server: None };
        let reg = manatee_registry_text(&spec, &paths);
        assert!(reg.contains("NAME \"UD demo \\\"EWT\\\"\"\n"), "{reg}");
        assert!(reg.contains("PATH \"/var/lib/manatee/data/ud-demo/\"\n"));
        assert!(reg.contains("LANGUAGE \"English\"\n"));
        assert!(reg.contains("DEFAULTATTR word\n"));
        assert!(reg.contains("FULLREF \"text.id\"\n"));
        assert!(reg.contains("ATTRIBUTE feats {\n    MULTIVALUE y\n    MULTISEP \"|\"\n}\n"));
        // the word form first, then lemma; deletions (zero-width) left out
        let attrs: Vec<&str> = reg.lines().filter_map(|l| l.strip_prefix("ATTRIBUTE ")).map(|a| a.trim_end_matches(" {")).collect();
        assert_eq!(&attrs[..2], &["word", "lemma"]);
        assert!(!reg.contains("STRUCTURE del"));
        assert!(reg.contains("STRUCTURE text {\n    ATTRIBUTE id\n}\n"));
        let vert = manatee_shell_vertical(&spec);
        assert_eq!(vert, "<text id=\"shell\">\n<s id=\"shell\">\n_\t_\t_\t_\t_\t_\t_\t_\n</s>\n</text>\n");

        let teitok_spec = ShellSpec {
            crp_path: Some("/teitok/migrantstories/"),
            crp_server: Some("lindat.mff.cuni.cz"),
            ..spec
        };
        let reg2 = manatee_registry_text(&teitok_spec, &paths);
        assert!(reg2.contains("STRUCTURE crp {\n    ATTRIBUTE path\n    ATTRIBUTE server\n}\n"), "{reg2}");
        let vert2 = manatee_shell_vertical(&teitok_spec);
        assert!(vert2.starts_with("<crp path=\"/teitok/migrantstories/\" server=\"lindat.mff.cuni.cz\">\n"), "{vert2}");
        assert!(vert2.ends_with("</crp>\n"), "{vert2}");
    }

    #[test]
    fn teitok_crp_path_from_project_url() {
        assert_eq!(
            teitok_crp_from_project_url("https://lindat.mff.cuni.cz/teitok/migrantstories/index.php"),
            Some(("lindat.mff.cuni.cz".into(), "/teitok/migrantstories/".into()))
        );
        assert_eq!(
            teitok_crp_from_project_url("http://example.org/services/teitok/foo"),
            Some(("example.org".into(), "/services/teitok/foo/".into()))
        );
    }

    #[test]
    fn registry_shell_is_built_and_kept_up_to_date() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("fqs-shell-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let reg_dir = d.join("registry");
        fs::create_dir_all(&reg_dir).unwrap();
        // a stand-in for Manatee's encodevert: writes a file into the -p folder
        let ev = d.join("encodevert");
        fs::write(&ev, "#!/bin/sh\nwhile [ $# -gt 1 ]; do [ \"$1\" = -p ] && out=$2; shift; done\necho encoded > \"$out/word.lex\"\n").unwrap();
        fs::set_permissions(&ev, fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = json!({ "encodevert": ev.display().to_string() });
        let mut info = ud_info();
        let spec = |info: &PandoCorpusInfo| create_manatee_shell(Some(&cfg), &reg_dir, &ShellSpec {
            encoded: true, ident: "ud-demo", corpus_id: "ud-demo", label: "UD demo", description: None, language: None, info,
            crp_path: None, crp_server: None });
        let r = spec(&info);
        assert_eq!(r["status"], "added", "{r}");
        assert!(d.join("data/ud-demo/word.lex").is_file());
        assert!(d.join("vert/ud-demo.vert").is_file());
        assert_eq!(registry_state(&reg_dir, "ud-demo", info.index_id.as_deref()), "ok");
        assert_eq!(spec(&info)["status"], "ok");
        // a new index: FQS's registry is outdated and gets rebuilt
        info.index_id = Some("newer".into());
        assert_eq!(registry_state(&reg_dir, "ud-demo", Some("newer")), "outdated");
        assert_eq!(spec(&info)["status"], "updated");
        assert_eq!(registry_state(&reg_dir, "ud-demo", Some("newer")), "ok");
        // a registry FQS did not write is never touched
        fs::write(reg_dir.join("other"), "NAME \"hand made\"\n").unwrap();
        assert_eq!(registry_state(&reg_dir, "other", Some("x")), "ok");
        let r = create_manatee_shell(Some(&cfg), &reg_dir, &ShellSpec {
            encoded: true, ident: "other", corpus_id: "other", label: "x", description: None, language: None, info: &info,
            crp_path: None, crp_server: None });
        assert_eq!(r["status"], "ok");
        assert_eq!(fs::read_to_string(reg_dir.join("other")).unwrap(), "NAME \"hand made\"\n");
        let _ = fs::remove_dir_all(&d);
    }


    #[test]
    fn registry_without_encodevert_has_an_empty_data_folder() {
        let d = std::env::temp_dir().join(format!("fqs-noenc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let reg_dir = d.join("registry");
        let info = ud_info();
        let spec = ShellSpec { encoded: true, ident: "ud-demo", corpus_id: "ud-demo", label: "UD demo",
            description: None, language: None, info: &info, crp_path: None, crp_server: None };
        let r = write_unencoded_shell(&reg_dir, &shell_paths(None, &reg_dir, "ud-demo"), &spec, "missing");
        assert_eq!(r["status"], "added", "{r}");
        assert_eq!(r["encoded"], false);
        assert!(d.join("data/ud-demo").is_dir());
        assert!(fs::read_dir(d.join("data/ud-demo")).unwrap().next().is_none());
        assert!(fs::read_to_string(reg_dir.join("ud-demo")).unwrap().contains(" encoded=no\n"));
        assert_eq!(registry_state(&reg_dir, "ud-demo", info.index_id.as_deref()), "unencoded");
        let _ = fs::remove_dir_all(&d);
    }

}
