//! SRU 1.2 / 2.0 response XML with the CLARIN-FCS records, Endpoint
//! Description and diagnostics.

use super::config::Resource;
use super::engine::{Hit, Token};
use super::query::Layer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    V1_2,
    V2_0,
}

impl Version {
    fn ns(self) -> &'static str {
        match self {
            Version::V1_2 => "http://www.loc.gov/zing/srw/",
            Version::V2_0 => "http://docs.oasis-open.org/ns/search-ws/sruResponse",
        }
    }
    fn prefix(self) -> &'static str {
        match self {
            Version::V1_2 => "sru",
            Version::V2_0 => "sruResponse",
        }
    }
    fn diag_ns(self) -> &'static str {
        match self {
            Version::V1_2 => "http://www.loc.gov/zing/srw/diagnostic/",
            Version::V2_0 => "http://docs.oasis-open.org/ns/search-ws/diagnostic",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Version::V1_2 => "1.2",
            Version::V2_0 => "2.0",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub uri: String,
    pub details: Option<String>,
    pub message: String,
}

impl Diagnostic {
    pub fn sru(code: u32, details: Option<&str>, message: &str) -> Diagnostic {
        Diagnostic { uri: format!("info:srw/diagnostic/1/{code}"), details: details.map(str::to_string), message: message.into() }
    }
    pub fn fcs(code: u32, details: Option<&str>, message: &str) -> Diagnostic {
        Diagnostic { uri: format!("http://clarin.eu/fcs/diagnostic/{code}"), details: details.map(str::to_string), message: message.into() }
    }
}

/// What explain says about the endpoint itself.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub database: String,
    /// public URL of the endpoint (…/fcs), without a trailing '/'
    pub base_url: Option<String>,
    pub title: String,
    pub description: String,
    pub default_records: u64,
    pub max_records: u64,
}

impl Endpoint {
    pub fn layer_id(l: Layer) -> &'static str {
        match l {
            Layer::Text => "word",
            other => other.name(),
        }
    }
    pub fn layer_uri(&self, l: Layer) -> String {
        match &self.base_url {
            Some(b) => format!("{b}/layers/{}", Endpoint::layer_id(l)),
            None => format!("http://clarin.eu/fcs/layers/{}", Endpoint::layer_id(l)),
        }
    }
}

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            // characters XML 1.0 does not allow
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => o.push(c),
        }
    }
    o
}

const XML_DECL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";

fn diagnostics_xml(v: Version, diags: &[Diagnostic]) -> String {
    diagnostics_xml_in(v, v.prefix(), diags)
}

fn diagnostics_xml_in(v: Version, p: &str, diags: &[Diagnostic]) -> String {
    if diags.is_empty() {
        return String::new();
    }
    let mut o = format!("<{p}:diagnostics>\n");
    for d in diags {
        o.push_str(&format!("<diag:diagnostic xmlns:diag=\"{}\">\n<diag:uri>{}</diag:uri>\n", v.diag_ns(), esc(&d.uri)));
        if let Some(det) = &d.details {
            o.push_str(&format!("<diag:details>{}</diag:details>\n", esc(det)));
        }
        o.push_str(&format!("<diag:message>{}</diag:message>\n</diag:diagnostic>\n", esc(&d.message)));
    }
    o.push_str(&format!("</{p}:diagnostics>\n"));
    o
}

fn record_packing(v: Version) -> String {
    let p = v.prefix();
    match v {
        Version::V1_2 => format!("<{p}:recordPacking>xml</{p}:recordPacking>\n"),
        Version::V2_0 => format!("<{p}:recordXMLEscaping>xml</{p}:recordXMLEscaping>\n"),
    }
}

// ── explain ────────────────────────────────────────────────────────────

