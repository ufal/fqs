//! CLARIN-FCS 2.0 / SRU 1.2 + 2.0 endpoint, independent of the rest of FQS.
//!
//! The module needs three things from its host (FQS, or any other server):
//! the list of `Resource`s (see `config`), an `Endpoint` description, and a
//! way to get an `Engine` for a resource (see `engine`). Queueing, access
//! control and CPU limits stay with the host: it decides which resources a
//! caller may see and wraps `search` in whatever admission it uses.
//!
//! ```text
//!   params ─ parse_request ─┬─ explain ───────────────────────────── XML
//!                           └─ prepare_search (parse + translate) ─ search(engines) ─ XML
//! ```

pub mod config;
pub mod engine;
pub mod lex;
pub mod query;
pub mod sru;
pub mod translate;

use std::collections::HashMap;

pub use config::{Resource, ResourceSpec};
pub use engine::{Engine, EngineError, SearchArgs};
pub use sru::{Diagnostic, Endpoint, Version};

use query::{Parsed, QueryError};
use translate::Native;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Explain,
    SearchRetrieve,
    Scan,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::Explain => "explain",
            Operation::SearchRetrieve => "searchRetrieve",
            Operation::Scan => "scan",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryType {
    /// Basic Search
    Cql,
    /// Advanced Search (FCS-QL)
    Fcs,
    /// Lexical Search (LexCQL, LexFCS 1.0)
    Lex,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub version: Version,
    pub operation: Operation,
    pub query: String,
    pub query_type: QueryType,
    /// 1-based
    pub start: u64,
    pub max: u64,
    /// `x-fcs-context`: PIDs (or ids) to search; empty = all
    pub context: Vec<String>,
    /// `x-fcs-dataviews`
    pub dataviews: Vec<String>,
    /// `x-fcs-endpoint-description=true`
    pub endpoint_description: bool,
    /// `x-fcs-lex-fields`: the field types the Lex view shows (empty: all)
    pub lex_fields: Vec<String>,
}

/// A request that cannot be served: answer with this (version, operation, diagnostic).
pub type Refusal = (Version, Operation, Diagnostic);

fn param<'a>(p: &'a HashMap<String, String>, k: &str) -> Option<&'a str> {
    p.get(k).map(|s| s.trim()).filter(|s| !s.is_empty())
}

