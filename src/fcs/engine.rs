//! Search engines behind FCS: each turns native queries into a page of hits.
//!
//! * `PandoEngine` — pando's `/query` JSON, over any transport: FQS's warm
//!   in-process corpus, or a plain `pando-server` over HTTP.
//! * `CwbEngine` — runs `cqp` directly (`size` + `tabulate`), no other tooling.
//! * `KontextEngine` — Manatee through a KonText (or NoSketch-style) HTTP
//!   concordance with `format=json`; old (Lines with `attr` items) and new
//!   (`posattrs`) KonText outputs are both read.
//!
//! None of them depends on flexicorp: they need only the engine itself.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use super::query::Layer;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Token {
    pub text: String,
    /// values of the non-text layers (lemma, pos, …) when the engine gave them
    pub layers: Vec<(Layer, String)>,
}

impl Token {
    fn text(s: &str) -> Token {
        Token { text: s.to_string(), layers: vec![] }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hit {
    pub left: Vec<Token>,
    pub kwic: Vec<Token>,
    pub right: Vec<Token>,
    /// corpus position of the first / last matched token
    pub start: Option<u64>,
    pub end: Option<u64>,
    /// document id / reference, when known
    pub doc: Option<String>,
    /// id of the first matched token (`tokid_attr`), when known
    pub tokid: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Page {
    pub total: u64,
    pub exact: bool,
    pub hits: Vec<Hit>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EngineError {
    /// the engine rejected the query (→ query syntax / unsupported diagnostic)
    Query(String),
    /// anything else (→ general system error)
    System(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Query(s) | EngineError::System(s) => write!(f, "{s}"),
        }
    }
}

pub struct SearchArgs<'a> {
    /// native queries; more than one = their union (pando)
    pub queries: &'a [String],
    pub offset: u64,
    pub limit: u64,
    /// words of context on each side
    pub context: u32,
    /// layer → engine attribute (the text layer first)
    pub layers: &'a [(Layer, String)],
    /// structure attribute naming the document (CWB s-attribute, KonText ref)
    pub doc_attr: Option<&'a str>,
    /// positional attribute holding the token id
    pub tokid_attr: Option<&'a str>,
}

pub trait Engine: Send {
    fn search(&self, a: &SearchArgs<'_>) -> Result<Page, EngineError>;
}

// ── pando ──────────────────────────────────────────────────────────────

/// `POST /query` body → (HTTP status, JSON reply).
pub type PandoTransport = Box<dyn Fn(&str) -> Result<(u16, Value), String> + Send>;

pub struct PandoEngine {
    pub transport: PandoTransport,
    /// extra members for the `/query` body (tier, timeout_ms, …)
    pub extra: serde_json::Map<String, Value>,
}

/// A transport to a stand-alone `pando-server` (`base` = `http://host:port`).
pub fn pando_http_transport(base: &str, timeout: Duration) -> PandoTransport {
    let url = format!("{}/query", base.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    Box::new(move |body: &str| match agent.post(&url).set("Content-Type", "application/json").send_string(body) {
        Ok(r) => {
            let st = r.status();
            let v: Value = r.into_json().map_err(|e| format!("pando-server reply: {e}"))?;
            Ok((st, v))
        }
        Err(ureq::Error::Status(st, r)) => {
            let v = r.into_json::<Value>().unwrap_or_else(|_| json!({"error": format!("HTTP {st}")}));
            Ok((st, v))
        }
        Err(e) => Err(format!("pando-server {url}: {e}")),
    })
}

/// A transport that runs the `pando` command line (same JSON as `/query`), one
/// process per request: for hosts without the warm library or a pando-server.
pub fn pando_cli_transport(binary: String, index_dir: PathBuf) -> PandoTransport {
    Box::new(move |body: &str| {
        let b: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
        let s = |k: &str| b.get(k).map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
        let mut cmd = Command::new(&binary);
        cmd.arg(&index_dir)
            .arg(s("query").unwrap_or_default())
            .args(["--json", "--api", "--total", "--strict-quoted-strings"]);
        for (k, flag) in [("offset", "--offset"), ("limit", "--limit"), ("context", "--context"), ("attrs", "--attrs")] {
            if let Some(v) = s(k).filter(|v| !v.is_empty() && v != "null") {
                cmd.arg(flag).arg(v);
            }
        }
        let out = cmd.output().map_err(|e| format!("cannot run {binary}: {e}"))?;
        let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| {
            format!("pando CLI: {}", String::from_utf8_lossy(&out.stderr).trim().chars().take(300).collect::<String>())
        })?;
        // the CLI exits 0 with {"ok": false, …} for a bad query
        let status = if v.get("ok").and_then(Value::as_bool) == Some(false) { 400 } else { 200 };
        Ok((status, v))
    })
}

impl PandoEngine {
    fn one(&self, q: &str, offset: u64, limit: u64, a: &SearchArgs<'_>) -> Result<(u64, bool, Vec<Hit>), EngineError> {
        let mut attrs: Vec<&str> = a.layers.iter().map(|(_, v)| v.as_str()).collect();
        if let Some(t) = a.tokid_attr {
            attrs.push(t);
        }
        let mut body = json!({
            "query": q,
            "offset": offset,
            "limit": limit,
            "context": a.context,
            "total": true,
            "strict_quoted_strings": true,
            "attrs": attrs.join(","),
        });
        for (k, v) in &self.extra {
            body[k] = v.clone();
        }
        let (status, v) = (self.transport)(&body.to_string()).map_err(EngineError::System)?;
        if status >= 400 || v.get("ok").and_then(Value::as_bool) == Some(false) {
            // pando-server: "error": "…"; the pando CLI: "error": {"stage", "message"}
            let msg = v
                .get("error")
                .and_then(|e| e.as_str().or_else(|| e.get("message").and_then(Value::as_str)))
                .unwrap_or("pando error")
                .to_string();
            return Err(if status == 400 { EngineError::Query(msg) } else { EngineError::System(msg) });
        }
        let res = v.get("result").unwrap_or(&v);
        let page = res.get("page").cloned().unwrap_or(Value::Null);
        let total = page.get("total").and_then(Value::as_u64).unwrap_or(0);
        let exact = page.get("total_exact").and_then(Value::as_bool).unwrap_or(true);
        let text_attr = a.layers.iter().find(|(l, _)| *l == Layer::Text).map(|(_, v)| v.as_str()).unwrap_or("form");
        let mut hits = Vec::new();
        for h in res.get("hits").and_then(Value::as_array).into_iter().flatten() {
            hits.push(pando_hit(h, text_attr, a.layers, a.tokid_attr));
        }
        Ok((total, exact, hits))
    }
}

fn split_words(s: &str) -> Vec<Token> {
    s.split_whitespace().map(Token::text).collect()
}

fn pando_hit(h: &Value, text_attr: &str, layers: &[(Layer, String)], tokid_attr: Option<&str>) -> Hit {
    let ctx = h.get("context");
    let get = |k: &str| ctx.and_then(|c| c.get(k)).and_then(Value::as_str).unwrap_or("");
    let start = h.get("match_start").and_then(Value::as_u64);
    let end = h.get("match_end").and_then(Value::as_u64);
    // tokens the engine reports (the matched query tokens, with attributes)
    let mut by_pos: HashMap<u64, &Value> = HashMap::new();
    for t in h.get("tokens").and_then(Value::as_array).into_iter().flatten() {
        if let Some(p) = t.get("pos").and_then(Value::as_u64) {
            by_pos.insert(p, t);
        }
    }
    let words = split_words(get("match"));
    let span = match (start, end) {
        (Some(s), Some(e)) if e >= s => (e - s + 1) as usize,
        _ => 0,
    };
    let to_token = |t: &Value, fallback: &str| {
        let text = t.get(text_attr).and_then(Value::as_str).unwrap_or(fallback).to_string();
        let mut ls = Vec::new();
        for (l, attr) in layers {
            if *l == Layer::Text {
                continue;
            }
            if let Some(v) = t.get(attr.as_str()).and_then(Value::as_str) {
                ls.push((*l, v.to_string()));
            }
        }
        Token { text, layers: ls }
    };
    let kwic = if span > 0 && span == words.len() {
        let s = start.unwrap();
        words
            .iter()
            .enumerate()
            .map(|(i, w)| match by_pos.get(&(s + i as u64)) {
                Some(t) => to_token(t, &w.text),
                None => w.clone(),
            })
            .collect()
    } else if !by_pos.is_empty() {
        let mut ps: Vec<_> = by_pos.keys().copied().collect();
        ps.sort();
        ps.iter().map(|p| to_token(by_pos[p], "")).collect()
    } else {
        words
    };
    Hit {
        left: split_words(get("left")),
        kwic,
        right: split_words(get("right")),
        start,
        end,
        doc: h.get("doc_id").and_then(Value::as_str).map(str::to_string),
        tokid: tokid_attr.and_then(|t| {
            let mut ps: Vec<&u64> = by_pos.keys().collect();
            ps.sort();
            ps.first().and_then(|p| by_pos[*p].get(t)).and_then(Value::as_str).map(str::to_string)
        }),
    }
}

/// Upper bound on `offset + limit` when a query is a union of several.
pub const MAX_UNION_WINDOW: u64 = 20_000;

impl Engine for PandoEngine {
    fn search(&self, a: &SearchArgs<'_>) -> Result<Page, EngineError> {
        if a.queries.len() == 1 {
            let (total, exact, hits) = self.one(&a.queries[0], a.offset, a.limit, a)?;
            return Ok(Page { total, exact, hits });
        }
        // a union: the first offset+limit hits of each, merged in corpus order
        let window = a.offset + a.limit;
        if window > MAX_UNION_WINDOW {
            return Err(EngineError::Query(format!(
                "paging beyond {MAX_UNION_WINDOW} records is not supported for this query"
            )));
        }
        let mut total = 0u64;
        let mut all: Vec<Hit> = Vec::new();
        for q in a.queries {
            let (t, _, hits) = self.one(q, 0, window, a)?;
            total += t;
            all.extend(hits);
        }
        all.sort_by_key(|h| (h.start, h.end));
        let before = all.len();
        all.dedup_by_key(|h| (h.start, h.end));
        // overlaps seen inside the window are known; beyond it the sum is an upper bound
        total = total.saturating_sub((before - all.len()) as u64);
        let hits = all.into_iter().skip(a.offset as usize).take(a.limit as usize).collect();
        Ok(Page { total, exact: false, hits })
    }
}

// ── CWB ────────────────────────────────────────────────────────────────

pub struct CwbEngine {
    pub cqp: String,
    pub registry: Option<PathBuf>,
    /// CWB corpus id (upper case in CQP)
    pub corpus: String,
    pub cwd: Option<PathBuf>,
}

const EOL: &str = "-::-EOL-::-";

impl Engine for CwbEngine {
    fn search(&self, a: &SearchArgs<'_>) -> Result<Page, EngineError> {
        let q = a.queries.first().ok_or_else(|| EngineError::System("no query".into()))?;
        let text = a.layers.iter().find(|(l, _)| *l == Layer::Text).map(|(_, v)| v.as_str()).unwrap_or("word");
        let others: Vec<&(Layer, String)> = a.layers.iter().filter(|(l, _)| *l != Layer::Text).collect();
        let c = a.context.max(1);
        let mut cols = vec![
            "match".to_string(),
            "matchend".to_string(),
            format!("match[-{c}]..match[-1] {text}"),
            format!("match..matchend {text}"),
            format!("matchend[1]..matchend[{c}] {text}"),
        ];
        for (_, attr) in &others {
            cols.push(format!("match..matchend {attr}"));
        }
        if let Some(d) = a.doc_attr {
            cols.push(format!("match {d}"));
        }
        if let Some(t) = a.tokid_attr {
            cols.push(format!("match {t}"));
        }
        let mut script = format!("set PrettyPrint off;\n{};\nFcsHits = {q};\nsize FcsHits;\n.EOL.;\n", self.corpus.to_uppercase());
        if a.limit > 0 {
            script.push_str(&format!(
                "tabulate FcsHits {} {} {};\n.EOL.;\n",
                a.offset,
                a.offset + a.limit - 1,
                cols.join(", ")
            ));
        }
        let mut cmd = Command::new(&self.cqp);
        if let Some(r) = &self.registry {
            cmd.arg("-r").arg(r);
        }
        if let Some(d) = &self.cwd {
            cmd.current_dir(d);
        }
        cmd.arg("-c").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| EngineError::System(format!("cannot run {}: {e}", self.cqp)))?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .map_err(|e| EngineError::System(format!("cqp stdin: {e}")))?;
        let out = child.wait_with_output().map_err(|e| EngineError::System(format!("cqp: {e}")))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        parse_cqp_output(&stdout, &stderr, others.iter().map(|(l, _)| *l).collect(), a.doc_attr.is_some(), a.tokid_attr.is_some())
    }
}

fn parse_cqp_output(stdout: &str, stderr: &str, layers: Vec<Layer>, with_doc: bool, with_tokid: bool) -> Result<Page, EngineError> {
    let all = format!("{stdout}\n{stderr}");
    if let Some(i) = all.find("CQP Error").or_else(|| all.find("CQP Syntax Error")).or_else(|| all.find("PARSE ERROR")) {
        let msg: String = all[i..].lines().map(str::trim).filter(|l| !l.is_empty()).take(4).collect::<Vec<_>>().join(" ");
        return Err(if msg.contains("undefined") && msg.contains("Corpus") {
            EngineError::System(msg)
        } else {
            EngineError::Query(msg)
        });
    }
    let mut blocks = stdout.split(EOL);
    let head = blocks.next().unwrap_or("");
    let total = head
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("CQP version"))
        .last()
        .and_then(|l| l.parse::<u64>().ok())
        .ok_or_else(|| EngineError::System(format!("unexpected cqp output: {}", head.trim())))?;
    let mut hits = Vec::new();
    if let Some(tab) = blocks.next() {
        for line in tab.lines().filter(|l| !l.trim().is_empty()) {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 5 {
                continue;
            }
            let kw: Vec<&str> = f[3].split(' ').filter(|s| !s.is_empty()).collect();
            let per_layer: Vec<Vec<&str>> = (0..layers.len())
                .map(|i| f.get(5 + i).map(|s| s.split(' ').filter(|s| !s.is_empty()).collect()).unwrap_or_default())
                .collect();
            let kwic = kw
                .iter()
                .enumerate()
                .map(|(i, w)| Token {
                    text: w.to_string(),
                    layers: layers
                        .iter()
                        .enumerate()
                        .filter_map(|(k, l)| per_layer[k].get(i).map(|v| (*l, v.to_string())))
                        .collect(),
                })
                .collect();
            let extra = |k: usize| f.get(5 + layers.len() + k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            let doc = if with_doc { extra(0) } else { None };
            let tokid = if with_tokid { extra(usize::from(with_doc)) } else { None };
            hits.push(Hit {
                left: split_words(f[2]),
                kwic,
                right: split_words(f[4]),
                start: f[0].trim().parse().ok(),
                end: f[1].trim().parse().ok(),
                doc,
                tokid,
            });
        }
    }
    Ok(Page { total, exact: true, hits })
}

// ── KonText / Manatee ──────────────────────────────────────────────────

pub struct KontextEngine {
    /// KonText base URL (…/services/kontext)
    pub url: String,
    pub corpname: String,
    /// `view` (all KonText versions) or `create_view` (KonText ≥ 0.16)
    pub action: String,
    pub timeout: Duration,
    /// extra query parameters (e.g. a user name for a proxied login)
    pub extra_params: Vec<(String, String)>,
}

pub fn url_encode(s: &str) -> String {
    let mut o = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

impl KontextEngine {
    fn page(&self, q: &str, fromp: u64, pagesize: u64, a: &SearchArgs<'_>) -> Result<Value, EngineError> {
        let attrs: Vec<&str> = a.layers.iter().map(|(_, v)| v.as_str()).collect();
        let c = a.context.max(1);
        let mut params: Vec<(String, String)> = vec![
            ("corpname".into(), self.corpname.clone()),
            ("q".into(), format!("q{q}")),
            ("format".into(), "json".into()),
            ("pagesize".into(), pagesize.max(1).to_string()),
            ("fromp".into(), fromp.to_string()),
            ("attrs".into(), attrs.join(",")),
            ("ctxattrs".into(), attrs.first().copied().unwrap_or("word").to_string()),
            ("attr_allpos".into(), "kw".into()),
            ("attr_vmode".into(), "visible-kwic".into()),
            ("structs".into(), String::new()),
            ("refs".into(), a.doc_attr.map(|r| format!("={r}")).unwrap_or_default()),
            ("kwicleftctx".into(), format!("-{c}")),
            ("kwicrightctx".into(), c.to_string()),
            ("viewmode".into(), "kwic".into()),
            ("asnc".into(), "0".into()),
            ("async".into(), "0".into()),
        ];
        params.extend(self.extra_params.iter().cloned());
        let qs: Vec<String> = params.iter().map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v))).collect();
        let url = format!("{}/{}?{}", self.url.trim_end_matches('/'), self.action, qs.join("&"));
        let agent = ureq::AgentBuilder::new().timeout(self.timeout).build();
        let v: Value = match agent.get(&url).call() {
            Ok(r) => r.into_json().map_err(|e| EngineError::System(format!("KonText reply is not JSON: {e}")))?,
            Err(ureq::Error::Status(st, r)) => {
                let v = r.into_json::<Value>().unwrap_or(Value::Null);
                let msg = kontext_error(&v).unwrap_or_else(|| format!("KonText HTTP {st}"));
                return Err(if st == 400 || st == 422 { EngineError::Query(msg) } else { EngineError::System(msg) });
            }
            Err(e) => return Err(EngineError::System(format!("KonText: {e}"))),
        };
        if let Some(msg) = kontext_error(&v) {
            return Err(EngineError::Query(msg));
        }
        Ok(v)
    }
}

