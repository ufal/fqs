//! LexFCS (Federated Content Search for Lexical Resources, Text+/CLARIN, 1.0): lexical resources as FCS
//! resources. The query (LexCQL) goes unchanged to the lexicon store's own program (`lexfcs.py search`,
//! from the lexicon store concept), which answers with entries already flattened into LexFCS fields; this
//! module only runs it and writes the Lex Data View, the Hits view with `kind`, and the parts of the
//! Endpoint Description (Lexical Search capability, SupportedLexFields, AvailableLexFields).

use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

use super::engine::{Engine, EngineError, Hit, Page, SearchArgs};
use super::sru::esc;

pub const CAPABILITY: &str = "http://clarin.eu/fcs/capability/lexical-search/1.0";
pub const MIME: &str = "application/x-clarin-fcs-lex+xml";

/// What a catalogue entry says about its lexicon (`settings.fcs`: engine "lex").
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LexSpec {
    /// the store (`*.db`)
    pub store: String,
    /// the resources of the store this FCS resource covers (empty: all)
    pub resources: Vec<String>,
    /// LexFCS field types it can be searched on and shows (AvailableLexFields)
    pub fields: Vec<String>,
    /// an entry page, with {uid} and {id}: each entry's `ref`
    pub entry_link: Option<String>,
    /// `lexfcs.py` and the Python to run it with
    pub script: Option<String>,
    pub python: Option<String>,
}

impl LexSpec {
    pub fn from_fcs(f: &Value) -> Option<LexSpec> {
        let s = |k: &str| f.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
        let list = |k: &str| -> Vec<String> {
            match f.get(k) {
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect(),
                Some(Value::String(v)) => v.split([',', ' ']).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string).collect(),
                _ => vec![],
            }
        };
        let mut fields = list("lex_fields");
        if fields.is_empty() {
            fields = vec!["lemma".into()]; // LexFCS: lemma is mandatory
        }
        Some(LexSpec {
            store: s("store")?,
            resources: list("resources"),
            fields,
            entry_link: s("entry_link"),
            script: s("lexfcs"),
            python: s("python"),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LexValue {
    pub text: String,
    /// attribute name (xml:lang, xml:id, idRefs, vocabRef, type, source, …) → value
    pub attrs: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LexField {
    pub ftype: String,
    pub values: Vec<LexValue>,
}

/// One entry in the LexFCS data model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LexEntry {
    pub lang: Option<String>,
    pub fields: Vec<LexField>,
}

impl LexEntry {
    pub fn from_json(v: &Value) -> LexEntry {
        let fields = v
            .get("fields")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|f| {
                        let ftype = f.get("type")?.as_str()?.to_string();
                        let values = f
                            .get("values")?
                            .as_array()?
                            .iter()
                            .filter_map(|x| {
                                let text = x.get("text")?.as_str()?.to_string();
                                let attrs = x
                                    .get("attrs")
                                    .and_then(Value::as_object)
                                    .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
                                    .unwrap_or_default();
                                Some(LexValue { text, attrs })
                            })
                            .collect();
                        Some(LexField { ftype, values })
                    })
                    .collect()
            })
            .unwrap_or_default();
        LexEntry { lang: v.get("lang").and_then(Value::as_str).map(str::to_string), fields }
    }
    fn first(&self, t: &str) -> Option<&str> {
        self.fields.iter().find(|f| f.ftype == t).and_then(|f| f.values.first()).map(|v| v.text.as_str())
    }
}

// ── the engine ─────────────────────────────────────────────────────────

/// Runs `lexfcs.py STORE search QUERY` (one process per request, like pando without its warm library).
pub struct LexEngine {
    pub python: String,
    pub script: String,
    pub spec: LexSpec,
    pub timeout: Duration,
}

/// `lexfcs.py`'s error kinds as SRU diagnostics: 10 query syntax, 16 unsupported index,
/// 19 unsupported relation, 48 query feature unsupported.
fn error_code(kind: &str) -> Option<u32> {
    match kind {
        "syntax" => Some(10),
        "index" => Some(16),
        "relation" => Some(19),
        "unsupported" => Some(48),
        _ => None,
    }
}