pub fn parse_request(p: &HashMap<String, String>, ep: &Endpoint) -> Result<Request, Refusal> {
    // `operation` exists only in SRU 1.x: with it and no (or a bad) version, answer 1.2
    let legacy = param(p, "operation").is_some();
    let version = match param(p, "version") {
        Some("2.0") => Version::V2_0,
        Some("1.2") | Some("1.1") => Version::V1_2,
        None if legacy => Version::V1_2,
        None => Version::V2_0,
        Some(v) => {
            let answer = if legacy { Version::V1_2 } else { Version::V2_0 };
            return Err((answer, Operation::Explain, Diagnostic::sru(5, Some(answer.as_str()), &format!("Unsupported version {v}"))));
        }
    };
    let operation = match param(p, "operation") {
        Some(o) if o.eq_ignore_ascii_case("explain") => Operation::Explain,
        Some(o) if o.eq_ignore_ascii_case("searchRetrieve") => Operation::SearchRetrieve,
        Some(o) if o.eq_ignore_ascii_case("scan") => Operation::Scan,
        Some(o) => return Err((version, Operation::Explain, Diagnostic::sru(4, Some(o), "Unsupported operation"))),
        // SRU 2.0 has no `operation`: the parameters tell
        None if param(p, "query").is_some() => Operation::SearchRetrieve,
        None if param(p, "scanClause").is_some() => Operation::Scan,
        None => Operation::Explain,
    };
    let refuse = |d: Diagnostic| Err((version, operation, d));
    let num = |k: &str, default: u64| -> Result<u64, Diagnostic> {
        match param(p, k) {
            None => Ok(default),
            Some(s) => s.parse::<u64>().map_err(|_| Diagnostic::sru(6, Some(k), &format!("Unsupported parameter value for {k}"))),
        }
    };
    let start = match num("startRecord", 1) {
        Ok(0) => return refuse(Diagnostic::sru(6, Some("startRecord"), "startRecord must be 1 or more")),
        Ok(n) => n,
        Err(d) => return refuse(d),
    };
    let max = match num("maximumRecords", ep.default_records) {
        Ok(n) => n.min(ep.max_records),
        Err(d) => return refuse(d),
    };
    let query_type = match param(p, "queryType") {
        None | Some("cql") => QueryType::Cql,
        Some("fcs") if version == Version::V2_0 => QueryType::Fcs,
        Some("lex") => QueryType::Lex,
        Some(q) => return refuse(Diagnostic::sru(6, Some("queryType"), &format!("Unsupported query type '{q}'"))),
    };
    // records are only sent as XML (SRU 71: unsupported record packing)
    for k in ["recordXMLEscaping", "recordPacking"] {
        if let Some(v) = param(p, k) {
            if v != "xml" {
                return refuse(Diagnostic::sru(71, Some(v), "Unsupported record packing"));
            }
        }
    }
    if operation == Operation::Scan {
        // FCS 2.0 has no scan; still check its arguments first (SRU 6), as clients test that
        for k in ["maximumTerms", "responsePosition"] {
            if let Some(v) = param(p, k) {
                if v.parse::<u64>().is_err() {
                    return refuse(Diagnostic::sru(6, Some(k), &format!("Unsupported parameter value for {k}")));
                }
            }
        }
    }
    if let Some(s) = param(p, "recordSchema") {
        if s != "http://clarin.eu/fcs/resource" && s != "fcs" {
            return refuse(Diagnostic::sru(66, Some(s), "Unknown schema for retrieval"));
        }
    }
    let list = |k: &str| -> Vec<String> {
        param(p, k).map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()).unwrap_or_default()
    };
    let query = param(p, "query").unwrap_or("").to_string();
    if operation == Operation::SearchRetrieve && query.is_empty() {
        return refuse(Diagnostic::sru(7, Some("query"), "Mandatory parameter not supplied"));
    }
    Ok(Request {
        version,
        operation,
        query,
        query_type,
        start,
        max,
        context: list("x-fcs-context"),
        dataviews: list("x-fcs-dataviews"),
        endpoint_description: matches!(param(p, "x-fcs-endpoint-description"), Some("true") | Some("1")),
        lex_fields: list("x-fcs-lex-fields"),
    })
}

pub fn explain(req: &Request, ep: &Endpoint, resources: &[Resource]) -> String {
    sru::explain_response(req.version, ep, resources, req.endpoint_description, &[])
}

/// A refused explain still carries the explain record (SRU requires `record` in an
/// explainResponse; strict clients reject one with diagnostics only).
pub fn explain_refusal_xml(r: &Refusal, ep: &Endpoint, resources: &[Resource]) -> String {
    if r.1 == Operation::Explain {
        return sru::explain_response(r.0, ep, resources, false, std::slice::from_ref(&r.2));
    }
    refusal_xml(r)
}

pub fn refusal_xml(r: &Refusal) -> String {
    sru::diagnostic_response(r.0, r.1.as_str(), std::slice::from_ref(&r.2))
}

fn query_diag(e: &QueryError, qt: QueryType) -> Diagnostic {
    match (e, qt) {
        // FCS Core 2.0 §: 5 = FCS-QL syntax error, 6 = query too complex (unsupported)
        (QueryError::Syntax(m), QueryType::Fcs) => Diagnostic::fcs(5, Some(m), "General query syntax error"),
        (QueryError::Unsupported(m), QueryType::Fcs) => Diagnostic::fcs(6, Some(m), "Query too complex. Cannot perform Query"),
        (QueryError::Syntax(m), QueryType::Cql) => Diagnostic::sru(10, Some(m), "Query syntax error"),
        (QueryError::Unsupported(m), QueryType::Cql) => Diagnostic::sru(48, Some(m), "Query feature unsupported"),
        (QueryError::Syntax(m), QueryType::Lex) => Diagnostic::sru(10, Some(m), "Query syntax error"),
        (QueryError::Unsupported(m), QueryType::Lex) => Diagnostic::sru(48, Some(m), "Query feature unsupported"),
    }
}