pub fn explain_response(v: Version, ep: &Endpoint, resources: &[Resource], with_ed: bool, diags: &[Diagnostic]) -> String {
    let p = v.prefix();
    let mut o = String::from(XML_DECL);
    o.push_str(&format!("<{p}:explainResponse xmlns:{p}=\"{}\">\n<{p}:version>{}</{p}:version>\n", v.ns(), v.as_str()));
    o.push_str(&format!("<{p}:record>\n<{p}:recordSchema>http://explain.z3950.org/dtd/2.0/</{p}:recordSchema>\n"));
    o.push_str(&record_packing(v));
    o.push_str(&format!("<{p}:recordData>\n"));
    o.push_str("<zr:explain xmlns:zr=\"http://explain.z3950.org/dtd/2.0/\">\n");
    o.push_str(&format!(
        "<zr:serverInfo protocol=\"SRU\" version=\"{}\" transport=\"http\">\n<zr:host>{}</zr:host>\n<zr:port>{}</zr:port>\n<zr:database>{}</zr:database>\n</zr:serverInfo>\n",
        v.as_str(), esc(&ep.host), ep.port, esc(&ep.database)
    ));
    o.push_str(&format!(
        "<zr:databaseInfo>\n<zr:title lang=\"en\" primary=\"true\">{}</zr:title>\n<zr:description lang=\"en\" primary=\"true\">{}</zr:description>\n</zr:databaseInfo>\n",
        esc(&ep.title), esc(&ep.description)
    ));
    o.push_str("<zr:schemaInfo>\n<zr:schema identifier=\"http://clarin.eu/fcs/resource\" name=\"fcs\">\n<zr:title lang=\"en\" primary=\"true\">CLARIN Federated Content Search</zr:title>\n</zr:schema>\n</zr:schemaInfo>\n");
    o.push_str(&format!(
        "<zr:configInfo>\n<zr:default type=\"numberOfRecords\">{}</zr:default>\n<zr:setting type=\"maximumRecords\">{}</zr:setting>\n</zr:configInfo>\n",
        ep.default_records, ep.max_records
    ));
    o.push_str("</zr:explain>\n");
    o.push_str(&format!("</{p}:recordData>\n</{p}:record>\n"));
    o.push_str(&diagnostics_xml(v, diags));
    if with_ed {
        o.push_str(&format!("<{p}:extraResponseData>\n"));
        o.push_str(&endpoint_description(v, ep, resources));
        o.push_str(&format!("</{p}:extraResponseData>\n"));
    }
    o.push_str(&format!("</{p}:explainResponse>\n"));
    o
}

pub fn endpoint_description(v: Version, ep: &Endpoint, resources: &[Resource]) -> String {
    let v2 = v == Version::V2_0;
    let adv = v2 && resources.iter().any(|r| r.advanced);
    let mut o = format!("<ed:EndpointDescription xmlns:ed=\"http://clarin.eu/fcs/endpoint-description\" version=\"{}\">\n", if v2 { 2 } else { 1 });
    o.push_str("<ed:Capabilities>\n<ed:Capability>http://clarin.eu/fcs/capability/basic-search</ed:Capability>\n");
    if adv {
        o.push_str("<ed:Capability>http://clarin.eu/fcs/capability/advanced-search</ed:Capability>\n");
    }
    o.push_str("</ed:Capabilities>\n<ed:SupportedDataViews>\n");
    o.push_str("<ed:SupportedDataView id=\"hits\" delivery-policy=\"send-by-default\">application/x-clarin-fcs-hits+xml</ed:SupportedDataView>\n");
    if adv {
        o.push_str("<ed:SupportedDataView id=\"adv\" delivery-policy=\"need-to-request\">application/x-clarin-fcs-adv+xml</ed:SupportedDataView>\n");
    }
    o.push_str("</ed:SupportedDataViews>\n");
    if adv {
        let mut used: Vec<Layer> = Vec::new();
        for r in resources.iter().filter(|r| r.advanced) {
            for l in r.layers() {
                if !used.contains(&l) {
                    used.push(l);
                }
            }
        }
        used.sort_by_key(|l| Layer::ALL.iter().position(|x| x == l));
        o.push_str("<ed:SupportedLayers>\n");
        for l in used {
            o.push_str(&format!(
                "<ed:SupportedLayer id=\"{}\" result-id=\"{}\">{}</ed:SupportedLayer>\n",
                Endpoint::layer_id(l), esc(&ep.layer_uri(l)), l.name()
            ));
        }
        o.push_str("</ed:SupportedLayers>\n");
    }
    o.push_str("<ed:Resources>\n");
    for r in resources {
        o.push_str(&format!("<ed:Resource pid=\"{}\">\n", esc(&r.pid)));
        for (lang, t) in &r.titles {
            o.push_str(&format!("<ed:Title xml:lang=\"{}\">{}</ed:Title>\n", esc(lang), esc(t)));
        }
        for (lang, t) in &r.descriptions {
            o.push_str(&format!("<ed:Description xml:lang=\"{}\">{}</ed:Description>\n", esc(lang), esc(t)));
        }
        if v2 {
            for (lang, t) in &r.institutions {
                o.push_str(&format!("<ed:Institution xml:lang=\"{}\">{}</ed:Institution>\n", esc(lang), esc(t)));
            }
        }
        if let Some(u) = &r.landing_page {
            o.push_str(&format!("<ed:LandingPageURI>{}</ed:LandingPageURI>\n", esc(u)));
        }
        o.push_str("<ed:Languages>\n");
        for l in &r.languages {
            o.push_str(&format!("<ed:Language>{}</ed:Language>\n", esc(l)));
        }
        o.push_str("</ed:Languages>\n");
        let views = if adv && r.advanced { "hits adv" } else { "hits" };
        o.push_str(&format!("<ed:AvailableDataViews ref=\"{views}\"/>\n"));
        if adv && r.advanced {
            let ids: Vec<&str> = r.layers().into_iter().map(Endpoint::layer_id).collect();
            o.push_str(&format!("<ed:AvailableLayers ref=\"{}\"/>\n", ids.join(" ")));
        }
        o.push_str("</ed:Resource>\n");
    }
    o.push_str("</ed:Resources>\n</ed:EndpointDescription>\n");
    o
}

