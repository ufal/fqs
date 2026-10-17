//! Query languages: which ones a corpus accepts, by engine and annotation, and what `language` on
//! `/query` and `/run` becomes for the engine.
//!
//! - pando: `pando-cql` (its own), `cwb-cql`, and with dependency annotation `pmltq`, with
//!   constituency annotation (`node` regions) `tiger` — pando translates them (its `dialect`);
//! - CWB (`cqp`): `cwb-cql`; Manatee: `manatee-cql`.
//!
//! A corpus's settings can narrow the list (`settings.query_languages`: the ones to offer, only
//! those its engines accept). `/corpora` lists each corpus's languages, so front ends (KonText,
//! flexicorp, TEITOK, FCS) offer exactly those; a language a corpus does not accept is refused
//! with the list it does accept.
//!
//! A query can also name its language itself, as its first word (`PMLTQ a-node [ lemma = "be" ]`):
//! one of a small fixed list, in capitals, followed by white space. It is taken off the query and
//! replaces `language`, so a front end without a language choice (KonText's CQL field) can still
//! send PML-TQ or CWB CQL.

use crate::CorpusEntry;
use serde_json::Value;

/// A language name as front ends write it, as FQS's id ("" when unknown): aliases of pando's own
/// CQL, CWB's, PML-TQ, TIGER and Manatee's.
pub fn canonical(name: &str) -> &'static str {
    match name.trim().to_ascii_lowercase().as_str() {
        "pando-cql" | "pando" | "native" => "pando-cql",
        "cwb-cql" | "cwb" | "cqp" | "cqp-cql" => "cwb-cql",
        "pmltq" | "pml-tq" => "pmltq",
        "tiger" | "tigersearch" | "tiger-ql" => "tiger",
        "manatee-cql" | "manatee" | "sketch-cql" => "manatee-cql",
        _ => "",
    }
}

/// The words a query may start with to name its language (in capitals: no CQL query starts with
/// a bare word, and a pando program's names are not these).
const PREFIXES: &[(&str, &str)] = &[
    ("PANDO", "pando-cql"),
    ("CWB", "cwb-cql"),
    ("CQP", "cwb-cql"),
    ("PMLTQ", "pmltq"),
    ("PML-TQ", "pmltq"),
    ("TIGER", "tiger"),
    ("MANATEE", "manatee-cql"),
];

/// A query that starts with a language's name (`PMLTQ a-node [...]`): the language and the query
/// without it; None for any other query.
pub fn split_prefix(query: &str) -> Option<(&'static str, &str)> {
    let q = query.trim_start();
    let end = q.find(char::is_whitespace)?;
    let lang = PREFIXES.iter().find(|(w, _)| *w == &q[..end])?.1;
    let rest = q[end..].trim_start();
    (!rest.is_empty()).then_some((lang, rest))
}

/// An engine's default language (for "" / "auto").
pub fn default_for(backend: &str) -> &'static str {
    match backend {
        "pando" => "pando-cql",
        "cqp" => "cwb-cql",
        "manatee" => "manatee-cql",
        _ => "",
    }
}

fn has_feature(corpus: &CorpusEntry, feature: &str) -> bool {
    let label = format!("feature:{feature}");
    if corpus.labels.iter().any(|l| l == &label) {
        return true;
    }
    corpus
        .capabilities
        .pointer("/browse/features")
        .and_then(Value::as_array)
        .is_some_and(|fs| fs.iter().any(|f| f.as_str() == Some(feature)))
}

/// The languages an engine accepts for this corpus (before the corpus's own narrowing).
pub fn engine_languages(corpus: &CorpusEntry, backend: &str) -> Vec<&'static str> {
    match backend {
        "pando" => {
            let mut v = vec!["pando-cql", "cwb-cql"];
            if has_feature(corpus, "dependencies") || has_feature(corpus, "ud") {
                v.push("pmltq");
            }
            if has_feature(corpus, "constituency") || has_feature(corpus, "trees") {
                v.push("tiger");
            }
            v
        }
        "cqp" => vec!["cwb-cql"],
        "manatee" => vec!["manatee-cql"],
        _ => vec![],
    }
}

/// The corpus's own list (settings.query_languages), canonical; None when it gives none.
fn corpus_list(corpus: &CorpusEntry) -> Option<Vec<&'static str>> {
    let a = corpus.settings.get("query_languages")?.as_array()?;
    Some(a.iter().filter_map(Value::as_str).map(canonical).filter(|l| !l.is_empty()).collect())
}

/// The languages this corpus accepts with `backend`: the engine's, narrowed by the corpus's list.
pub fn accepted(corpus: &CorpusEntry, backend: &str) -> Vec<&'static str> {
    let engine = engine_languages(corpus, backend);
    match corpus_list(corpus) {
        Some(own) => engine.into_iter().filter(|l| own.contains(l)).collect(),
        None => engine,
    }
}