/// An SRU diagnostic named by its code (what a lexicon store reports).
fn sru_diag(code: u32, details: &str) -> Diagnostic {
    let message = match code {
        10 => "Query syntax error",
        16 => "Unsupported index",
        19 => "Unsupported relation",
        20 => "Unsupported relation modifier",
        48 => "Query feature unsupported",
        _ => "General system error",
    };
    Diagnostic::sru(code, Some(details), message)
}

/// A resource with its query in the engine's language.
pub struct Prepared {
    pub resource: Resource,
    pub native: Native,
    /// the query in the hit link's language (`{cql}`)
    pub link_query: Option<String>,
}

pub struct Plan {
    pub parts: Vec<Prepared>,
    /// non-fatal diagnostics so far (resources left out, …)
    pub diagnostics: Vec<Diagnostic>,
}

/// Parse the query, pick the resources, translate: everything before an engine runs.
pub fn prepare_search(req: &Request, resources: &[Resource]) -> Result<Plan, Refusal> {
    let refusal = |d: Diagnostic| -> Refusal { (req.version, Operation::SearchRetrieve, d) };
    let mut diagnostics = Vec::new();
    let mut chosen: Vec<&Resource> = Vec::new();
    if req.context.is_empty() {
        chosen.extend(resources.iter());
    } else {
        for key in &req.context {
            match resources.iter().find(|r| r.matches(key)) {
                Some(r) if !chosen.iter().any(|c| c.id == r.id) => chosen.push(r),
                Some(_) => {}
                None => diagnostics.push(Diagnostic::fcs(1, Some(key), "Persistent identifier passed for restricting the search is invalid")),
            }
        }
        if chosen.is_empty() {
            return Err(refusal(diagnostics.remove(0)));
        }
    }
    // Lexical Search: the lexicons only. A lexicon also answers a Basic Search (a CQL term is a LexCQL
    // term: the lemma); the query goes to it unchanged, so it is only parsed for the corpora
    if req.query_type == QueryType::Lex {
        let before = chosen.len();
        chosen.retain(|r| r.lex.is_some());
        if chosen.is_empty() {
            return Err(refusal(Diagnostic::sru(48, Some("Lexical Search is not available for the selected resources"), "Query feature unsupported")));
        }
        if chosen.len() < before {
            diagnostics.push(Diagnostic::fcs(8, None, "Resources without Lexical Search were left out"));
        }
    }
    let lex_parts: Vec<Prepared> = chosen
        .iter()
        .filter(|r| r.lex.is_some())
        .map(|r| Prepared { resource: (*r).clone(), native: Native { queries: vec![req.query.clone()] }, link_query: None })
        .collect();
    chosen.retain(|r| r.lex.is_none());
    if chosen.is_empty() {
        return Ok(Plan { parts: lex_parts, diagnostics });
    }
    let parsed: Parsed = match match req.query_type {
        QueryType::Cql | QueryType::Lex => query::parse_cql(&req.query),
        QueryType::Fcs => query::parse_fcsql(&req.query),
    } {
        Ok(p) => p,
        // a query only the lexicons understand: search them, say why the corpora were left out
        Err(e) if !lex_parts.is_empty() => {
            let d = query_diag(&e, req.query_type);
            diagnostics.push(Diagnostic { details: Some(format!("corpora: {}", d.details.clone().unwrap_or_default())), ..d });
            return Ok(Plan { parts: lex_parts, diagnostics });
        }
        Err(e) => return Err(refusal(query_diag(&e, req.query_type))),
    };
    if req.query_type == QueryType::Fcs {
        let before = chosen.len();
        chosen.retain(|r| r.advanced);
        if chosen.is_empty() {
            return Err(refusal(Diagnostic::fcs(6, Some("Advanced Search is not available for the selected resources"), "Query too complex. Cannot perform Query")));
        }
        if chosen.len() < before {
            diagnostics.push(Diagnostic::fcs(8, None, "Resources without Advanced Search were left out"));
        }
    }
    let mut parts = Vec::new();
    let mut first_err = None;
    for r in chosen {
        match translate::translate(&parsed, r.dialect, &r.mapping) {
            Ok(native) => {
                let link_query = r.hit_link.as_ref().and_then(|l| {
                    if l.dialect == r.dialect && native.queries.len() == 1 {
                        return native.queries.first().cloned();
                    }
                    translate::translate(&parsed, l.dialect, &r.mapping).ok().and_then(|n| n.queries.into_iter().next())
                });
                parts.push(Prepared { resource: r.clone(), native, link_query })
            }
            Err(e) => {
                let d = query_diag(&e, req.query_type);
                if first_err.is_none() {
                    first_err = Some(d.clone());
                }
                diagnostics.push(Diagnostic { details: Some(format!("{}: {}", r.pid, d.details.clone().unwrap_or_default())), ..d });
            }
        }
    }
    parts.extend(lex_parts);
    if parts.is_empty() {
        return Err(refusal(first_err.unwrap_or_else(|| Diagnostic::sru(1, None, "No resource to search"))));
    }
    Ok(Plan { parts, diagnostics })
}

