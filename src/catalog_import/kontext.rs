//! KonText rows: the corpora of KonText's corplist.xml (default_corparch / lindat_corparch2: nested
//! `<corplist title=…>`, `<corpus ident=… access=… repo=…>`, `<metadata><keywords><item>…`), with
//! their Manatee registry files (NAME, PATH, INFO, LANGUAGE, and the attributes with their labels,
//! registry_info.rs). Without --kontext-corplist FILE / --registry DIR, FQS's KonText module finds
//! them (fqs.json, FQS_KONTEXT_CORPLIST / MANATEE_REGISTRY, KonText's config.xml, the usual places).

use super::{corpus_id, labels, Options, PathMap, Row};
use crate::registry_info;
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One `<corpus>` of the corplist.
#[derive(Debug, Default)]
pub(crate) struct CorplistCorpus {
    pub ident: String,
    pub attrs: HashMap<String, String>,
    /// the titles of the `<corplist>` folders it is in
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    pub desc: String,
    /// `access="…"`: the groups that may use it; None when the corplist does not say
    pub access: Option<Vec<String>>,
}

/// The text of `<title lang=…>` children (English first), else of the element itself.
fn text_of(n: roxmltree::Node<'_, '_>) -> String {
    let titles: Vec<_> = n.children().filter(|c| c.has_tag_name("title")).collect();
    if !titles.is_empty() {
        let pick = titles.iter().find(|t| t.attribute("lang") == Some("en")).unwrap_or(&titles[0]);
        return pick.text().unwrap_or("").trim().to_string();
    }
    n.text().unwrap_or("").trim().to_string()
}

pub(crate) fn read_corplist(text: &str) -> Result<Vec<CorplistCorpus>> {
    let doc = roxmltree::Document::parse(text)?;
    // keyword idents → their (English) label
    let mut kw_labels = HashMap::new();
    for kw in doc.descendants().filter(|n| n.has_tag_name("keyword")) {
        let Some(id) = kw.attribute("ident") else { continue };
        let labels: Vec<_> = kw.children().filter(|c| c.has_tag_name("label")).collect();
        let en = labels.iter().find(|l| l.attribute("lang") == Some("en")).or(labels.first());
        let l = en.and_then(|l| l.text()).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(id);
        kw_labels.insert(id.to_string(), l.to_string());
    }
    let mut out = Vec::new();
    fn walk(n: roxmltree::Node<'_, '_>, path: &[String], kw: &HashMap<String, String>, out: &mut Vec<CorplistCorpus>) {
        if n.has_tag_name("corplist") {
            let title = n.attribute("title").map(str::to_string).unwrap_or_else(|| {
                if n.children().any(|c| c.has_tag_name("title")) { text_of(n) } else { String::new() }
            });
            let mut here = path.to_vec();
            if !title.is_empty() {
                here.push(title);
            }
            for c in n.children().filter(|c| c.is_element()) {
                walk(c, &here, kw, out);
            }
        } else if n.has_tag_name("corpus") && n.attribute("ident").is_some() {
            let meta = n.children().find(|c| c.has_tag_name("metadata"));
            let mut c = CorplistCorpus {
                ident: n.attribute("ident").unwrap_or("").to_string(),
                attrs: n.attributes().map(|a| (a.name().to_string(), a.value().to_string())).collect(),
                categories: path.to_vec(),
                access: n.attribute("access").map(|a| a.split(',').map(str::trim).filter(|g| !g.is_empty()).map(str::to_string).collect()),
                ..Default::default()
            };
            if let Some(m) = meta {
                for k in m.children().filter(|c| c.has_tag_name("keywords")) {
                    for item in k.children().filter(|c| c.has_tag_name("item")) {
                        let t = item.text().unwrap_or("").trim();
                        if !t.is_empty() {
                            c.keywords.push(kw.get(t).cloned().unwrap_or_else(|| t.to_string()));
                        }
                    }
                }
                let descs: Vec<_> = m.children().filter(|c| c.has_tag_name("desc")).collect();
                if let Some(d) = descs.iter().find(|d| d.attribute("lang") == Some("en")).or(descs.first()) {
                    c.desc = d.text().unwrap_or("").trim().to_string();
                }
            }
            out.push(c);
        } else {
            for c in n.children().filter(|c| c.is_element()) {
                walk(c, path, kw, out);
            }
        }
    }
    walk(doc.root_element(), &[], &kw_labels, &mut out);
    Ok(out)
}

