//! One-shot catalogue enrichment from on-disk TEITOK / index layout.
//!
//! Fills browse labels (`lang:…`, `feature:…`), `interfaces`, and a
//! `capabilities.browse` snapshot. Merges additively — never removes hand-edited
//! labels. Run via `fqs corpora enrich` or after registration/validate `--enrich`.

use crate::CorpusEntry;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct EnrichReport {
    pub id: String,
    pub added_labels: Vec<String>,
    pub added_interfaces: Vec<String>,
    pub features: Vec<String>,
    pub languages: Vec<String>,
    pub notes: Vec<String>,
    pub changed: bool,
}

/// Merge detectable metadata into `entry`. Returns what was added.
pub fn enrich_corpus_entry(entry: &mut CorpusEntry) -> EnrichReport {
    let mut report = EnrichReport {
        id: entry.id.clone(),
        ..Default::default()
    };

    let root = resolve_project_dir(&entry.project_root);
    let detected = detect_from_disk(&root, entry);

    report.languages = detected.languages.clone();
    report.features = detected.features.clone();
    report.notes = detected.notes.clone();

    // Labels
    let mut labels = entry.labels.clone();
    for lab in &detected.labels {
        if !labels.iter().any(|x| x.eq_ignore_ascii_case(lab)) {
            labels.push(lab.clone());
            report.added_labels.push(lab.clone());
        }
    }
    labels.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));
    labels.dedup_by(|a, b| a.eq_ignore_ascii_case(b));

    // Interfaces
    let mut interfaces = entry.interfaces.clone();
    for iface in &detected.interfaces {
        if !interfaces.iter().any(|x| x.eq_ignore_ascii_case(iface)) {
            interfaces.push(iface.clone());
            report.added_interfaces.push(iface.clone());
        }
    }

    // settings.languages (only if missing/empty)
    let mut settings = entry.settings.clone();
    if let Some(obj) = settings.as_object_mut() {
        let has_langs = obj
            .get("languages")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if !has_langs && !detected.languages.is_empty() {
            obj.insert(
                "languages".into(),
                Value::Array(
                    detected
                        .languages
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
            report.changed = true;
            report.notes.push("settings.languages filled from detection".into());
        }
        if !obj.contains_key("teitok_project_root") && root != entry.project_root {
            obj.insert(
                "teitok_project_root".into(),
                json!(root.display().to_string()),
            );
            report.changed = true;
        }
    }

    // capabilities.browse snapshot + teitok_integration when TEITOK tree
    let mut capabilities = entry.capabilities.clone();
    if let Some(obj) = capabilities.as_object_mut() {
        let mut browse = obj
            .get("browse")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        browse.insert(
            "languages".into(),
            Value::Array(
                detected
                    .languages
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        browse.insert(
            "features".into(),
            Value::Array(
                detected
                    .features
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        browse.insert(
            "detected_at".into(),
            json!(time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "unknown".into())),
        );
        browse.insert("auto".into(), json!(true));
        obj.insert("browse".into(), Value::Object(browse));

        if detected.teitok_tree && obj.get("teitok_integration").and_then(Value::as_bool) != Some(true)
        {
            obj.insert("teitok_integration".into(), json!(true));
            report.notes.push("capabilities.teitok_integration=true".into());
            report.changed = true;
        }

        // FCS languages if FCS block exists and languages empty
        if let Some(fcs) = obj.get_mut("fcs").and_then(|v| v.as_object_mut()) {
            let empty = fcs
                .get("languages")
                .and_then(Value::as_array)
                .map(|a| a.is_empty())
                .unwrap_or(true);
            if empty && !detected.languages.is_empty() {
                fcs.insert(
                    "languages".into(),
                    Value::Array(
                        detected
                            .languages
                            .iter()
                            .cloned()
                            .map(Value::String)
                            .collect(),
                    ),
                );
                report.notes.push("capabilities.fcs.languages filled".into());
                report.changed = true;
            }
        }
    }

    if entry.interface_preference.is_none() && detected.teitok_tree {
        entry.interface_preference = Some("teitok".into());
        report.notes.push("interface_preference=teitok".into());
        report.changed = true;
    }

    if !report.added_labels.is_empty()
        || !report.added_interfaces.is_empty()
        || labels != entry.labels
        || interfaces != entry.interfaces
    {
        report.changed = true;
    }

    entry.labels = labels;
    entry.interfaces = interfaces;
    entry.settings = settings;
    entry.capabilities = capabilities;

    report
}

struct Detected {
    labels: Vec<String>,
    interfaces: Vec<String>,
    languages: Vec<String>,
    features: Vec<String>,
    notes: Vec<String>,
    teitok_tree: bool,
}

fn resolve_project_dir(project_root: &Path) -> PathBuf {
    if project_root.join("index.php").is_file() {
        return project_root.to_path_buf();
    }
    // …/pando or …/cqp → parent TEITOK root when it looks like one
    if let Some(parent) = project_root.parent() {
        if parent.join("index.php").is_file() {
            return parent.to_path_buf();
        }
    }
    project_root.to_path_buf()
}

fn detect_from_disk(root: &Path, entry: &CorpusEntry) -> Detected {
    let mut languages = Vec::new();
    let mut features = Vec::new();
    let mut notes = Vec::new();
    let mut labels = Vec::new();
    let mut interfaces = Vec::new();

    let has_index = root.join("index.php").is_file();
    let has_pando = root.join("pando").is_dir() || looks_like_pando_index(root);
    let has_cqp = root.join("cqp").is_dir();
    let has_manatee = root.join("manatee").is_dir();
    let teitok_tree = has_index
        && (has_pando
            || has_cqp
            || has_manatee
            || root.join("Scripts").exists()
            || root.join("Resources/settings.xml").is_file()
            || root.join("Pages").is_dir()
            || root.join("xmlfiles").is_dir());

    if teitok_tree {
        notes.push(format!("TEITOK project tree at {}", root.display()));
    }

    // Languages: settings.xml + existing catalogue
    push_langs(&mut languages, read_settings_xml_langs(root));
    if let Some(arr) = entry.settings.get("languages").and_then(Value::as_array) {
        for v in arr {
            if let Some(s) = v.as_str() {
                push_lang(&mut languages, s);
            }
        }
    }
    if let Some(arr) = entry
        .capabilities
        .get("fcs")
        .and_then(|f| f.get("languages"))
        .and_then(Value::as_array)
    {
        for v in arr {
            if let Some(s) = v.as_str() {
                push_lang(&mut languages, s);
            }
        }
    }

    // Feature folders / TEITOK layout
    if dir_nonempty(&root.join("Audio"))
        || dir_nonempty(&root.join("audio"))
        || dir_nonempty(&root.join("Media"))
        || dir_nonempty(&root.join("media"))
    {
        features.push("spoken".into());
        notes.push("spoken: Audio/Media folder".into());
    }
    if dir_nonempty(&root.join("Facsimile"))
        || dir_nonempty(&root.join("facsimile"))
        || settings_xml_mentions(root, &["facsimile", "pageimg", "page_image"])
        || settings_xml_mentions(root, &["folder=\"facsimile\"", "folder='facsimile'"])
    {
        features.push("facsimile".into());
        notes.push("facsimile: Facsimile folder or settings".into());
    }
    // Do not treat Pages/ as facsimile — that is TEITOK site PHP/HTML.
    if dir_nonempty(&root.join("Video"))
        || dir_nonempty(&root.join("video"))
        || settings_xml_mentions(root, &["folder=\"video\"", "folder='video'", ".mp4", ".webm"])
    {
        features.push("video".into());
        notes.push("video: Video folder or media extensions".into());
    }
    if settings_xml_mentions(root, &["<geomap", "geolocation", "latitude", "longitude"])
        || dir_nonempty(&root.join("Geo"))
        || root.join("Resources/geo.json").is_file()
    {
        features.push("geolocation".into());
        notes.push("geolocation: geomap / geo.json / coords".into());
    }
    if has_dependencies(root) {
        features.push("dependencies".into());
        notes.push("dependencies: deprel/head indexed or in settings".into());
    }
    if has_parallel(root) {
        features.push("parallel".into());
        notes.push("parallel: text_tuid / text_setid or s_tuid+tuid wiring".into());
    }
    if has_ner(root) {
        features.push("ner".into());
        notes.push("ner: <ner> / nerid in settings".into());
    }
    if has_ud_morph(root) {
        features.push("ud".into());
        notes.push("ud: upos/feats or udpipe in settings".into());
    }
    if root.join("xmlfiles").is_dir() || entry.supports_xml {
        // not a browse "feature" chip necessarily; drives interfaces
    }

    // CQP settings spoken cues
    if settings_xml_mentions(root, &["wavesurfer", "chunk_url", "u_media", "audio"])
        && !features.iter().any(|f| f == "spoken")
    {
        features.push("spoken".into());
        notes.push("spoken: settings media fields".into());
    }

    for lang in &languages {
        labels.push(format!("lang:{lang}"));
    }
    for feat in &features {
        labels.push(format!("feature:{feat}"));
    }

    // Interfaces FQS can expose
    if has_pando || has_cqp || entry.preferred_backend == "pando" || entry.preferred_backend == "cqp"
    {
        push_unique(&mut interfaces, "query");
        push_unique(&mut interfaces, "kwic");
    }
    if has_cqp || has_pando {
        push_unique(&mut interfaces, "freq");
    }
    if teitok_tree || entry.supports_xml {
        push_unique(&mut interfaces, "xml_context");
    }
    if features.iter().any(|f| f == "dependencies") {
        push_unique(&mut interfaces, "deps");
    }
    if features.iter().any(|f| f == "parallel") {
        push_unique(&mut interfaces, "aligned");
    }

    Detected {
        labels,
        interfaces,
        languages,
        features,
        notes,
        teitok_tree,
    }
}

fn looks_like_pando_index(dir: &Path) -> bool {
    dir.join("corpus.info").is_file()
}

fn dir_nonempty(p: &Path) -> bool {
    let Ok(rd) = fs::read_dir(p) else {
        return false;
    };
    rd.flatten().take(1).count() > 0
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x.eq_ignore_ascii_case(s)) {
        v.push(s.to_string());
    }
}

fn push_lang(langs: &mut Vec<String>, raw: &str) {
    let code = raw.trim().to_ascii_lowercase();
    let code = code
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim();
    if code.is_empty() || code == "und" {
        return;
    }
    let code = match code {
        "czech" | "cz" => "cs",
        "english" => "en",
        "german" => "de",
        "dutch" => "nl",
        "french" => "fr",
        "spanish" => "es",
        "slovak" => "sk",
        "polish" => "pl",
        "russian" => "ru",
        other => other,
    };
    if code.len() <= 8 && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        push_unique(langs, code);
    }
}

fn push_langs(langs: &mut Vec<String>, more: Vec<String>) {
    for m in more {
        push_lang(langs, &m);
    }
}

fn read_settings_xml_langs(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let path = root.join("Resources/settings.xml");
    let Ok(txt) = fs::read_to_string(&path) else {
        return out;
    };
    // Cheap attribute scrape (avoid pulling an XML crate for this).
    for key in ["lang=\"", "language=\"", "lang='", "language='"] {
        for (i, _) in txt.match_indices(key) {
            let rest = &txt[i + key.len()..];
            let end = rest
                .find(|c| c == '"' || c == '\'')
                .unwrap_or(0);
            if end > 0 {
                push_lang(&mut out, &rest[..end]);
            }
        }
    }
    out
}

fn settings_xml_mentions(root: &Path, needles: &[&str]) -> bool {
    let path = root.join("Resources/settings.xml");
    let Ok(txt) = fs::read_to_string(path) else {
        // also check cqpsettings
        let p2 = root.join("cqpsettings.xml");
        let Ok(txt2) = fs::read_to_string(p2) else {
            return false;
        };
        let lower = txt2.to_ascii_lowercase();
        return needles.iter().any(|n| lower.contains(n));
    };
    let lower = txt.to_ascii_lowercase();
    needles.iter().any(|n| lower.contains(&n.to_ascii_lowercase()))
}

fn catalog_text_blobs(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for cand in [
        root.join("Resources/settings.xml"),
        root.join("cqpsettings.xml"),
        root.join("tmp/cqpsettings.xml"),
        root.join("pando/corpus.info"),
        root.join("corpus.info"),
    ] {
        if let Ok(txt) = fs::read_to_string(cand) {
            out.push(txt.to_ascii_lowercase());
        }
    }
    out
}

fn blobs_contain_any(blobs: &[String], needles: &[&str]) -> bool {
    needles
        .iter()
        .any(|n| blobs.iter().any(|b| b.contains(&n.to_ascii_lowercase())))
}

fn has_dependencies(root: &Path) -> bool {
    let blobs = catalog_text_blobs(root);
    // Require deprel+head together — bare "dep" / "head" alone are too common.
    blobs.iter().any(|b| b.contains("deprel") && b.contains("head"))
}

/// TEITOK parallel/bitext: TUID pairing, not folder names.
/// `s_tuid` alone is used for dependency trees — not enough for parallel.
fn has_parallel(root: &Path) -> bool {
    let blobs = catalog_text_blobs(root);
    if blobs_contain_any(&blobs, &["text_tuid", "text_setid"]) {
        return true;
    }
    let has_s_tuid = blobs_contain_any(&blobs, &["s_tuid", "key=\"s_tuid\"", "key='s_tuid'"]);
    // Token-level alignment attrs (pattribute tuid / p_tuid), not sentence s_tuid alone.
    let has_tok_tuid = blobs.iter().any(|b| {
        b.contains("key=\"tuid\"")
            || b.contains("key='tuid'")
            || b.contains("key=\"p_tuid\"")
            || b.contains("key='p_tuid'")
            || b.contains("<pattribute>tuid</pattribute>")
            || (b.contains("\"tuid\"") && b.contains("pattribut"))
    });
    has_s_tuid && has_tok_tuid
}

fn has_ner(root: &Path) -> bool {
    let blobs = catalog_text_blobs(root);
    blobs_contain_any(&blobs, &["<ner", "nerid", "key=\"nerid\"", "key='nerid'"])
}

fn has_ud_morph(root: &Path) -> bool {
    let blobs = catalog_text_blobs(root);
    if blobs_contain_any(&blobs, &["udpipe", "<parser"]) && blobs_contain_any(&blobs, &["upos"]) {
        return true;
    }
    // UD morphology attrs commonly co-indexed.
    blobs
        .iter()
        .any(|b| b.contains("upos") && (b.contains("feats") || b.contains("xpos")))
}

/// JSON summary for CLI / API.
pub fn report_json(r: &EnrichReport) -> Value {
    json!({
        "id": r.id,
        "changed": r.changed,
        "added_labels": r.added_labels,
        "added_interfaces": r.added_interfaces,
        "languages": r.languages,
        "features": r.features,
        "notes": r.notes,
    })
}

#[allow(dead_code)]
pub fn merge_maps(base: &mut Map<String, Value>, patch: Map<String, Value>) {
    for (k, v) in patch {
        base.insert(k, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_root(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("fqs_enrich_{tag}_{n}"));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(p.join("Resources")).unwrap();
        p
    }

    #[test]
    fn parallel_needs_more_than_s_tuid() {
        let root = tmp_root("s_only");
        fs::write(
            root.join("Resources/settings.xml"),
            r#"<ttsettings><cqp><sattributes><item key="s_tuid" xpath="@tuid"/></sattributes></cqp></ttsettings>"#,
        )
        .unwrap();
        assert!(!has_parallel(&root));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parallel_from_text_tuid() {
        let root = tmp_root("text_tuid");
        fs::write(
            root.join("Resources/settings.xml"),
            r#"<ttsettings><cqp><sattributes><item key="text_tuid" xpath="@text_tuid"/></sattributes></cqp></ttsettings>"#,
        )
        .unwrap();
        assert!(has_parallel(&root));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parallel_from_s_tuid_and_tok_tuid() {
        let root = tmp_root("pair");
        fs::write(
            root.join("Resources/settings.xml"),
            r#"<ttsettings><cqp>
              <pattributes><item key="tuid" xpath="@tuid"/></pattributes>
              <sattributes><item key="s_tuid" xpath="@tuid"/></sattributes>
            </cqp></ttsettings>"#,
        )
        .unwrap();
        assert!(has_parallel(&root));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ner_and_ud_detection() {
        let root = tmp_root("ner_ud");
        fs::write(
            root.join("Resources/settings.xml"),
            r#"<ttsettings><ner/><cqp><pattributes>
              <item key="upos"/><item key="feats"/>
            </pattributes></cqp></ttsettings>"#,
        )
        .unwrap();
        assert!(has_ner(&root));
        assert!(has_ud_morph(&root));
        let _ = fs::remove_dir_all(&root);
    }
}
