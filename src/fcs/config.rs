//! What FCS publishes about one resource (corpus), from a small JSON block.
//!
//! Everything lives under one object (in FQS: `capabilities.fcs`, overlaid by
//! `settings.fcs`; schema in dev/FQS-FCS2-IMPLEMENTATION.md §4), so another
//! front end can fill it from its own catalogue:
//!
//! ```json
//! { "enabled": true,
//!   "pid": "hdl:11234/1-5287",               // default: <base>/resource/<id>
//!   "title": "UD English EWT" | {"en": "…", "cs": "…"},
//!   "description": {"en": "…"}, "institution": {"en": "…"},
//!   "landing_page": "https://…",
//!   "languages": ["en"],                      // ISO 639-1 or -3; default "und"
//!   "layers": {"text": "word", "lemma": "lemma", "pos": {"attr": "upos"}},
//!   "dataviews": ["hits", "adv"],             // adv = Advanced Search too
//!   "sentence": "s", "within": {"text": "doc", "paragraph": "p"},
//!   "context": 5,                             // KWIC words on each side
//!   "doc_attr": "text_id", "tokid_attr": "id",// for {doc} / {tokid} in links
//!   "hit_link": {"frontend": "kontext", "base": "https://…", "corpus": "x"}
//!             | "https://…?pos={pos}&q={cql}" | "landing" | "none" }
//! ```

use serde_json::Value;

use super::query::Layer;
use super::translate::{Dialect, Mapping};

#[derive(Clone, Debug)]
pub struct Resource {
    pub id: String,
    pub pid: String,
    /// (language, text); the first one is primary
    pub titles: Vec<(String, String)>,
    pub descriptions: Vec<(String, String)>,
    pub institutions: Vec<(String, String)>,
    pub landing_page: Option<String>,
    /// ISO 639-3
    pub languages: Vec<String>,
    pub dialect: Dialect,
    pub mapping: Mapping,
    pub advanced: bool,
    pub context: u32,
    /// a hit's ResourceFragment `ref`
    pub hit_link: Option<HitLink>,
    /// structural attribute naming the document (CWB s-attribute, KonText ref)
    pub doc_attr: Option<String>,
    /// positional attribute with the token id (TEITOK: `id`)
    pub tokid_attr: Option<String>,
}

/// A link template and the query language its `{cql}` placeholder takes.
///
/// Placeholders: `{base}` `{corpus}` (from the preset), `{n}` (the hit's rank in
/// this resource's result, 1-based), `{pos}` / `{start}`,
/// `{end}`, `{doc}`, `{tokid}`, `{cql}` / `{query}` (URL-encoded), `{id}`,
/// `{pid}`. A link with a placeholder the hit cannot fill is left out.
#[derive(Clone, Debug, PartialEq)]
pub struct HitLink {
    pub template: String,
    pub dialect: Dialect,
}

impl HitLink {
    /// `"none"`, `"landing"`, a template string, or `{frontend, base, corpus, template}`
    /// with the presets kontext (KonText ≥ 0.16, `create_view`), kontext_first (older
    /// KonText), teitok, cqpweb, korp.
    pub fn from_value(v: &Value, own: Dialect, landing: Option<&str>) -> Option<HitLink> {
        match v {
            Value::String(s) if s == "none" || s.is_empty() => None,
            Value::String(s) if s == "landing" => landing.map(|l| HitLink { template: l.to_string(), dialect: own }),
            Value::String(s) => Some(HitLink { template: s.clone(), dialect: own }),
            Value::Object(o) => {
                let get = |k: &str| o.get(k).and_then(Value::as_str);
                let frontend = get("frontend").unwrap_or("template");
                let (preset, dialect) = match frontend {
                    "none" => return None,
                    "landing" => return landing.map(|l| HitLink { template: l.to_string(), dialect: own }),
                    // KonText ≥ 0.16: create_view is its entry for external links; the
                    // concordance is in corpus order like the FCS records, so hit {n}
                    // is line {n} (one line per page: exactly that hit)
                    "kontext" | "kontext_create_view" => (
                        "{base}/create_view?corpname={corpus}&q=q{cql}&pagesize=1&fromp={n}",
                        Dialect::Manatee,
                    ),
                    // older KonText forks (first_form + CQL row)
                    "kontext_first" => ("{base}/first?corpname={corpus}&queryselector=cqlrow&cql={cql}", Dialect::Manatee),
                    "teitok" => ("{base}/index.php?action=file&cid={doc}&jmp={tokid}", Dialect::Cwb),
                    "cqpweb" => ("{base}/concordance.php?c={corpus}&qmode=cqp&theData={cql}", Dialect::Cwb),
                    "korp" => ("{base}/#?corpus={corpus}&search=cqp&cqp={cql}", Dialect::Cwb),
                    _ => ("", own),
                };
                let mut t = get("template").unwrap_or(preset).to_string();
                if t.is_empty() {
                    return None;
                }
                if let Some(b) = get("base") {
                    t = t.replace("{base}", b.trim_end_matches('/'));
                }
                if let Some(c) = get("corpus") {
                    t = t.replace("{corpus}", c);
                }
                Some(HitLink { template: t, dialect })
            }
            _ => None,
        }
    }
}