fn kontext_error(v: &Value) -> Option<String> {
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        return Some(e.to_string());
    }
    for key in ["messages", "system_messages"] {
        for m in v.get(key).and_then(Value::as_array).into_iter().flatten() {
            // ["error", "text"] or {"type": "error", "text"/"message": …}
            let (kind, text) = match m {
                Value::Array(a) => (a.first().and_then(Value::as_str), a.get(1).and_then(Value::as_str)),
                Value::Object(_) => (
                    m.get("type").or_else(|| m.get("message_type")).and_then(Value::as_str),
                    m.get("text").or_else(|| m.get("message")).or_else(|| m.get("content")).and_then(Value::as_str),
                ),
                _ => (None, None),
            };
            if kind == Some("error") {
                return Some(text.unwrap_or("KonText error").to_string());
            }
        }
    }
    None
}

/// KonText token lists → tokens: skip structures (`strc`), read attributes from
/// `posattrs` (new) or the `attr` items that follow a token (old, `/lemma/tag`).
fn kontext_tokens(items: Option<&Value>, other: &[Layer]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    for it in items.and_then(Value::as_array).into_iter().flatten() {
        let class = it.get("class").and_then(Value::as_str).unwrap_or("");
        let s = it.get("str").and_then(Value::as_str).unwrap_or("");
        if class.split_whitespace().any(|c| c == "strc") {
            continue;
        }
        if class.split_whitespace().any(|c| c == "attr") {
            if let Some(last) = out.last_mut() {
                let vals: Vec<&str> = s.trim().trim_start_matches('/').split('/').collect();
                last.layers = other.iter().zip(vals).map(|(l, v)| (*l, v.to_string())).collect();
            }
            continue;
        }
        let posattrs: Vec<String> = it
            .get("posattrs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .or_else(|| it.get("mouseover").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str)
                .map(|m| m.split('/').map(str::to_string).collect()))
            .unwrap_or_default();
        let words: Vec<&str> = s.split_whitespace().collect();
        let n = words.len();
        for (i, w) in words.into_iter().enumerate() {
            let mut t = Token::text(w);
            if i + 1 == n && !posattrs.is_empty() {
                t.layers = other.iter().zip(posattrs.iter()).map(|(l, v)| (*l, v.clone())).collect();
            }
            out.push(t);
        }
    }
    out
}