fn expand_link(t: &str, r: &Resource, h: &engine::Hit, rank: u64, link_query: Option<&str>) -> Option<String> {
    let n = |v: Option<u64>| v.map(|x| x.to_string());
    let enc = |v: Option<&str>| v.map(engine::url_encode);
    let vars: [(&str, Option<String>); 11] = [
        ("{n}", Some(rank.to_string())),
        ("{pos}", n(h.start)),
        ("{start}", n(h.start)),
        ("{end}", n(h.end)),
        ("{doc}", enc(h.doc.as_deref())),
        ("{tokid}", enc(h.tokid.as_deref())),
        ("{id}", enc(Some(&r.id))),
        ("{pid}", enc(Some(&r.pid))),
        ("{cql}", enc(link_query)),
        ("{query}", enc(link_query)),
        ("{corpus}", enc(Some(&r.id))),
    ];
    let mut out = t.to_string();
    for (k, v) in vars {
        if out.contains(k) {
            out = out.replace(k, &v?);
        }
    }
    // a placeholder nobody filled: better no link than a broken one
    (!out.contains('{')).then_some(out)
}

/// Run the plan (resources one after the other, records numbered across them).
pub fn search(req: &Request, ep: &Endpoint, plan: Plan, engine_for: &dyn Fn(&Resource) -> Result<Box<dyn Engine>, String>) -> String {
    let mut diags = plan.diagnostics;
    // the Advanced view on request, and always for an Advanced Search (FCS-QL)
    let want_adv = req.version == Version::V2_0
        && (req.dataviews.iter().any(|d| d == "adv") || req.query_type == QueryType::Fcs);
    let offset = req.start - 1;
    let mut cum = 0u64; // records in the resources before this one
    let mut exact = true;
    let mut records: Vec<(usize, engine::Hit, Option<String>, bool)> = Vec::new();
    let single = plan.parts.len() == 1;
    for (i, part) in plan.parts.iter().enumerate() {
        let r = &part.resource;
        let local_off = offset.saturating_sub(cum);
        let need = req.max.saturating_sub(records.len() as u64);
        let eng = match engine_for(r) {
            Ok(e) => e,
            Err(m) => {
                if single {
                    return sru::diagnostic_response(req.version, "searchRetrieve", &[Diagnostic::sru(1, Some(&m), "General system error")]);
                }
                diags.push(Diagnostic::sru(1, Some(&format!("{}: {m}", r.pid)), "General system error"));
                continue;
            }
        };
        let args = SearchArgs {
            queries: &part.native.queries,
            offset: local_off,
            limit: need,
            context: r.context,
            layers: &r.mapping.layers,
            doc_attr: r.doc_attr.as_deref(),
            tokid_attr: r.tokid_attr.as_deref(),
        };
        match eng.search(&args) {
            Ok(page) => {
                exact &= page.exact;
                cum += page.total;
                let adv = want_adv && r.advanced;
                if want_adv && !r.advanced {
                    diags.push(Diagnostic::fcs(4, Some("application/x-clarin-fcs-adv+xml"), "Requested Data View not valid for this resource"));
                }
                for (k, h) in page.hits.into_iter().enumerate() {
                    let rank = local_off + k as u64 + 1;
                    let link = r.hit_link.as_ref().and_then(|l| expand_link(&l.template, r, &h, rank, part.link_query.as_deref()));
                    records.push((i, h, link, adv));
                }
            }
            Err(e) => {
                let d = match &e {
                    EngineError::Query(m) => query_diag(&QueryError::Syntax(m.clone()), req.query_type),
                    EngineError::System(m) => Diagnostic::sru(1, Some(m), "General system error"),
                    EngineError::Diag(code, m) => sru_diag(*code, m),
                };
                if single {
                    return sru::diagnostic_response(req.version, "searchRetrieve", &[d]);
                }
                diags.push(Diagnostic { details: Some(format!("{}: {}", r.pid, d.details.clone().unwrap_or_default())), ..d });
            }
        }
    }
    if records.is_empty() && req.start > 1 && offset >= cum {
        diags.push(Diagnostic::sru(61, Some(&req.start.to_string()), "First record position out of range"));
    }
    let recs: Vec<sru::Record<'_>> = records
        .into_iter()
        .map(|(i, hit, hit_ref, adv)| sru::Record { resource: &plan.parts[i].resource, hit, hit_ref, adv })
        .collect();
    sru::search_response(req.version, ep, cum, exact, req.start, &recs, &diags, &req.lex_fields)
}

