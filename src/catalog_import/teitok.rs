//! TEITOK rows: every project of a TEITOK folder (`<DIR>/<project>/Resources/settings.xml`: the
//! title, `<cqp corpus=…>`, `<flexicorp fqs_corpus_id=…>`; which indexes it has: pando/, a CWB
//! registry in cqp/), with what the shared project's corpus list (`shared/Resources/corplist.xml`)
//! says about it (name, fullname, enname, descr, lang, type, …).

use super::{corpus_id, labels, Options, PathMap, Row};
use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// What a project's settings.xml says. Hand-edited settings do not always parse: then the three
/// attributes are looked up in the text (`parse_error` says so).
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ProjectSettings {
    pub title: String,
    pub cqp_corpus: String,
    pub fqs_corpus_id: String,
    pub parse_error: Option<String>,
}

pub(crate) fn read_settings(text: &str) -> ProjectSettings {
    match roxmltree::Document::parse(text) {
        Ok(doc) => {
            let root = doc.root_element();
            fn child<'a, 'i>(n: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
                n.children().find(|c| c.has_tag_name(name))
            }
            let mut s = ProjectSettings::default();
            if let Some(t) = child(root, "defaults").and_then(|d| child(d, "title")) {
                s.title = t.attribute("display").map(str::to_string).unwrap_or_else(|| t.text().unwrap_or("").to_string()).trim().to_string();
            }
            if let Some(c) = child(root, "cqp") {
                s.cqp_corpus = c.attribute("corpus").unwrap_or("").to_string();
            }
            if let Some(f) = child(root, "flexicorp") {
                s.fqs_corpus_id = f.attribute("fqs_corpus_id").unwrap_or("").to_string();
            }
            s
        }
        Err(e) => ProjectSettings {
            title: attribute_in_text(text, "title", "display"),
            cqp_corpus: attribute_in_text(text, "cqp", "corpus"),
            fqs_corpus_id: attribute_in_text(text, "flexicorp", "fqs_corpus_id"),
            parse_error: Some(e.to_string()),
        },
    }
}

/// The value of `attr` on the first `<tag …>` start tag in `text` that has it ("" when none): for
/// settings that do not parse as XML.
fn attribute_in_text(text: &str, tag: &str, attr: &str) -> String {
    let open = format!("<{tag}");
    let mut rest = text;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        rest = after;
        if !after.starts_with(|c: char| c.is_whitespace()) {
            continue;   // <cqpsettings…, not <cqp
        }
        let tag_text = &after[..after.find('>').unwrap_or(after.len())];
        let mut t = tag_text;
        while let Some(j) = t.find(attr) {
            let before_ok = j == 0 || t[..j].ends_with(|c: char| c.is_whitespace());
            let after_name = t[j + attr.len()..].trim_start();
            if before_ok {
                if let Some(v) = after_name.strip_prefix('=') {
                    let v = v.trim_start();
                    if let Some(q) = v.chars().next().filter(|c| *c == '"' || *c == '\'') {
                        if let Some(end) = v[1..].find(q) {
                            return v[1..1 + end].to_string();
                        }
                    }
                }
            }
            t = &t[j + attr.len()..];
        }
    }
    String::new()
}

/// {project folder: {field: text}} from the shared corpus list.
pub(crate) fn read_corplist(text: &str) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
    let doc = roxmltree::Document::parse(text)?;
    let mut out = BTreeMap::new();
    for c in doc.descendants().filter(|n| n.has_tag_name("corpus")) {
        let Some(id) = c.attribute("id") else { continue };
        let fields = c
            .children()
            .filter(|ch| ch.is_element())
            .filter_map(|ch| {
                let t = ch.text().unwrap_or("").trim();
                (!t.is_empty()).then(|| (ch.tag_name().name().to_string(), t.to_string()))
            })
            .collect();
        out.insert(id.to_string(), fields);
    }
    Ok(out)
}