/// The corplist and registry folder: as given (`auto` for the corplist: find it), else found the
/// way FQS's KonText module finds them. What was found goes into `found` (the summary).
pub(crate) fn resolve_sources(corplist: &Path, registry: Option<&Path>, found: &mut Vec<String>) -> Result<(PathBuf, Option<PathBuf>)> {
    let corplist = if corplist.as_os_str() == "auto" {
        match crate::frontends::kontext::resolve_kontext_corplist(None) {
            (Some(p), src) => {
                found.push(format!("KonText corplist: {} (found: {})", p.display(), src.unwrap_or("?")));
                p
            }
            _ => bail!("--kontext-corplist auto: no KonText corplist.xml found (give its path)"),
        }
    } else {
        corplist.to_path_buf()
    };
    let registry = match registry {
        Some(r) => Some(std::fs::canonicalize(r).with_context(|| format!("--registry {}", r.display()))?),
        None => {
            let r = crate::frontends::kontext::resolve_manatee_registry(None);
            if let Some(p) = &r {
                found.push(format!("Manatee registry: {} (found; --registry DIR names another)", p.display()));
            }
            r
        }
    };
    Ok((corplist, registry))
}

fn find_registry(dir: &Path, ident: &str) -> Option<PathBuf> {
    [ident.to_string(), ident.to_lowercase()].into_iter().map(|n| dir.join(n)).find(|p| p.is_file())
}

