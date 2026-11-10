//! `fqs corpora import`: catalogue rows from what existing TEITOK and KonText installations already
//! know about their corpora, so they need not be registered one by one. Nothing is written to the
//! catalogue: the rows go to a JSON file for `fqs corpora upsert-json`, with a review table and a
//! summary, to be checked first.
//!
//! - teitok.rs: every TEITOK project (settings.xml, indexes) and the shared corpus list;
//! - kontext.rs: KonText's corplist.xml with the Manatee registry files;
//! - merge.rs: rows already in the catalogue (an export) keep what is set there;
//! - this file: the options, linking the same corpus in both front-ends, the output.
//!
//! Access: KonText gives each corpus the groups that may use it; FQS has no groups. Every corpus
//! becomes public_query, the groups are kept in settings.access, and the corpora not open to
//! "anonymous" are listed: the front-ends' login protects them, with FQS's query port reachable
//! from the front-ends only. FCS should not be enabled for them.

mod kontext;
mod merge;
mod teitok;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// The options of `fqs corpora import` (main.rs maps the CLI arguments onto these).
#[derive(Debug, Default, Clone)]
pub struct Options {
    pub teitok: Option<PathBuf>,
    pub teitok_corplist: Option<PathBuf>,
    pub teitok_url: Option<String>,
    /// pando indexes kept apart from the TEITOK projects: `<DIR>/<id>/pando` or `<DIR>/<id>`
    pub pando_root: Option<PathBuf>,
    pub kontext_corplist: Option<PathBuf>,
    pub registry: Option<PathBuf>,
    pub kontext_url: Option<String>,
    /// FQS opens the Manatee data itself (true), or reaches it through KonText (false)
    pub manatee_native: bool,
    pub map_path: Vec<String>,
    pub existing: Option<PathBuf>,
    pub environment: String,
    pub portal_label: Option<String>,
    pub portal: Option<String>,
    pub portal_url: Option<String>,
    pub only: Vec<String>,
    /// leave these corpus ids out
    pub exclude: Vec<String>,
    /// private until released: http_policy_mode auth_required (admins only)
    pub private: bool,
    /// groups the rows are restricted to (settings.access)
    pub access: Vec<String>,
}

/// One catalogue row and what the review table says about it.
pub(crate) struct Row {
    pub value: Map<String, Value>,
    /// teitok | kontext
    pub source: &'static str,
    pub access: String,
    pub notes: Vec<String>,
    /// the name the other front-end may know the corpus by (the CWB corpus, the KonText corpname)
    pub match_key: String,
}

impl Row {
    pub fn id(&self) -> &str {
        self.value.get("id").and_then(Value::as_str).unwrap_or("")
    }
    pub(crate) fn str_field(&self, k: &str) -> String {
        self.value.get(k).and_then(Value::as_str).unwrap_or("").to_string()
    }
    fn settings_mut(&mut self) -> &mut Map<String, Value> {
        let s = self.value.entry("settings").or_insert_with(|| json!({}));
        if !s.is_object() {
            *s = json!({});
        }
        s.as_object_mut().expect("object")
    }
}

/// `--map-path OLD=NEW`: paths as the FQS server will see them (the longest OLD first).
pub(crate) struct PathMap(Vec<(String, String)>);

impl PathMap {
    fn new(pairs: &[String]) -> Result<PathMap> {
        let mut v = Vec::new();
        for p in pairs {
            let Some((old, new)) = p.split_once('=') else { bail!("--map-path needs OLD=NEW, got '{p}'") };
            v.push((old.trim_end_matches('/').to_string(), new.trim_end_matches('/').to_string()));
        }
        v.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        Ok(PathMap(v))
    }
    pub fn map(&self, path: &Path) -> String {
        let s = path.to_string_lossy().to_string();
        for (old, new) in &self.0 {
            if s == *old || s.starts_with(&format!("{old}/")) {
                return format!("{new}{}", &s[old.len()..]);
            }
        }
        s
    }
}

/// The id FQS's own scan makes from a name (scan.rs).
pub(crate) fn corpus_id(s: &str) -> String {
    crate::scan::sanitize_id(s)
}