// ── searchRetrieve ─────────────────────────────────────────────────────

pub struct Record<'a> {
    pub resource: &'a Resource,
    pub hit: Hit,
    pub hit_ref: Option<String>,
    pub adv: bool,
}

fn words(ts: &[Token]) -> String {
    ts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" ")
}

fn hits_view(h: &Hit) -> String {
    let mut o = String::from("<fcs:DataView type=\"application/x-clarin-fcs-hits+xml\">\n<hits:Result xmlns:hits=\"http://clarin.eu/fcs/dataview/hits\">");
    let l = words(&h.left);
    if !l.is_empty() {
        o.push_str(&esc(&l));
        o.push(' ');
    }
    o.push_str(&format!("<hits:Hit>{}</hits:Hit>", esc(&words(&h.kwic))));
    let r = words(&h.right);
    if !r.is_empty() {
        o.push(' ');
        o.push_str(&esc(&r));
    }
    o.push_str("</hits:Result>\n</fcs:DataView>\n");
    o
}

fn adv_view(h: &Hit, r: &Resource, ep: &Endpoint) -> String {
    let all: Vec<(&Token, bool)> = h
        .left
        .iter()
        .map(|t| (t, false))
        .chain(h.kwic.iter().map(|t| (t, true)))
        .chain(h.right.iter().map(|t| (t, false)))
        .collect();
    let mut o = String::from("<fcs:DataView type=\"application/x-clarin-fcs-adv+xml\">\n<adv:Advanced xmlns:adv=\"http://clarin.eu/fcs/dataview/advanced\" unit=\"item\">\n<adv:Segments>\n");
    // unit="item": 1-based, inclusive character offsets in the tokens joined by spaces
    let mut at = 1usize;
    for (i, (t, _)) in all.iter().enumerate() {
        let n = t.text.chars().count().max(1);
        o.push_str(&format!("<adv:Segment id=\"s{}\" start=\"{at}\" end=\"{}\"/>\n", i + 1, at + n - 1));
        at += n + 1;
    }
    o.push_str("</adv:Segments>\n<adv:Layers>\n");
    for l in r.layers() {
        o.push_str(&format!("<adv:Layer id=\"{}\">\n", esc(&ep.layer_uri(l))));
        for (i, (t, kw)) in all.iter().enumerate() {
            let value = if l == Layer::Text {
                Some(t.text.as_str())
            } else {
                t.layers.iter().find(|(x, _)| *x == l).map(|(_, v)| v.as_str())
            };
            // every layer has a span for every segment (clients need at least one per
            // layer and line layers up by segment): empty where the engine gave no value
            let value = value.unwrap_or("");
            let hl = if *kw { " highlight=\"h1\"" } else { "" };
            o.push_str(&format!("<adv:Span ref=\"s{}\"{hl}>{}</adv:Span>\n", i + 1, esc(value)));
        }
        o.push_str("</adv:Layer>\n");
    }
    o.push_str("</adv:Layers>\n</adv:Advanced>\n</fcs:DataView>\n");
    o
}