pub(crate) fn rows(
    o: &Options,
    corplist: &Path,
    registry: Option<&Path>,
    pmap: &PathMap,
    issues: &mut Vec<String>,
    taken: &mut HashSet<String>,
) -> Result<Vec<Row>> {
    let text = std::fs::read_to_string(corplist).with_context(|| format!("reading {}", corplist.display()))?;
    let corpora = read_corplist(&text).with_context(|| format!("{} is not a KonText corplist", corplist.display()))?;
    let mut out = Vec::new();
    for c in &corpora {
        let mut notes = Vec::new();
        let reg_path = registry.and_then(|r| find_registry(r, &c.ident));
        let reg_text = match &reg_path {
            Some(p) => match std::fs::read_to_string(p) {
                Ok(t) => t,
                Err(e) => {
                    notes.push(format!("registry not readable: {e}"));
                    String::new()
                }
            },
            None => {
                if registry.is_some() {
                    notes.push("no registry file".into());
                }
                String::new()
            }
        };
        let top = registry_info::top_values(&reg_text);
        let get = |k: &str| top.get(k).cloned().unwrap_or_default();
        let mut id = corpus_id(&c.ident);
        if taken.contains(&id) {
            notes.push(format!("id '{id}' is a TEITOK project too; this row is {id}-manatee"));
            id.push_str("-manatee");
        }
        taken.insert(id.clone());
        let data_path = get("PATH");
        if o.manatee_native && data_path.is_empty() {
            notes.push("no PATH in the registry: FQS cannot open it natively".into());
        } else if !data_path.is_empty() && !Path::new(&data_path).is_dir() {
            notes.push(format!("PATH {data_path} does not exist here"));
        }
        let corpname = c.ident.to_lowercase();
        let mut s = Map::new();
        if o.manatee_native {
            if let Some(p) = &reg_path {
                s.insert("manatee_registry".into(), json!(pmap.map(p)));
            }
        } else if let Some(u) = &o.kontext_url {
            s.insert("kontext".into(), json!({"url": u.trim_end_matches('/'), "corpname": corpname}));
        } else {
            notes.push("--manatee kontext without --kontext-url: FQS has no way to reach it".into());
        }
        s.insert("kontext_corpname".into(), json!(corpname));
        let desc = if c.desc.is_empty() { get("INFO") } else { c.desc.clone() };
        if !desc.is_empty() {
            s.insert("description".into(), json!(desc));
        }
        let lang = get("LANGUAGE");
        if !lang.is_empty() {
            s.insert("languages".into(), json!([lang]));
        }
        if let Some(a) = &c.access {
            s.insert("access".into(), json!(a));
        }
        let mut cl = Map::new();
        for k in ["repo", "pid", "sentence_struct", "parallel", "pmltq", "tagset", "web"] {
            if let Some(v) = c.attrs.get(k).filter(|v| !v.is_empty()) {
                cl.insert(k.into(), json!(v));
            }
        }
        if !cl.is_empty() {
            s.insert("kontext_corplist".into(), Value::Object(cl));
        }
        // the attributes with their labels, in /info's shape: a front-end's fallback (the portal)
        if !reg_text.is_empty() {
            let info = registry_info::info_from_registry_text(&reg_text, None, "manatee");
            if info["attributes"].as_array().is_some_and(|a| !a.is_empty()) {
                s.insert(
                    "corpus_info".into(),
                    json!({"attributes": info["attributes"], "structures": info["structures"], "region_attrs": info["region_attrs"]}),
                );
            }
        }
        if let Some(p) = &o.portal {
            s.insert("portals".into(), json!([p]));
        }
        if let Some(u) = &o.portal_url {
            s.insert("portal_url".into(), json!(format!("{}/index.php?action=flexicorp&corpus={id}", u.trim_end_matches('/'))));
        }
        let access = c.access.clone().unwrap_or_else(|| vec!["anonymous".into()]);
        let restricted = !access.iter().any(|g| g == "anonymous");
        let mut ls = vec!["KonText".to_string()];
        ls.extend(c.categories.iter().cloned());
        ls.extend(c.keywords.iter().cloned());
        if !lang.is_empty() {
            ls.push(format!("lang:{lang}"));
        }
        ls.extend(o.portal_label.iter().cloned());
        let label = [get("NAME"), c.attrs.get("name").cloned().unwrap_or_default(), c.ident.clone()]
            .into_iter()
            .find(|x| !x.is_empty())
            .unwrap_or_default();
        let root = if !data_path.is_empty() {
            pmap.map(Path::new(&data_path))
        } else {
            reg_path.as_deref().map(|p| pmap.map(p)).unwrap_or_default()
        };
        let mut v = Map::new();
        v.insert("id".into(), json!(id));
        v.insert("label".into(), json!(label));
        v.insert("project_root".into(), json!(root));
        v.insert("project_url".into(), o.kontext_url.as_ref().map(|u| json!(format!("{}/query?corpname={corpname}", u.trim_end_matches('/')))).unwrap_or(Value::Null));
        v.insert("preferred_backend".into(), json!("manatee"));
        v.insert("source_kind".into(), json!("manatee"));
        v.insert("interface_preference".into(), json!("kontext"));
        v.insert("supports_xml".into(), json!(false));
        v.insert("environment".into(), json!(o.environment));
        v.insert("labels".into(), labels(ls));
        v.insert("http_policy_mode".into(), json!("public_query"));
        v.insert("settings".into(), Value::Object(s));
        v.insert("is_current".into(), json!(true));
        out.push(Row {
            value: v,
            source: "kontext",
            access: access.join(",") + if restricted { " (RESTRICTED)" } else { "" },
            notes,
            match_key: corpname,
        });
    }
    if let Some(r) = registry {
        let listed: HashSet<String> = corpora.iter().map(|c| c.ident.to_lowercase()).collect();
        let mut names: Vec<String> = std::fs::read_dir(r)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| !n.starts_with('.') && !listed.contains(&n.to_lowercase()))
            .collect();
        names.sort();
        issues.extend(names.into_iter().map(|n| format!("registry '{n}' is not in the KonText corplist (not imported)")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corplist_nesting_keywords_access() {
        let xml = r#"<corplist title="LINDAT">
          <keywords><keyword ident="spoken"><label lang="cs">mluvený</label><label lang="en">Spoken</label></keyword></keywords>
          <corplist><title lang="cs">Česky</title><title lang="en">Czech</title>
            <corpus ident="ORAL2013" access="anonymous,authenticated" repo="http://hdl/x">
              <metadata><desc lang="en">Spoken Czech</desc><keywords><item>spoken</item></keywords></metadata>
            </corpus>
            <corpus ident="SECRET" access="cnc_users"/>
          </corplist>
        </corplist>"#;
        let c = read_corplist(xml).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].ident, "ORAL2013");
        assert_eq!(c[0].categories, vec!["LINDAT", "Czech"]);
        assert_eq!(c[0].keywords, vec!["Spoken"]);
        assert_eq!(c[0].desc, "Spoken Czech");
        assert_eq!(c[0].access.as_deref(), Some(&["anonymous".to_string(), "authenticated".to_string()][..]));
        assert_eq!(c[1].access.as_deref(), Some(&["cnc_users".to_string()][..]));
    }
}