/// "Czech, Slovak and German" → [Czech, Slovak, German]
pub(crate) fn split_langs(s: &str) -> Vec<String> {
    s.replace(" and ", ",")
        .split([',', ';', '/'])
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn rows(o: &Options, dir: &Path, pmap: &PathMap, issues: &mut Vec<String>) -> Result<Vec<Row>> {
    let root = std::fs::canonicalize(dir).map_err(|e| anyhow::anyhow!("--teitok {}: {e}", dir.display()))?;
    let clist_path = o.teitok_corplist.clone().unwrap_or_else(|| root.join("shared/Resources/corplist.xml"));
    let clist = if clist_path.is_file() {
        match std::fs::read_to_string(&clist_path).map_err(anyhow::Error::from).and_then(|t| read_corplist(&t)) {
            Ok(c) => c,
            Err(e) => {
                issues.push(format!("TEITOK corpus list {} not read: {e}", clist_path.display()));
                BTreeMap::new()
            }
        }
    } else if o.teitok_corplist.is_some() {
        bail!("no TEITOK corpus list at {}", clist_path.display());
    } else {
        BTreeMap::new()
    };

    let mut names: Vec<String> = std::fs::read_dir(&root)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for name in names {
        let proj = root.join(&name);
        let settings_file = proj.join("Resources/settings.xml");
        if ["shared", "common", "Scripts"].contains(&name.as_str()) || !settings_file.is_file() {
            continue;
        }
        seen.insert(name.clone());
        let st = read_settings(&std::fs::read_to_string(&settings_file).unwrap_or_default());
        let cl = clist.get(&name).cloned().unwrap_or_default();
        let id = if st.fqs_corpus_id.is_empty() { corpus_id(&name) } else { st.fqs_corpus_id.clone() };
        let mut notes = Vec::new();
        if let Some(e) = &st.parse_error {
            notes.push(format!("settings.xml does not parse ({e}); read from its text"));
        }
        let pando = proj.join("pando/corpus.info").is_file();
        let cqp_name = st.cqp_corpus.clone();
        let cqp = !cqp_name.is_empty() && !cqp_name.ends_with('-') && proj.join("cqp").join(cqp_name.to_lowercase()).is_file();
        let mut s = Map::new();
        if pando {
            s.insert("index_dir".into(), json!(pmap.map(&proj.join("pando"))));
        }
        if cqp {
            s.insert("cqp_corpus".into(), json!(cqp_name));
            s.insert("registry_hint".into(), json!(pmap.map(&proj.join("cqp"))));
        }
        if !pando && !cqp {
            notes.push("no index (no pando/corpus.info, no CWB registry for the cqp corpus): build one first".into());
        }
        if let Some(d) = cl.get("descr") {
            s.insert("description".into(), json!(d));
        }
        let langs = split_langs(cl.get("lang").map(String::as_str).unwrap_or(""));
        if !langs.is_empty() {
            s.insert("languages".into(), json!(langs));
        }
        if !cl.is_empty() {
            s.insert("teitok_corplist".into(), json!(cl));
        } else if !clist.is_empty() {
            notes.push("not in the TEITOK corpus list".into());
        }
        let label = ["enname", "fullname"]
            .iter()
            .find_map(|k| cl.get(*k).cloned())
            .or_else(|| (!st.title.is_empty()).then(|| st.title.clone()))
            .or_else(|| cl.get("name").cloned())
            .unwrap_or_else(|| name.clone());
        let mut ls = vec!["TEITOK".to_string()];
        ls.extend(cl.get("type").cloned());
        ls.extend(langs.iter().map(|l| format!("lang:{l}")));
        let backend = if pando { "pando" } else if cqp { "cqp" } else { "auto" };
        let mut v = Map::new();
        v.insert("id".into(), json!(id));
        v.insert("label".into(), json!(label));
        v.insert("project_root".into(), json!(pmap.map(&proj)));
        v.insert("project_url".into(), o.teitok_url.as_ref().map(|u| json!(format!("{}/{name}/index.php", u.trim_end_matches('/')))).unwrap_or(Value::Null));
        v.insert("preferred_backend".into(), json!(backend));
        v.insert("source_kind".into(), json!(match backend { "pando" => "teitok_pando", "cqp" => "teitok_cqp", _ => "teitok" }));
        v.insert("interface_preference".into(), json!("teitok"));
        v.insert("supports_xml".into(), json!(true));
        v.insert("capabilities".into(), json!({"teitok_integration": true}));
        v.insert("environment".into(), json!(o.environment));
        v.insert("labels".into(), labels(ls));
        v.insert("settings".into(), Value::Object(s));
        v.insert("is_current".into(), json!(true));
        out.push(Row { value: v, source: "teitok", access: String::new(), notes, match_key: cqp_name.to_lowercase() });
    }
    for name in clist.keys().filter(|n| !seen.contains(*n)) {
        issues.push(format!("the TEITOK corpus list names '{name}', but there is no project folder for it"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parsed_and_from_text() {
        let ok = r#"<ttsettings><defaults><title display="My corpus"/></defaults><cqp corpus="TT-MINE"/><flexicorp fqs_corpus_id="mine"/></ttsettings>"#;
        let s = read_settings(ok);
        assert_eq!((s.title.as_str(), s.cqp_corpus.as_str(), s.fqs_corpus_id.as_str()), ("My corpus", "TT-MINE", "mine"));
        assert!(s.parse_error.is_none());
        // an undeclared entity: not XML, read from the text
        let bad = r#"<ttsettings><cqpsettings x="1"/><defaults><title display="A &nbsp; B"/></defaults><cqp  corpus='TT-B'/></ttsettings>"#;
        let s = read_settings(bad);
        assert!(s.parse_error.is_some());
        assert_eq!(s.title, "A &nbsp; B");
        assert_eq!(s.cqp_corpus, "TT-B", "not taken from <cqpsettings>");
        assert_eq!(s.fqs_corpus_id, "");
    }

    #[test]
    fn languages_and_corplist() {
        assert_eq!(split_langs("Czech, Slovak and German/Polish"), vec!["Czech", "Slovak", "German", "Polish"]);
        let c = read_corplist(r#"<corplist><corpus id="mine"><name>Mine</name><lang>cs</lang><descr> </descr></corpus></corplist>"#).unwrap();
        assert_eq!(c["mine"].get("name").map(String::as_str), Some("Mine"));
        assert!(!c["mine"].contains_key("descr"), "empty fields left out");
    }
}