impl LexEngine {
    fn run(&self, query: &str, offset: u64, limit: u64) -> Result<Value, EngineError> {
        let mut cmd = Command::new(&self.python);
        cmd.args(["-I", "-B", &self.script, &self.spec.store, "search", query])
            .args(["--offset", &offset.to_string(), "--limit", &limit.to_string()]);
        if !self.spec.resources.is_empty() {
            cmd.args(["--resources", &self.spec.resources.join(",")]);
        }
        if let Some(r) = &self.spec.entry_link {
            cmd.args(["--ref", r]);
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| EngineError::System(format!("cannot run {} {}: {e}", self.python, self.script)))?;
        let out = wait_with_timeout(child, self.timeout)?;
        let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| {
            let err = String::from_utf8_lossy(&out.stderr);
            EngineError::System(format!("lexfcs.py gave no answer: {}", err.lines().last().unwrap_or("").trim()))
        })?;
        if let Some(msg) = v.get("error").and_then(Value::as_str) {
            let kind = v.get("kind").and_then(Value::as_str).unwrap_or("syntax");
            return Err(match error_code(kind) {
                Some(code) => EngineError::Diag(code, msg.to_string()),
                None => EngineError::System(msg.to_string()),
            });
        }
        Ok(v)
    }
}

fn wait_with_timeout(mut child: std::process::Child, timeout: Duration) -> Result<std::process::Output, EngineError> {
    let t0 = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map_err(|e| EngineError::System(format!("lexfcs.py: {e}"))),
            Ok(None) if t0.elapsed() > timeout => {
                let _ = child.kill();
                return Err(EngineError::System(format!("the lexicon search took longer than {} s", timeout.as_secs())));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(EngineError::System(format!("lexfcs.py: {e}"))),
        }
    }
}

impl Engine for LexEngine {
    fn search(&self, a: &SearchArgs<'_>) -> Result<Page, EngineError> {
        let q = a.queries.first().ok_or_else(|| EngineError::System("no query".into()))?;
        let v = self.run(q, a.offset, a.limit)?;
        let total = v.get("total").and_then(Value::as_u64).unwrap_or(0);
        let hits = v
            .get("rows")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .map(|r| Hit { doc: r.get("resource").and_then(Value::as_str).map(str::to_string), lex: Some(LexEntry::from_json(r)), ..Hit::default() })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Page { total, exact: true, hits })
    }
}

// ── views ──────────────────────────────────────────────────────────────

/// The Lex Data View (`lex:Entry`), with only the requested field types (`x-fcs-lex-fields`) when given.
pub fn lex_view(e: &LexEntry, only: &[String]) -> String {
    let lang = e.lang.as_ref().map(|l| format!(" xml:lang=\"{}\"", esc(l))).unwrap_or_default();
    let mut o = format!("<fcs:DataView type=\"{MIME}\">\n<lex:Entry xmlns:lex=\"http://clarin.eu/fcs/dataview/lex\"{lang}>\n");
    for f in &e.fields {
        if !only.is_empty() && !only.iter().any(|x| x.eq_ignore_ascii_case(&f.ftype)) {
            continue;
        }
        if f.values.is_empty() {
            continue;
        }
        o.push_str(&format!("<lex:Field type=\"{}\">\n", esc(&f.ftype)));
        for v in &f.values {
            let attrs: String = v.attrs.iter().map(|(k, x)| format!(" {}=\"{}\"", esc(k), esc(x))).collect();
            o.push_str(&format!("<lex:Value{attrs}>{}</lex:Value>\n", esc(&v.text)));
        }
        o.push_str("</lex:Field>\n");
    }
    o.push_str("</lex:Entry>\n</fcs:DataView>\n");
    o
}