fn str_or_map(v: Option<&Value>, fallback: &str) -> Vec<(String, String)> {
    match v {
        Some(Value::String(s)) if !s.trim().is_empty() => vec![("en".into(), s.trim().to_string())],
        Some(Value::Object(m)) => {
            let mut out: Vec<(String, String)> = m
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect();
            // English first (primary), others after
            out.sort_by_key(|(k, _)| k != "en");
            if out.is_empty() && !fallback.is_empty() {
                out.push(("en".into(), fallback.into()));
            }
            out
        }
        _ if !fallback.is_empty() => vec![("en".into(), fallback.into())],
        _ => vec![],
    }
}

/// ISO 639-1 → 639-3 for the languages we are likely to see; others as given.
pub fn iso639_3(code: &str) -> String {
    let c = code.trim().to_ascii_lowercase();
    let c = c.split(['-', '_']).next().unwrap_or("").to_string();
    let m = match c.as_str() {
        "en" => "eng", "cs" => "ces", "sk" => "slk", "de" => "deu", "nl" => "nld", "fr" => "fra",
        "es" => "spa", "it" => "ita", "pt" => "por", "pl" => "pol", "ru" => "rus", "uk" => "ukr",
        "sl" => "slv", "hr" => "hrv", "sr" => "srp", "bg" => "bul", "hu" => "hun", "fi" => "fin",
        "et" => "est", "lv" => "lav", "lt" => "lit", "sv" => "swe", "da" => "dan", "no" => "nor",
        "nb" => "nob", "nn" => "nno", "is" => "isl", "el" => "ell", "tr" => "tur", "ar" => "ara",
        "he" => "heb", "fa" => "fas", "hi" => "hin", "zh" => "zho", "ja" => "jpn", "ko" => "kor",
        "ro" => "ron", "ca" => "cat", "eu" => "eus", "gl" => "glg", "ga" => "gle", "cy" => "cym",
        "la" => "lat", "grc" => "grc", "af" => "afr", "sq" => "sqi", "mt" => "mlt", "be" => "bel",
        "ka" => "kat", "hy" => "hye", "id" => "ind", "vi" => "vie", "th" => "tha",
        "" => "und",
        other => return other.to_string(),
    };
    m.to_string()
}

/// The engine attributes a dialect usually has for the FCS layers.
pub fn default_layers(d: Dialect) -> Vec<(Layer, String)> {
    match d {
        Dialect::Pando => vec![(Layer::Text, "form".into()), (Layer::Lemma, "lemma".into()), (Layer::Pos, "upos".into())],
        Dialect::Cwb => vec![(Layer::Text, "word".into()), (Layer::Lemma, "lemma".into()), (Layer::Pos, "pos".into())],
        Dialect::Manatee => vec![(Layer::Text, "word".into()), (Layer::Lemma, "lemma".into()), (Layer::Pos, "tag".into())],
    }
}

pub struct ResourceSpec<'a> {
    pub id: &'a str,
    pub label: &'a str,
    pub landing_page: Option<&'a str>,
    /// the merged `fcs` object
    pub fcs: &'a Value,
    pub dialect: Dialect,
    /// endpoint base URL (for the default PID), without a trailing '/'
    pub base_url: Option<&'a str>,
    /// a hit link to use when `fcs.hit_link` is not set
    pub default_hit_link: Option<HitLink>,
}