/// Unique, non-empty, in order.
pub(crate) fn labels(items: impl IntoIterator<Item = String>) -> Value {
    let mut seen = HashSet::new();
    Value::Array(items.into_iter().filter(|x| !x.is_empty() && seen.insert(x.clone())).map(Value::String).collect())
}

/// What an import made.
pub struct Outcome {
    /// the rows for `fqs corpora upsert-json`
    pub rows: Vec<Value>,
    /// the review table (TSV)
    pub report: String,
    /// the summary (for stderr)
    pub summary: Vec<String>,
    /// the rows that need a look, with their notes (listed when there is no report to see them in)
    pub noted: Vec<String>,
    /// things outside the rows (a corplist entry without a project, a registry not in the corplist)
    pub issues: Vec<String>,
}

pub fn run(o: &Options) -> Result<Outcome> {
    if o.teitok.is_none() && o.kontext_corplist.is_none() {
        bail!("give --teitok DIR and/or --kontext-corplist FILE (or --kontext-corplist auto)");
    }
    if o.pando_root.is_some() && o.teitok.is_none() {
        bail!("--pando-root needs --teitok (it adds pando indexes to the TEITOK projects)");
    }
    if o.registry.is_some() && o.kontext_corplist.is_none() {
        bail!("--registry needs --kontext-corplist (the corplist says which registries are corpora)");
    }
    let pmap = PathMap::new(&o.map_path)?;
    let mut issues = Vec::new();
    let mut found = Vec::new();
    let mut rows = match &o.teitok {
        Some(dir) => teitok::rows(o, dir, &pmap, &mut issues)?,
        None => Vec::new(),
    };
    if let Some(cl) = &o.kontext_corplist {
        let (corplist, registry) = kontext::resolve_sources(cl, o.registry.as_deref(), &mut found)?;
        let mut taken: HashSet<String> = rows.iter().map(|r| r.id().to_string()).collect();
        rows.extend(kontext::rows(o, &corplist, registry.as_deref(), &pmap, &mut issues, &mut taken)?);
    }
    if !o.only.is_empty() {
        rows.retain(|r| o.only.iter().any(|w| w == r.id()));
    }
    rows.retain(|r| !o.exclude.iter().any(|w| w == r.id()));
    // public_query is written explicitly, so that importing a released version again makes it public
    for r in rows.iter_mut() {
        r.value.insert("http_policy_mode".into(), json!(if o.private { "auth_required" } else { "public_query" }));
        if o.private {
            r.notes.push("private (auth_required) until imported without --private".into());
        }
        if !o.access.is_empty() {
            r.settings_mut().insert("access".into(), json!(o.access));
            r.access = o.access.join(",");
        }
    }
    link_same_corpus(&mut rows);

    let existing = match &o.existing {
        Some(f) => merge::read_existing(f)?,
        None => HashMap::new(),
    };
    let (mut out, mut kept_total, mut updated) = (Vec::new(), 0usize, 0usize);
    for r in rows.iter_mut() {
        if let Some(old) = existing.get(r.id()) {
            let (m, kept) = merge::merge_existing(&r.value, old);
            kept_total += kept;
            updated += 1;
            r.notes.push(format!("already in the catalogue: {kept} field(s) kept as they are"));
            if m.len() > 1 {
                out.push(Value::Object(m));
            }
        } else {
            out.push(Value::Object(r.value.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), v.clone())).collect()));
        }
    }

    let mut report = String::from("id\tsource\tbackend\tlabel\taccess\tproject_root\tproject_url\tnotes\n");
    for r in &rows {
        let cells = [
            r.id().to_string(), r.source.to_string(), r.str_field("preferred_backend"), r.str_field("label"),
            r.access.clone(), r.str_field("project_root"), r.str_field("project_url"), r.notes.join("; "),
        ];
        report.push_str(&cells.iter().map(|c| c.replace(['\t', '\n'], " ")).collect::<Vec<_>>().join("\t"));
        report.push('\n');
    }

    let n_t = rows.iter().filter(|r| r.source == "teitok").count();
    let mut summary = found;
    summary.push(format!(
        "{} row(s) written ({n_t} TEITOK, {} KonText{})",
        out.len(),
        rows.len() - n_t,
        if existing.is_empty() { String::new() } else { format!("; {updated} already in the catalogue, {kept_total} field(s) kept") }
    ));
    let restricted: Vec<&str> = rows.iter().filter(|r| r.access.contains("RESTRICTED")).map(|r| r.id()).collect();
    if !restricted.is_empty() {
        summary.push(format!(
            "{} KonText corpora are not open to anonymous users (kept in settings.access; FQS does not check groups): {}{}",
            restricted.len(),
            restricted.iter().take(12).cloned().collect::<Vec<_>>().join(", "),
            if restricted.len() > 12 { " …" } else { "" }
        ));
    }
    let noted: Vec<&Row> = rows
        .iter()
        .filter(|r| r.notes.iter().any(|n| !n.starts_with("also in") && !n.starts_with("already in")))
        .collect();
    let noted = noted.iter().map(|r| format!("{}: {}", r.id(), r.notes.join("; "))).collect();
    Ok(Outcome { rows: out, report, summary, noted, issues })
}