/// The Hits view of an entry, with LexFCS's `kind` hints: "lemma: POS. definition".
pub fn hits_view(e: &LexEntry) -> String {
    let mut o = String::from("<fcs:DataView type=\"application/x-clarin-fcs-hits+xml\">\n<hits:Result xmlns:hits=\"http://clarin.eu/fcs/dataview/hits\">");
    let lemma = e.fields.iter().find(|f| f.ftype == "lemma").map(|f| f.values.iter().map(|v| v.text.as_str()).collect::<Vec<_>>().join(", "));
    o.push_str(&format!("<hits:Hit kind=\"lex-lemma\">{}</hits:Hit>", esc(lemma.as_deref().unwrap_or(""))));
    if let Some(p) = e.first("pos") {
        o.push_str(&format!(": <hits:Hit kind=\"lex-pos\">{}</hits:Hit>", esc(p)));
    }
    if let Some(d) = e.first("definition").or_else(|| e.first("translation")) {
        let kind = if e.first("definition").is_some() { "lex-definition" } else { "lex-translation" };
        o.push_str(&format!(". <hits:Hit kind=\"{kind}\">{}</hits:Hit>", esc(d)));
    }
    o.push_str("</hits:Result>\n</fcs:DataView>\n");
    o
}

/// `<ed:SupportedLexFields>`: every field type some resource offers, in LexFCS's order.
pub fn supported_fields_xml(all: &[String]) -> String {
    let mut o = String::from("<ed:SupportedLexFields>\n");
    for f in all {
        o.push_str(&format!("<ed:SupportedLexField id=\"{0}\">{0}</ed:SupportedLexField>\n", esc(f)));
    }
    o.push_str("</ed:SupportedLexFields>\n");
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry() -> LexEntry {
        LexEntry::from_json(&json!({"lang": "cs", "fields": [
            {"type": "lemma", "values": [{"text": "brát", "attrs": {"xml:lang": "cs"}}]},
            {"type": "pos", "values": [{"text": "VERB", "attrs": {"vocabRef": "https://universaldependencies.org/u/pos/"}}]},
            {"type": "definition", "values": [{"text": "přijímat & získávat", "attrs": {"xml:id": "u1"}}]},
            {"type": "citation", "values": [{"text": "bral úplatky", "attrs": {"idRefs": "u1", "type": "example"}}]}
        ]}))
    }

    #[test]
    fn lex_view_writes_fields_and_escapes() {
        let x = lex_view(&entry(), &[]);
        assert!(x.contains("<lex:Entry xmlns:lex=\"http://clarin.eu/fcs/dataview/lex\" xml:lang=\"cs\">"));
        assert!(x.contains("<lex:Field type=\"lemma\">\n<lex:Value xml:lang=\"cs\">brát</lex:Value>"));
        assert!(x.contains("přijímat &amp; získávat"));
        assert!(x.contains("idRefs=\"u1\""));
    }

    #[test]
    fn lex_fields_filter_the_view() {
        let x = lex_view(&entry(), &["lemma".into(), "POS".into()]);
        assert!(x.contains("type=\"pos\""));
        assert!(!x.contains("type=\"definition\""));
        assert!(!x.contains("type=\"citation\""));
    }

    #[test]
    fn hits_view_has_kinds() {
        let x = hits_view(&entry());
        assert!(x.contains("<hits:Hit kind=\"lex-lemma\">brát</hits:Hit>: <hits:Hit kind=\"lex-pos\">VERB</hits:Hit>. <hits:Hit kind=\"lex-definition\">"));
    }

    #[test]
    fn spec_needs_a_store_and_keeps_lemma() {
        assert!(LexSpec::from_fcs(&json!({"engine": "lex"})).is_none());
        let s = LexSpec::from_fcs(&json!({"store": "/x.db", "resources": "a,b"})).unwrap();
        assert_eq!(s.resources, vec!["a", "b"]);
        assert_eq!(s.fields, vec!["lemma"]);
    }

    #[test]
    fn error_kinds_are_sru_codes() {
        assert_eq!(error_code("index"), Some(16));
        assert_eq!(error_code("unsupported"), Some(48));
        assert_eq!(error_code("system"), None);
    }
}