pub fn search_response(
    v: Version,
    ep: &Endpoint,
    total: u64,
    exact: bool,
    first_position: u64,
    records: &[Record<'_>],
    diags: &[Diagnostic],
) -> String {
    let p = v.prefix();
    let mut o = String::from(XML_DECL);
    o.push_str(&format!(
        "<{p}:searchRetrieveResponse xmlns:{p}=\"{}\">\n<{p}:version>{}</{p}:version>\n<{p}:numberOfRecords>{total}</{p}:numberOfRecords>\n",
        v.ns(), v.as_str()
    ));
    if !records.is_empty() {
        o.push_str(&format!("<{p}:records>\n"));
        for (i, rec) in records.iter().enumerate() {
            o.push_str(&format!("<{p}:record>\n<{p}:recordSchema>http://clarin.eu/fcs/resource</{p}:recordSchema>\n"));
            o.push_str(&record_packing(v));
            o.push_str(&format!("<{p}:recordData>\n"));
            let res_ref = rec.resource.landing_page.as_ref().map(|u| format!(" ref=\"{}\"", esc(u))).unwrap_or_default();
            o.push_str(&format!(
                "<fcs:Resource xmlns:fcs=\"http://clarin.eu/fcs/resource\" pid=\"{}\"{res_ref}>\n",
                esc(&rec.resource.pid)
            ));
            let frag_ref = rec.hit_ref.as_ref().map(|u| format!(" ref=\"{}\"", esc(u))).unwrap_or_default();
            o.push_str(&format!("<fcs:ResourceFragment{frag_ref}>\n"));
            o.push_str(&hits_view(&rec.hit));
            if rec.adv {
                o.push_str(&adv_view(&rec.hit, rec.resource, ep));
            }
            o.push_str("</fcs:ResourceFragment>\n</fcs:Resource>\n");
            o.push_str(&format!(
                "</{p}:recordData>\n<{p}:recordPosition>{}</{p}:recordPosition>\n</{p}:record>\n",
                first_position + i as u64
            ));
        }
        o.push_str(&format!("</{p}:records>\n"));
        let next = first_position + records.len() as u64;
        if next <= total {
            o.push_str(&format!("<{p}:nextRecordPosition>{next}</{p}:nextRecordPosition>\n"));
        }
    }
    o.push_str(&diagnostics_xml(v, diags));
    if v == Version::V2_0 {
        let prec = if exact { "exact" } else { "estimate" };
        o.push_str(&format!(
            "<{p}:resultCountPrecision>info:srw/vocabulary/resultCountPrecision/1/{prec}</{p}:resultCountPrecision>\n"
        ));
    }
    o.push_str(&format!("</{p}:searchRetrieveResponse>\n"));
    o
}

/// A response that carries only (fatal) diagnostics, for any operation.
pub fn diagnostic_response(v: Version, operation: &str, diags: &[Diagnostic]) -> String {
    let el = match operation {
        "searchRetrieve" => "searchRetrieveResponse",
        "scan" => "scanResponse",
        _ => "explainResponse",
    };
    // SRU 2.0 puts scan in its own namespace (1.2: the same one as the rest)
    let (p, ns) = if el == "scanResponse" && v == Version::V2_0 {
        ("scan", "http://docs.oasis-open.org/ns/search-ws/scan")
    } else {
        (v.prefix(), v.ns())
    };
    let mut o = String::from(XML_DECL);
    o.push_str(&format!("<{p}:{el} xmlns:{p}=\"{ns}\">\n<{p}:version>{}</{p}:version>\n", v.as_str()));
    if el == "searchRetrieveResponse" {
        o.push_str(&format!("<{p}:numberOfRecords>0</{p}:numberOfRecords>\n"));
    }
    o.push_str(&diagnostics_xml_in(v, p, diags));
    o.push_str(&format!("</{p}:{el}>\n"));
    o
}