#[cfg(test)]
mod tests {
    use super::engine::{Hit, Page, Token};
    use super::query::Layer;
    use super::translate::Dialect;
    use super::*;
    use serde_json::json;

    fn ep() -> Endpoint {
        Endpoint {
            host: "h".into(), port: 80, database: "fcs".into(), base_url: Some("http://h/fcs".into()),
            title: "T".into(), description: "D".into(), default_records: 50, max_records: 1000,
        }
    }

    fn res(id: &str, d: Dialect) -> Resource {
        Resource::from_spec(&ResourceSpec {
            id, label: id, landing_page: Some("http://land"), fcs: &json!({"hit_link": "http://x/{id}?p={start}&q={query}"}),
            dialect: d, base_url: Some("http://h/fcs"), default_hit_link: None,
        })
    }

    struct Fake(u64);
    impl Engine for Fake {
        fn search(&self, a: &SearchArgs<'_>) -> Result<Page, EngineError> {
            let hits = (a.offset..self.0.min(a.offset + a.limit))
                .map(|i| Hit {
                    left: vec![Token { text: "a<".into(), layers: vec![] }],
                    kwic: vec![Token { text: format!("w{i}"), layers: vec![(Layer::Lemma, "l".into())] }],
                    right: vec![],
                    start: Some(i), end: Some(i), doc: None, tokid: None, lex: None,
                })
                .collect();
            Ok(Page { total: self.0, exact: true, hits })
        }
    }

