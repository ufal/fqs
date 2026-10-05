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
    /// `fqs corpora enrich --reset-features`: feature labels that no longer apply
    pub removed_labels: Vec<String>,
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

    // settings.languages (only if missing/empty/placeholder-only)
    let mut settings = entry.settings.clone();
    if let Some(obj) = settings.as_object_mut() {
        let has_langs = obj
            .get("languages")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter().any(|v| {
                    v.as_str()
                        .map(|s| {
                            let s = s.trim().to_ascii_lowercase();
                            !s.is_empty()
                                && !matches!(s.as_str(), "und" | "unk" | "unknown" | "zxx")
                        })
                        .unwrap_or(false)
                })
            })
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
                .map(|a| {
                    a.is_empty()
                        || a.iter().all(|v| {
                            v.as_str()
                                .map(|s| {
                                    matches!(
                                        s.trim().to_ascii_lowercase().as_str(),
                                        "" | "und" | "unk" | "unknown" | "zxx"
                                    )
                                })
                                .unwrap_or(true)
                        })
                })
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

    // Media and page images: only real evidence counts — a non-empty folder, or media,
    // time stamps and facsimile references in the documents themselves. The words in
    // settings.xml do not: stock settings (teiHeader fields, menus, help texts) mention
    // "audio" and "facsimile" in projects that have neither.
    let xml_ev = sample_xml_evidence(root);
    let media_dir = dir_nonempty(&root.join("Media")) || dir_nonempty(&root.join("media"));
    if dir_nonempty(&root.join("Audio")) || dir_nonempty(&root.join("audio")) || xml_ev.audio
        || (media_dir && !xml_ev.video)
    {
        features.push("spoken".into());
        notes.push(if xml_ev.audio {
            "spoken: audio media in the documents".to_string()
        } else {
            "spoken: Audio/Media folder".to_string()
        });
    }
    if dir_nonempty(&root.join("Facsimile")) || dir_nonempty(&root.join("facsimile")) || xml_ev.facs {
        features.push("facsimile".into());
        notes.push(if xml_ev.facs {
            "facsimile: facs / surface references in the documents".to_string()
        } else {
            "facsimile: Facsimile folder".to_string()
        });
    }
    // Do not treat Pages/ as facsimile — that is TEITOK site PHP/HTML.
    if dir_nonempty(&root.join("Video")) || dir_nonempty(&root.join("video")) || xml_ev.video {
        features.push("video".into());
        notes.push("video: Video folder or video media in the documents".into());
    }
    if settings_xml_mentions(
        root,
        &[
            // not the bare words "geolocation" / "latitude": the stock teiHeader template
            // describes its place fields as "Geolocation coordinates (lat lng)"
            "<geomap",
            "key=\"latitude\"",
            "key='latitude'",
            "key=\"longitude\"",
            "key='longitude'",
            "key=\"lat\"",
            "key='lat'",
            "key=\"lon\"",
            "key='lon'",
            "key=\"long\"",
            "key='long'",
            "key=\"country\"",
            "key='country'",
            "key=\"country_or\"",
            "key='country_or'",
            "key=\"country_de\"",
            "key='country_de'",
            "xpath=\"@country",
            "xpath='@country",
        ],
    ) || dir_nonempty(&root.join("Geo"))
        || root.join("Resources/geo.json").is_file()
    {
        features.push("geolocation".into());
        notes.push("geolocation: geomap / geo.json / coords / country".into());
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

    // audio whose transcription has times (utterances or tokens with start / end, a timeline)
    if features.iter().any(|f| f == "spoken") && xml_ev.timed {
        features.push("timealigned".into());
        notes.push("timealigned: start / end times in the documents".into());
    }
    // documents described by dialect / variety
    if settings_xml_mentions(root, &["key=\"dialect\"", "key='dialect'", "display=\"dialect", "key=\"variety\"", "key='variety'"]) {
        features.push("dialect".into());
        notes.push("dialect: dialect / variety metadata".into());
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
    if code.is_empty() || matches!(code, "und" | "unk" | "unknown" | "zxx") {
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
    for path in [
        root.join("Resources/settings.xml"),
        root.join("cqpsettings.xml"),
        root.join("tmp/cqpsettings.xml"),
    ] {
        let Ok(txt) = fs::read_to_string(&path) else {
            continue;
        };
        // Prefer TEITOK <defaults lang="…"> / language="…" (content language).
        scrape_attr_langs(&txt, &["defaults"], &mut out);
        // Also general lang=/language=/xml:lang= (skip UI i18n noise later via push_lang).
        for key in [
            "xml:lang=\"",
            "xml:lang='",
            "lang=\"",
            "language=\"",
            "lang='",
            "language='",
        ] {
            for (i, _) in txt.match_indices(key) {
                let rest = &txt[i + key.len()..];
                let end = rest.find(|c| c == '"' || c == '\'').unwrap_or(0);
                if end > 0 {
                    push_lang(&mut out, &rest[..end]);
                }
            }
        }
    }
    // Sample TEI/XML docs for xml:lang when settings are silent.
    if out.is_empty() {
        push_langs(&mut out, sample_xmlfiles_langs(root));
    }
    out
}

/// Look for lang attrs inside (or near) named start tags, e.g. `<defaults … lang="en">`.
fn scrape_attr_langs(txt: &str, tags: &[&str], out: &mut Vec<String>) {
    let lower = txt.to_ascii_lowercase();
    for tag in tags {
        let needle = format!("<{tag}");
        let mut from = 0;
        while let Some(rel) = lower[from..].find(&needle) {
            let start = from + rel;
            let chunk_end = lower[start..]
                .find('>')
                .map(|i| start + i)
                .unwrap_or(lower.len().min(start + 400));
            let chunk = &txt[start..chunk_end];
            for key in ["lang=\"", "language=\"", "lang='", "language='"] {
                if let Some(i) = chunk.to_ascii_lowercase().find(key) {
                    // key is ascii; same index in chunk
                    let rest = &chunk[i + key.len()..];
                    let end = rest.find(|c| c == '"' || c == '\'').unwrap_or(0);
                    if end > 0 {
                        push_lang(out, &rest[..end]);
                    }
                }
            }
            from = chunk_end + 1;
            if from >= lower.len() {
                break;
            }
        }
    }
}

fn sample_xmlfiles_langs(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let dir = root.join("xmlfiles");
    let Ok(rd) = fs::read_dir(&dir) else {
        return out;
    };
    let mut n = 0;
    for ent in rd.flatten() {
        let p = ent.path();
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext != "xml" && ext != "tei" {
            continue;
        }
        let Ok(txt) = fs::read_to_string(&p) else {
            continue;
        };
        // First few docs only — enough for a dominant corpus language.
        for key in ["xml:lang=\"", "xml:lang='", " lang=\"", " lang='"] {
            for (i, _) in txt.match_indices(key) {
                let rest = &txt[i + key.len()..];
                let end = rest.find(|c| c == '"' || c == '\'').unwrap_or(0);
                if end > 0 && end <= 8 {
                    push_lang(&mut out, &rest[..end]);
                }
            }
        }
        n += 1;
        if n >= 8 || out.len() >= 3 {
            break;
        }
    }
    out
}

/// What a sample of the documents shows about media and page images.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct XmlEvidence {
    pub audio: bool,
    pub video: bool,
    pub timed: bool,
    pub facs: bool,
}

/// Look at up to 30 XML files under xmlfiles/ (the first 512 KB of each) for `<media>`
/// elements (audio or video by mime type or extension), start / begin times, and facsimile
/// references (`facs=`, `<facsimile>`, `<surface>`, `bbox=`).
fn sample_xml_evidence(root: &Path) -> XmlEvidence {
    use std::io::Read;
    let mut ev = XmlEvidence::default();
    let mut stack = vec![root.join("xmlfiles")];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            if ext != "xml" && ext != "tei" {
                continue;
            }
            let Ok(f) = fs::File::open(&p) else { continue };
            let mut buf = Vec::new();
            if f.take(512 * 1024).read_to_end(&mut buf).is_err() {
                continue;
            }
            let txt = String::from_utf8_lossy(&buf).to_ascii_lowercase();
            xml_evidence_in(&txt, &mut ev);
            seen += 1;
            if seen >= 30 || (ev.audio && ev.video && ev.timed && ev.facs) {
                return ev;
            }
        }
    }
    ev
}