/// The same corpus in both front-ends: TEITOK's CWB corpus (tt-x) or project id is KonText's
/// corpname. The KonText row then names the TEITOK project (its document view).
fn link_same_corpus(rows: &mut [Row]) {
    let mut by_key: HashMap<String, (String, Option<String>)> = HashMap::new();
    for r in rows.iter().filter(|r| r.source == "teitok") {
        let t = (r.id().to_string(), r.value.get("project_url").and_then(Value::as_str).map(str::to_string));
        if !r.match_key.is_empty() {
            by_key.entry(r.match_key.clone()).or_insert_with(|| t.clone());
        }
        by_key.entry(r.id().to_string()).or_insert(t);
    }
    for r in rows.iter_mut().filter(|r| r.source == "kontext") {
        let key = r.match_key.clone();
        let hit = by_key.get(&key).or_else(|| key.strip_prefix("tt-").and_then(|k| by_key.get(k))).cloned();
        if let Some((id, url)) = hit {
            let s = r.settings_mut();
            s.insert("teitok_project".into(), json!(id));
            if let Some(u) = url {
                s.insert("teitok_project_url".into(), json!(u));
            }
            r.notes.push(format!("also in TEITOK as '{id}'"));
        }
    }
}

/// Write the rows and the report; the summary goes to stderr.
pub fn write(outcome: &Outcome, out: Option<&Path>, report: Option<&Path>) -> Result<()> {
    let text = serde_json::to_string_pretty(&outcome.rows)? + "\n";
    match out {
        Some(p) => std::fs::write(p, text).with_context(|| format!("writing {}", p.display()))?,
        None => print!("{text}"),
    }
    if let Some(p) = report {
        std::fs::write(p, &outcome.report).with_context(|| format!("writing {}", p.display()))?;
    }
    for line in &outcome.summary {
        eprintln!("{line}");
    }
    if !outcome.noted.is_empty() {
        match report {
            Some(p) => eprintln!("{} row(s) need a look (see {})", outcome.noted.len(), p.display()),
            None => {
                eprintln!("{} row(s) need a look:", outcome.noted.len());
                for n in &outcome.noted {
                    eprintln!("  {n}");
                }
            }
        }
    }
    for i in &outcome.issues {
        eprintln!("  {i}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_map_longest_first() {
        let m = PathMap::new(&["/opt=/x".into(), "/opt/manatee=/var/lib/manatee".into()]).unwrap();
        assert_eq!(m.map(Path::new("/opt/manatee/sir")), "/var/lib/manatee/sir");
        assert_eq!(m.map(Path::new("/opt/other")), "/x/other");
        assert_eq!(m.map(Path::new("/optional")), "/optional");
        assert!(PathMap::new(&["nothing".into()]).is_err());
    }

    #[test]
    fn labels_unique_in_order() {
        assert_eq!(labels(["a".into(), "".into(), "b".into(), "a".into()]), json!(["a", "b"]));
    }
}