    fn params(kv: &[(&str, &str)]) -> HashMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn explain_and_search() {
        let rs = vec![res("a", Dialect::Pando), res("b", Dialect::Cwb)];
        let req = parse_request(&params(&[("x-fcs-endpoint-description", "true")]), &ep()).unwrap();
        assert_eq!(req.operation, Operation::Explain);
        let x = explain(&req, &ep(), &rs);
        assert!(x.contains("<ed:Resource pid=\"http://h/fcs/resource/a\">"));
        assert!(x.contains("advanced-search"));
        assert!(x.contains("<ed:AvailableLayers ref=\"word lemma pos\"/>"));

        // across two resources of 3 records each: records 3..5
        let req = parse_request(&params(&[("query", "house"), ("startRecord", "3"), ("maximumRecords", "3"), ("x-fcs-dataviews", "adv")]), &ep()).unwrap();
        let plan = prepare_search(&req, &rs).unwrap();
        let x = search(&req, &ep(), plan, &|_| Ok(Box::new(Fake(3))));
        assert!(x.contains("<sruResponse:numberOfRecords>6</sruResponse:numberOfRecords>"), "{x}");
        assert_eq!(x.matches("<sruResponse:record>").count(), 3);
        assert!(x.contains("<sruResponse:recordPosition>5</sruResponse:recordPosition>"));
        assert!(x.contains("<sruResponse:nextRecordPosition>6</sruResponse:nextRecordPosition>"));
        assert!(x.contains("a&lt; <hits:Hit>w2</hits:Hit>"));
        assert!(x.contains("ref=\"http://x/a?p=2&amp;q=%5Bform%3D%22house%22%5D\""), "{x}");
        assert!(x.contains("<adv:Span ref=\"s2\" highlight=\"h1\">l</adv:Span>"));
        assert!(x.contains("<adv:Advanced xmlns:adv=\"http://clarin.eu/fcs/dataview/advanced\" unit=\"item\">"));
        assert!(x.contains("<adv:Segment id=\"s2\" start=\"4\" end=\"5\"/>"), "{x}");

        // context by id, 1.2
        let req = parse_request(&params(&[("operation", "searchRetrieve"), ("version", "1.2"), ("query", "x"), ("x-fcs-context", "b,zz")]), &ep()).unwrap();
        let plan = prepare_search(&req, &rs).unwrap();
        assert_eq!(plan.parts.len(), 1);
        assert_eq!(plan.diagnostics.len(), 1);
        let x = search(&req, &ep(), plan, &|_| Ok(Box::new(Fake(1))));
        assert!(x.contains("xmlns:sru=\"http://www.loc.gov/zing/srw/\""));
        assert!(x.contains("<sru:recordPacking>xml</sru:recordPacking>"));
        assert!(x.contains("http://clarin.eu/fcs/diagnostic/1"));

        // errors
        let req = parse_request(&params(&[("query", "[lemma="), ("queryType", "fcs")]), &ep()).unwrap();
        let e = prepare_search(&req, &rs).err().unwrap();
        assert_eq!(e.2.uri, "http://clarin.eu/fcs/diagnostic/5");
        assert!(refusal_xml(&e).contains("<sruResponse:numberOfRecords>0</sruResponse:numberOfRecords>"));
        assert!(parse_request(&params(&[("query", "x"), ("startRecord", "0")]), &ep()).is_err());
        assert!(parse_request(&params(&[("version", "3.0")]), &ep()).is_err());
        let e = parse_request(&params(&[("operation", "explain"), ("version", "9.9")]), &ep()).err().unwrap();
        let x = explain_refusal_xml(&e, &ep(), &rs);
        assert!(x.find("<sru:record>").unwrap() < x.find("<sru:diagnostics>").unwrap(), "{x}");
        // SRU 1.x style (operation, no version) → 1.2; bad version with operation → answered in 1.2
        assert_eq!(parse_request(&params(&[("operation", "explain")]), &ep()).unwrap().version, Version::V1_2);
        assert_eq!(parse_request(&params(&[]), &ep()).unwrap().version, Version::V2_0);
        assert_eq!(parse_request(&params(&[("operation", "explain"), ("version", "9.9")]), &ep()).err().unwrap().0, Version::V1_2);
        // endpoint-tester cases
        let e = parse_request(&params(&[("query", "x"), ("recordXMLEscaping", "invalid")]), &ep()).err().unwrap();
        assert_eq!(e.2.uri, "info:srw/diagnostic/1/71");
        let e = parse_request(&params(&[("scanClause", "fcs.resource=root"), ("maximumTerms", "invalid")]), &ep()).err().unwrap();
        assert_eq!((e.1, e.2.uri.as_str()), (Operation::Scan, "info:srw/diagnostic/1/6"));
        let req = parse_request(&params(&[("query", "nothing"), ("startRecord", "2147483647")]), &ep()).unwrap();
        let plan = prepare_search(&req, &rs).unwrap();
        let x = search(&req, &ep(), plan, &|_| Ok(Box::new(Fake(0))));
        assert!(x.contains("info:srw/diagnostic/1/61"), "{x}");
    }