/// The evidence in one document's (lower-cased) text.
pub(crate) fn xml_evidence_in(txt: &str, ev: &mut XmlEvidence) {
    for (i, _) in txt.match_indices("<media") {
        let tag = &txt[i..txt[i..].find('>').map(|e| i + e).unwrap_or(txt.len())];
        if ["audio", ".wav", ".mp3", ".ogg", ".m4a", ".flac"].iter().any(|n| tag.contains(n)) {
            ev.audio = true;
        }
        if ["video", ".mp4", ".webm", ".mov"].iter().any(|n| tag.contains(n)) {
            ev.video = true;
        }
    }
    for key in [" start=\"", " start='", " begin=\"", " begin='"] {
        for (i, _) in txt.match_indices(key) {
            if txt[i + key.len()..].chars().next().is_some_and(|c| c.is_ascii_digit()) {
                ev.timed = true;
                break;
            }
        }
    }
    if txt.contains("<timeline") {
        ev.timed = true;
    }
    for key in [" facs=\"", " facs='"] {
        for (i, _) in txt.match_indices(key) {
            let next = txt[i + key.len()..].chars().next();
            if next.is_some_and(|c| c != '"' && c != '\'') {
                ev.facs = true;
                break;
            }
        }
    }
    if txt.contains("<facsimile") || txt.contains("<surface") || txt.contains(" bbox=\"") {
        ev.facs = true;
    }
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
        "removed_labels": r.removed_labels,
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

    #[test]
    fn defaults_lang_and_country_geo() {
        let root = tmp_root("defaults");
        fs::write(
            root.join("Resources/settings.xml"),
            r#"<ttsettings>
              <defaults lang="en" shared="/teitok/shared"/>
              <cqp><sattributes>
                <item key="country_or" xpath="@country"/>
              </sattributes></cqp>
            </ttsettings>"#,
        )
        .unwrap();
        let langs = read_settings_xml_langs(&root);
        assert!(langs.iter().any(|l| l == "en"), "langs={langs:?}");
        assert!(settings_xml_mentions(
            &root,
            &["key=\"country_or\"", "xpath=\"@country"]
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn xml_evidence_needs_the_documents_not_settings_words() {
        let mut ev = XmlEvidence::default();
        // a plain written document: a header describing recordings in general, empty facs
        xml_evidence_in(
            &"<TEI><teiHeader><note>audio/video recording; facsimile</note></teiHeader><text><s id=\"s1\"><tok facs=\"\">x</tok></s></text></TEI>"
                .to_ascii_lowercase(),
            &mut ev,
        );
        assert_eq!(ev, XmlEvidence::default());
        xml_evidence_in(
            &"<recordingStmt><media mimeType=\"audio/wav\" url=\"a.wav\"/></recordingStmt><u start=\"1.25\" end=\"2.5\">".to_ascii_lowercase(),
            &mut ev,
        );
        assert!(ev.audio && ev.timed && !ev.video && !ev.facs);
        let mut ev2 = XmlEvidence::default();
        xml_evidence_in(&"<pb n=\"1\" facs=\"page1.jpg\"/>".to_ascii_lowercase(), &mut ev2);
        assert!(ev2.facs && !ev2.audio);
    }
}