fn kontext_lines(v: &Value, other: &[Layer]) -> Vec<Hit> {
    let mut hits = Vec::new();
    for line in v.get("Lines").and_then(Value::as_array).into_iter().flatten() {
        let kwic = kontext_tokens(line.get("Kwic"), other);
        let start = line.get("toknum").and_then(Value::as_u64);
        let end = start.map(|s| s + (kwic.len().max(1) as u64) - 1);
        let doc = match line.get("ref") {
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).next().map(str::to_string),
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        }
        .map(|s| s.trim_start_matches('#').to_string())
        .filter(|s| !s.is_empty());
        hits.push(Hit { left: kontext_tokens(line.get("Left"), &[]), kwic, right: kontext_tokens(line.get("Right"), &[]), start, end, doc, tokid: None });
    }
    hits
}

impl Engine for KontextEngine {
    fn search(&self, a: &SearchArgs<'_>) -> Result<Page, EngineError> {
        let q = a.queries.first().ok_or_else(|| EngineError::System("no query".into()))?;
        let other: Vec<Layer> = a.layers.iter().skip(1).map(|(l, _)| *l).collect();
        // past the end: learn the size first (KonText / Manatee fail on huge page numbers)
        if a.offset > 0 {
            let probe = self.page(q, 1, 1, a)?;
            let total = probe.get("concsize").and_then(Value::as_u64).unwrap_or(0);
            if a.offset >= total || a.limit == 0 {
                let exact = probe.get("finished").and_then(Value::as_bool).unwrap_or(true);
                return Ok(Page { total, exact, hits: vec![] });
            }
        }
        // KonText pages are aligned to the page size: read one or two pages
        let ps = a.limit.max(1);
        let first = a.offset / ps + 1;
        let skip = (a.offset % ps) as usize;
        let v = self.page(q, first, ps, a)?;
        let total = v.get("concsize").and_then(Value::as_u64).unwrap_or(0);
        let exact = v.get("finished").and_then(Value::as_bool).unwrap_or(true);
        let mut hits = kontext_lines(&v, &other);
        if skip > 0 && a.offset + a.limit < total && hits.len() == ps as usize {
            hits.extend(kontext_lines(&self.page(q, first + 1, ps, a)?, &other));
        }
        let hits = if a.limit == 0 { vec![] } else { hits.into_iter().skip(skip).take(a.limit as usize).collect() };
        Ok(Page { total, exact, hits })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cqp_output() {
        let out = "CQP version 3.5.0\n95\n-::-EOL-::-\n194910\t194910\twith your new\thouse\tand with his\thouse\tNOUN\n195685\t195686\tovernight in a\thome stay\tthat was\thome stay\tNOUN NOUN\n-::-EOL-::-\n";
        let p = parse_cqp_output(out, "", vec![Layer::Lemma, Layer::Pos], false, false).unwrap();
        assert_eq!(p.total, 95);
        assert_eq!(p.hits.len(), 2);
        assert_eq!(p.hits[1].kwic[1], Token { text: "stay".into(), layers: vec![(Layer::Lemma, "stay".into()), (Layer::Pos, "NOUN".into())] });
        assert_eq!(p.hits[0].left.len(), 3);
        assert!(matches!(parse_cqp_output("CQP version 3.5.0\nCQP Error:\n\tCQP Syntax Error: x\n", "", vec![], false, false), Err(EngineError::Query(_))));
    }

    #[test]
    fn kontext_old_and_new() {
        let old = json!({"concsize": 12, "finished": true, "Lines": [{
            "toknum": 100, "ref": ["#7"],
            "Left": [{"str": "in the old", "class": ""}, {"str": "<s>", "class": "strc"}],
            "Kwic": [{"str": "house", "class": "col0 coll"}, {"str": "/house/NOUN", "class": "attr"}],
            "Right": [{"str": "of", "class": ""}]}]});
        let h = kontext_lines(&old, &[Layer::Lemma, Layer::Pos]);
        assert_eq!(h[0].left.len(), 3);
        assert_eq!(h[0].kwic[0].layers, vec![(Layer::Lemma, "house".into()), (Layer::Pos, "NOUN".into())]);
        assert_eq!(h[0].doc.as_deref(), Some("7"));
        let new = json!({"concsize": 1, "Lines": [{"toknum": 5, "Kwic": [{"str": "house", "class": "", "posattrs": ["house", "NOUN"]}], "Left": [], "Right": []}]});
        let h = kontext_lines(&new, &[Layer::Lemma, Layer::Pos]);
        assert_eq!(h[0].kwic[0].layers[1], (Layer::Pos, "NOUN".into()));
        assert_eq!(kontext_error(&json!({"messages": [["error", "bad query"]]})).as_deref(), Some("bad query"));
    }

    #[test]
    fn pando_json() {
        let h = json!({"doc_id": "d1", "match_start": 10, "match_end": 11,
            "context": {"left": "a b", "match": "the house", "right": "c"},
            "tokens": [{"pos": 11, "form": "house", "lemma": "house", "upos": "NOUN"}]});
        let layers = vec![(Layer::Text, "form".to_string()), (Layer::Lemma, "lemma".into()), (Layer::Pos, "upos".into())];
        let hit = pando_hit(&h, "form", &layers, Some("upos"));
        assert_eq!(hit.tokid.as_deref(), Some("NOUN"));
        assert_eq!(hit.kwic.len(), 2);
        assert_eq!(hit.kwic[0].layers, vec![]);
        assert_eq!(hit.kwic[1].layers.len(), 2);
        assert_eq!(hit.doc.as_deref(), Some("d1"));
    }
}