    fn lexres(id: &str) -> Resource {
        Resource::from_spec(&ResourceSpec {
            id, label: id, landing_page: None, fcs: &json!({"store": "/x.db", "lex_fields": ["any", "lemma", "pos"]}),
            dialect: Dialect::Lex, base_url: Some("http://h/fcs"), default_hit_link: None,
        })
    }

    /// a lexicon engine: entries, and the query it was given
    struct FakeLex;
    impl Engine for FakeLex {
        fn search(&self, a: &SearchArgs<'_>) -> Result<Page, EngineError> {
            if a.queries[0].contains("foo") {
                return Err(EngineError::Diag(16, "unknown field 'foo'".into()));
            }
            let e = lex::LexEntry::from_json(&json!({"lang": "cs", "fields": [
                {"type": "lemma", "values": [{"text": a.queries[0].clone(), "attrs": {}}]},
                {"type": "pos", "values": [{"text": "NOUN", "attrs": {}}]}]}));
            Ok(Page { total: 1, exact: true, hits: vec![Hit { lex: Some(e), ..Hit::default() }] })
        }
    }

    #[test]
    fn lexical_search() {
        let rs = vec![res("a", Dialect::Pando), lexres("lx")];
        let x = explain(&parse_request(&params(&[("x-fcs-endpoint-description", "true")]), &ep()).unwrap(), &ep(), &rs);
        assert!(x.contains("http://clarin.eu/fcs/capability/lexical-search/1.0"));
        assert!(x.contains("<ed:SupportedDataView id=\"lex\" delivery-policy=\"send-by-default\">application/x-clarin-fcs-lex+xml"));
        assert!(x.contains("<ed:SupportedLexField id=\"lemma\">lemma</ed:SupportedLexField>"));
        assert!(x.contains("<ed:AvailableDataViews ref=\"hits lex\"/>\n<ed:AvailableLexFields ref=\"any lemma pos\"/>"), "{x}");

        // LexCQL goes to the lexicon as it is; the corpus is left out
        let req = parse_request(&params(&[("query", "lemma =/ignoreCase dům"), ("queryType", "lex"), ("x-fcs-lex-fields", "lemma")]), &ep()).unwrap();
        let plan = prepare_search(&req, &rs).unwrap();
        assert_eq!(plan.parts.len(), 1);
        assert_eq!(plan.parts[0].native.queries, vec!["lemma =/ignoreCase dům".to_string()]);
        assert!(plan.diagnostics.iter().any(|d| d.uri == "http://clarin.eu/fcs/diagnostic/8"));
        let x = search(&req, &ep(), plan, &|r| if r.lex.is_some() { Ok(Box::new(FakeLex)) } else { Ok(Box::new(Fake(1))) });
        assert!(x.contains("<hits:Hit kind=\"lex-lemma\">lemma =/ignoreCase dům</hits:Hit>"), "{x}");
        assert!(x.contains("<lex:Field type=\"lemma\">"));
        assert!(!x.contains("<lex:Field type=\"pos\">"), "x-fcs-lex-fields: {x}");

        // a Basic Search reaches both; a CQL query only the lexicon understands searches the lexicon
        let req = parse_request(&params(&[("query", "house")]), &ep()).unwrap();
        assert_eq!(prepare_search(&req, &rs).unwrap().parts.len(), 2);
        let req = parse_request(&params(&[("query", "lemma =/ignoreCase house")]), &ep()).unwrap();
        let plan = prepare_search(&req, &rs).unwrap();
        assert_eq!(plan.parts.len(), 1);
        assert!(plan.parts[0].resource.lex.is_some());

        // the store's error is the SRU diagnostic it names; no lexicon: Lexical Search is refused
        let req = parse_request(&params(&[("query", "foo = x"), ("queryType", "lex")]), &ep()).unwrap();
        let x = search(&req, &ep(), prepare_search(&req, &rs).unwrap(), &|_| Ok(Box::new(FakeLex)));
        assert!(x.contains("info:srw/diagnostic/1/16"), "{x}");
        let e = prepare_search(&req, &rs[..1]).err().unwrap();
        assert_eq!(e.2.uri, "info:srw/diagnostic/1/48");
    }
}
