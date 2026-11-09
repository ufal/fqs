//! A stored recipe as what libfqs_manatee writes a `.subc` from (fqsm_subc_create): a structure and
//! a structure-attribute condition, as KonText's subcorpus form makes them
//! (`text`, `genre="fiction|poetry" & year!="1990"`), or a token query in Manatee CQL. Documents are
//! the condition on the document id attribute (long lists: several parts, joined in FQS).
//! Expressions are evaluated in FQS over the parts' ranges (ranges.rs), not here.

use super::definition::Definition;
use super::to_pando::literal_pattern;
use anyhow::{bail, Result};

/// Ids per condition; longer lists are written in parts and joined.
const IDS_PER_PART: usize = 500;

/// One part to write: `structure` empty → `query` is a token query.
#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub structure: String,
    pub query: String,
}

fn quoted(values: &[String]) -> String {
    let pat = values.iter().map(|v| literal_pattern(v)).collect::<Vec<_>>().join("|");
    format!("\"{}\"", pat.replace('"', "\\\""))
}

/// The parts of a regions / documents / query definition (their union is the subcorpus).
/// `doc` is the corpus's document structure and attribute (its FULLREF), for documents without them.
pub fn parts(def: &Definition, doc: (&str, &str)) -> Result<Vec<Part>> {
    Ok(match def {
        Definition::Regions { structure, conditions } if conditions.is_empty() => {
            vec![Part { structure: String::new(), query: format!("<{structure}/>") }]
        }
        Definition::Regions { structure, conditions } => {
            let mut q = Vec::new();
            for c in conditions {
                q.push(format!("{}{}{}", c.attr, c.op, quoted(&c.values()?)));
            }
            vec![Part { structure: structure.clone(), query: q.join(" & ") }]
        }
        Definition::Documents { ids, structure, attr } => {
            let s = structure.as_deref().unwrap_or(doc.0);
            let a = attr.as_deref().unwrap_or(doc.1);
            ids.chunks(IDS_PER_PART).map(|c| Part { structure: s.to_string(), query: format!("{a}={}", quoted(c)) }).collect()
        }
        Definition::Query { query, language } => {
            match language.as_deref().map(str::trim).unwrap_or("") {
                "" | "manatee-cql" | "manatee" => {}
                other => bail!("a Manatee subcorpus takes a token query in manatee-cql, not '{other}'"),
            }
            vec![Part { structure: String::new(), query: query.clone() }]
        }
        Definition::Expression { .. } => bail!("an expression is evaluated over its parts, not written as one"),
    })
}

/// The document structure and attribute of a Manatee corpus: its FULLREF's first `struct.attr`
/// ("doc.id"), else `doc` / `id`.
pub fn doc_attr(fullref: Option<&str>) -> (String, String) {
    fullref
        .and_then(|f| f.split_once('.'))
        .map(|(s, a)| (s.to_string(), a.to_string()))
        .unwrap_or_else(|| ("doc".to_string(), "id".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(v: serde_json::Value) -> Definition {
        Definition::parse(&v).unwrap()
    }

    #[test]
    fn as_kontext_writes_them() {
        let r = d(json!({"kind": "regions", "struct": "doc", "conditions": [
            {"attr": "genre", "value": ["fiction", "poetry"]}, {"attr": "year", "op": "!=", "value": "1990"}]}));
        assert_eq!(parts(&r, ("doc", "id")).unwrap(), vec![Part { structure: "doc".into(), query: r#"genre="fiction|poetry" & year!="1990""#.into() }]);
        let all = d(json!({"kind": "regions", "struct": "doc"}));
        assert_eq!(parts(&all, ("doc", "id")).unwrap()[0].query, "<doc/>");
        let lit = d(json!({"kind": "regions", "struct": "doc", "conditions": [{"attr": "title", "value": "a \"b\" 1.2"}]}));
        assert_eq!(parts(&lit, ("doc", "id")).unwrap()[0].query, r#"title="a \"b\" 1\.2""#);
        let docs = d(json!({"kind": "documents", "ids": ["d1", "d2"]}));
        assert_eq!(parts(&docs, ("text", "n")).unwrap(), vec![Part { structure: "text".into(), query: r#"n="d1|d2""#.into() }]);
        let many: Vec<String> = (0..1001).map(|i| format!("d{i}")).collect();
        assert_eq!(parts(&d(json!({"kind": "documents", "ids": many})), ("doc", "id")).unwrap().len(), 3);
        let q = d(json!({"kind": "query", "query": "[tag=\"N.*\"]"}));
        assert_eq!(parts(&q, ("doc", "id")).unwrap()[0], Part { structure: String::new(), query: "[tag=\"N.*\"]".into() });
        assert!(parts(&d(json!({"kind": "query", "query": "[]", "language": "pando-cql"})), ("doc", "id")).is_err());
        assert_eq!(doc_attr(Some("text.id")), ("text".to_string(), "id".to_string()));
        assert_eq!(doc_attr(None), ("doc".to_string(), "id".to_string()));
    }
}