impl Resource {
    pub fn from_spec(s: &ResourceSpec<'_>) -> Resource {
        let f = s.fcs;
        let g = |k: &str| f.get(k);
        let pid = g("pid")
            .or_else(|| g("resource_pid"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty() && !p.starts_with('<'))
            .map(str::to_string)
            .unwrap_or_else(|| match s.base_url {
                Some(b) => format!("{b}/resource/{}", s.id),
                None => s.id.to_string(),
            });
        let mut languages: Vec<String> = match g("languages") {
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(iso639_3).collect(),
            _ => vec![],
        };
        if languages.is_empty() {
            if let Some(l) = g("language").and_then(Value::as_str) {
                languages.push(iso639_3(l));
            }
        }
        if languages.is_empty() {
            languages.push("und".into());
        }
        languages.dedup();
        let mut layers = default_layers(s.dialect);
        if let Some(Value::Object(m)) = g("layers") {
            for (k, v) in m {
                let Some(l) = Layer::parse(k) else { continue };
                layers.retain(|(x, _)| *x != l);
                let attr = v.as_str().or_else(|| v.get("attr").and_then(Value::as_str));
                if let Some(a) = attr.filter(|a| !a.trim().is_empty()) {
                    layers.push((l, a.trim().to_string()));
                }
            }
        }
        // the text layer first (engines rely on it), then the fixed FCS order
        layers.sort_by_key(|(l, _)| Layer::ALL.iter().position(|x| x == l));
        let mut structures = vec![("sentence".to_string(), "s".to_string())];
        let mut scopes: Vec<(String, Value)> = Vec::new();
        for key in ["structures", "within"] {
            if let Some(Value::Object(m)) = g(key) {
                scopes.extend(m.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        }
        if let Some(sn) = g("sentence") {
            scopes.push(("sentence".into(), sn.clone()));
        }
        {
            let m = scopes;
            for (k, v) in &m {
                let Some(k) = super::translate::canonical_scope(k) else { continue };
                structures.retain(|(x, _)| x != k);
                if let Some(a) = v.as_str().filter(|a| !a.trim().is_empty()) {
                    structures.push((k.to_string(), a.trim().to_string()));
                }
            }
        }
        // a KonText hit link also gives the resource a landing page: its query form
        let kontext_landing = g("hit_link").and_then(|l| {
            let f = l.get("frontend").and_then(Value::as_str)?;
            if !f.starts_with("kontext") {
                return None;
            }
            let base = l.get("base").and_then(Value::as_str)?.trim_end_matches('/');
            let corpus = l.get("corpus").and_then(Value::as_str)?;
            Some(format!("{base}/query?corpname={corpus}"))
        });
        let landing_page = g("landing_page")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(kontext_landing)
            .or(s.landing_page.map(str::to_string));
        let hit_link = match g("hit_link") {
            Some(v) => HitLink::from_value(v, s.dialect, landing_page.as_deref()),
            None => s.default_hit_link.clone(),
        };
        let views: Vec<String> = ["dataviews", "supports_dataviews"]
            .iter()
            .find_map(|k| g(k).and_then(Value::as_array))
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let advanced = g("advanced")
            .and_then(Value::as_bool)
            .unwrap_or(views.is_empty() || views.iter().any(|v| v == "adv" || v == "advanced"));
        let attr = |k: &str| g(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string);
        Resource {
            id: s.id.to_string(),
            pid,
            titles: str_or_map(g("title"), s.label),
            descriptions: str_or_map(g("description"), ""),
            institutions: str_or_map(g("institution"), ""),
            landing_page,
            languages,
            dialect: s.dialect,
            mapping: Mapping { layers, structures },
            advanced,
            context: g("context").and_then(Value::as_u64).map(|n| n.clamp(1, 50) as u32).unwrap_or(5),
            hit_link,
            doc_attr: attr("doc_attr"),
            tokid_attr: attr("tokid_attr"),
        }
    }

    pub fn layers(&self) -> Vec<Layer> {
        self.mapping.layers.iter().map(|(l, _)| *l).collect()
    }

    /// Matches an `x-fcs-context` item: the PID or the plain id.
    pub fn matches(&self, key: &str) -> bool {
        self.pid == key || self.id == key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spec() {
        let fcs = json!({"title": {"cs": "Česky", "en": "English"}, "languages": ["en", "cs"],
                         "layers": {"pos": {"attr": "xpos"}, "lemma": null}, "within": {"text": "doc"},
                         "dataviews": ["hits"], "pid": "<PID_OR_EMPTY>",
                         "hit_link": {"frontend": "kontext", "base": "https://k/", "corpus": "c"}});
        let r = Resource::from_spec(&ResourceSpec {
            id: "x", label: "X", landing_page: None, fcs: &fcs, dialect: Dialect::Pando,
            base_url: Some("http://h/fcs"), default_hit_link: None,
        });
        assert_eq!(r.pid, "http://h/fcs/resource/x");
        assert_eq!(r.titles[0], ("en".to_string(), "English".to_string()));
        assert_eq!(r.languages, vec!["eng", "ces"]);
        assert_eq!(r.mapping.layers, vec![(Layer::Text, "form".to_string()), (Layer::Pos, "xpos".to_string())]);
        assert_eq!(r.mapping.structure("text"), Some("doc"));
        assert_eq!(r.mapping.structure("s"), Some("s"));
        assert!(!r.advanced);
        assert_eq!(r.hit_link, Some(HitLink { template: "https://k/create_view?corpname=c&q=q{cql}&pagesize=1&fromp={n}".into(), dialect: Dialect::Manatee }));
        assert_eq!(r.landing_page.as_deref(), Some("https://k/query?corpname=c"));
    }
}