/// The engines a corpus can be queried with (settings.available_backends, else its preferred
/// backend; `auto`: pando and CWB).
pub fn corpus_backends(corpus: &CorpusEntry) -> Vec<String> {
    if let Some(a) = corpus.settings.get("available_backends").and_then(Value::as_array) {
        let v: Vec<String> = a.iter().filter_map(Value::as_str).map(str::to_string).collect();
        if !v.is_empty() {
            return v;
        }
    }
    match corpus.preferred_backend.as_str() {
        "auto" | "" => vec!["pando".into(), "cqp".into()],
        b => vec![b.to_string()],
    }
}

/// Every language the corpus accepts with any of its engines (for `/corpora`), the default first.
pub fn corpus_languages(corpus: &CorpusEntry) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for b in corpus_backends(corpus) {
        for l in accepted(corpus, &b) {
            if !out.contains(&l) {
                out.push(l);
            }
        }
    }
    out
}

/// What `language` becomes for `backend` on this corpus: its canonical id, or an error naming the
/// languages the corpus accepts there.
pub fn effective(corpus: &CorpusEntry, backend: &str, requested: &str) -> anyhow::Result<&'static str> {
    let t = requested.trim();
    let want = if t.is_empty() || t.eq_ignore_ascii_case("auto") { default_for(backend) } else { canonical(t) };
    let ok = accepted(corpus, backend);
    if !want.is_empty() && ok.contains(&want) {
        return Ok(want);
    }
    let shown = if t.is_empty() { "(empty)" } else { t };
    if ok.is_empty() {
        anyhow::bail!("Unknown backend for query language validation: {backend}");
    }
    anyhow::bail!(
        "Query language '{shown}' is not available for this corpus with backend '{backend}' (it accepts: {})",
        ok.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn corpus(backend: &str, labels: &[&str], settings: Value) -> CorpusEntry {
        let mut c: CorpusEntry = serde_json::from_value(json!({
            "id": "c", "label": "c", "project_root": "/tmp", "preferred_backend": backend, "settings": settings
        }))
        .expect("a corpus entry");
        c.labels = labels.iter().map(|s| s.to_string()).collect();
        c
    }

    #[test]
    fn languages_by_engine_and_annotation() {
        let ud = corpus("pando", &["feature:dependencies"], json!({}));
        assert_eq!(accepted(&ud, "pando"), vec!["pando-cql", "cwb-cql", "pmltq"]);
        let plain = corpus("pando", &[], json!({}));
        assert_eq!(accepted(&plain, "pando"), vec!["pando-cql", "cwb-cql"], "no trees: no PML-TQ");
        assert_eq!(accepted(&plain, "cqp"), vec!["cwb-cql"]);
        assert_eq!(accepted(&plain, "manatee"), vec!["manatee-cql"]);
        let narrowed = corpus("pando", &["feature:dependencies"], json!({ "query_languages": ["pando-cql", "pml-tq", "manatee-cql"] }));
        assert_eq!(accepted(&narrowed, "pando"), vec!["pando-cql", "pmltq"], "the corpus's list narrows, never widens");
    }

    #[test]
    fn effective_language() {
        let ud = corpus("pando", &["feature:ud"], json!({}));
        assert_eq!(effective(&ud, "pando", "").unwrap(), "pando-cql");
        assert_eq!(effective(&ud, "pando", "auto").unwrap(), "pando-cql");
        assert_eq!(effective(&ud, "pando", "PML-TQ").unwrap(), "pmltq");
        assert_eq!(effective(&ud, "pando", "cqp").unwrap(), "cwb-cql");
        let err = effective(&ud, "pando", "tiger").unwrap_err().to_string();
        assert!(err.contains("it accepts: pando-cql, cwb-cql, pmltq"), "{err}");
        assert!(effective(&ud, "cqp", "pmltq").is_err(), "CWB does not take PML-TQ");
        assert!(effective(&ud, "manatee", "pando-cql").is_err());
    }

    #[test]
    fn prefix() {
        assert_eq!(split_prefix("PMLTQ a-node $a [ lemma=\"the\" ]"), Some(("pmltq", "a-node $a [ lemma=\"the\" ]")));
        assert_eq!(split_prefix("  CWB\t[word=\"x\"]"), Some(("cwb-cql", "[word=\"x\"]")));
        assert_eq!(split_prefix("PML-TQ a-node []"), Some(("pmltq", "a-node []")));
        assert_eq!(split_prefix("[lemma=\"PMLTQ\"]"), None);
        assert_eq!(split_prefix("pmltq a-node []"), None, "capitals only");
        assert_eq!(split_prefix("PMLTQ"), None, "nothing after it");
        assert_eq!(split_prefix("CWB:[word=\"x\"]"), None, "a named token, not a prefix");
    }

    #[test]
    fn listing() {
        let auto = corpus("auto", &["feature:dependencies"], json!({}));
        assert_eq!(corpus_languages(&auto), vec!["pando-cql", "cwb-cql", "pmltq"]);
        let man = corpus("manatee", &[], json!({ "available_backends": ["manatee"] }));
        assert_eq!(corpus_languages(&man), vec!["manatee-cql"]);
    }
}
