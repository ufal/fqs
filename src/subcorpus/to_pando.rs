//! A stored recipe as a pando subcorpus definition (pando-CQL; pando/wiki/PANDO-CQL-Reference.md,
//! "Subcorpora"), sent per request as `"subcorpus": {"definition", "mode", "name"}` and resolved at
//! creation by pando's `POST /subcorpus {"definition"}`.
//!
//! - regions → `r:<text> :: r.text_genre = "fiction" & r.text_year != "1990"` (pando needs the label
//!   on a lone region); several values → one full-match pattern `"fiction|poetry"`;
//! - documents → the same on the document id attribute;
//! - query → the query as written (another language: its language word first, `MANATEE …`, `CWB …`,
//!   which pando reads as the dialect; the query itself is not changed);
//! - expression → each name's definition in parentheses, joined by `&` `|` `-` with the
//!   expression's own grouping (pando: `&` binds tighter, `|` `-` left to right, as FQS parses it).
//!
//! Values are literal: a value with pattern characters is escaped, so pando's full-match pattern
//! matches exactly that value.

use super::definition::{Condition, Definition, Expr};
use anyhow::{bail, Result};

/// The default document structure and id attribute on pando corpora.
pub const DOC_STRUCT: &str = "text";
pub const DOC_ATTR: &str = "id";
/// Ids per pattern; longer lists become a union of several.
const IDS_PER_PATTERN: usize = 500;
/// Expressions nest at most this deep (a name defined through itself is refused earlier).
pub const MAX_DEPTH: usize = 16;

/// A pattern in double quotes. Only `"` is escaped: pando takes a quoted pattern's raw text as the
/// pattern, so the pattern's own escapes (`\.`, `\\`) must stay single.
fn quote(pattern: &str) -> String {
    format!("\"{}\"", pattern.replace('"', "\\\""))
}

/// A literal value as a pattern that matches only it.
pub fn literal_pattern(v: &str) -> String {
    let mut out = String::new();
    for c in v.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn values_pattern(values: &[String]) -> String {
    values.iter().map(|v| literal_pattern(v)).collect::<Vec<_>>().join("|")
}

fn region_filter(structure: &str, conds: &[(String, &str, Vec<String>)]) -> String {
    let mut s = format!("r:<{structure}>");
    for (i, (attr, op, values)) in conds.iter().enumerate() {
        s.push_str(if i == 0 { " :: " } else { " & " });
        s.push_str(&format!("r.{structure}_{attr} {op} {}", quote(&values_pattern(values))));
    }
    s
}

fn regions(structure: &str, conditions: &[Condition]) -> Result<String> {
    let conds = conditions
        .iter()
        .map(|c| Ok((c.attr.clone(), c.op.as_str(), c.values()?)))
        .collect::<Result<Vec<_>>>()?;
    Ok(region_filter(structure, &conds))
}

fn documents(ids: &[String], structure: &str, attr: &str) -> String {
    let parts: Vec<String> = ids
        .chunks(IDS_PER_PATTERN)
        .map(|chunk| region_filter(structure, &[(attr.to_string(), "=", chunk.to_vec())]))
        .collect();
    if parts.len() == 1 {
        parts.into_iter().next().unwrap_or_default()
    } else {
        parts.iter().map(|p| format!("({p})")).collect::<Vec<_>>().join(" | ")
    }
}

/// The language word pando reads before a query in another language.
fn language_word(language: Option<&str>) -> Result<&'static str> {
    Ok(match language.map(str::trim).unwrap_or("") {
        "" | "pando-cql" | "pando" => "",
        "manatee-cql" | "manatee" => "MANATEE ",
        "cwb-cql" | "cqp" | "cwb" => "CWB ",
        other => bail!("a pando subcorpus takes a token query in pando-cql, manatee-cql or cwb-cql, not '{other}'"),
    })
}

/// The pando definition of `def`; `lookup` gives the definition of another subcorpus of the same
/// corpus by name (for expressions).
pub fn definition(def: &Definition, lookup: &mut dyn FnMut(&str) -> Result<Definition>) -> Result<String> {
    definition_at(def, lookup, 0)
}

fn definition_at(def: &Definition, lookup: &mut dyn FnMut(&str) -> Result<Definition>, depth: usize) -> Result<String> {
    if depth > MAX_DEPTH {
        bail!("subcorpus expressions nest more than {MAX_DEPTH} deep (a name defined through itself?)");
    }
    Ok(match def {
        Definition::Regions { structure, conditions } => regions(structure, conditions)?,
        Definition::Documents { ids, structure, attr } => {
            documents(ids, structure.as_deref().unwrap_or(DOC_STRUCT), attr.as_deref().unwrap_or(DOC_ATTR))
        }
        Definition::Query { query, language } => format!("{}{}", language_word(language.as_deref())?, query),
        Definition::Expression { expression } => {
            let e = Expr::parse(expression)?;
            let mut leaf = |n: &str| -> Result<String> {
                let d = lookup(n)?;
                Ok(format!("({})", definition_at(&d, lookup, depth + 1)?))
            };
            e.eval(&mut leaf, &|op, a, b| format!("({a} {op} {b})"))?
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(v: serde_json::Value) -> Definition {
        Definition::parse(&v).unwrap()
    }

    fn none(_: &str) -> Result<Definition> {
        bail!("no names")
    }

    #[test]
    fn regions_and_documents() {
        let r = d(json!({"kind": "regions", "struct": "text", "conditions": [
            {"attr": "genre", "value": ["fiction", "poetry"]}, {"attr": "year", "op": "!=", "value": "1990"}]}));
        assert_eq!(definition(&r, &mut none).unwrap(), r#"r:<text> :: r.text_genre = "fiction|poetry" & r.text_year != "1990""#);
        let all = d(json!({"kind": "regions", "struct": "doc"}));
        assert_eq!(definition(&all, &mut none).unwrap(), "r:<doc>");
        let lit = d(json!({"kind": "regions", "struct": "text", "conditions": [{"attr": "title", "value": "A \"B\" (1.2)"}]}));
        assert_eq!(definition(&lit, &mut none).unwrap(), r#"r:<text> :: r.text_title = "A \"B\" \(1\.2\)""#);
        let docs = d(json!({"kind": "documents", "ids": ["d1", "d2"]}));
        assert_eq!(definition(&docs, &mut none).unwrap(), r#"r:<text> :: r.text_id = "d1|d2""#);
        let many: Vec<String> = (0..1200).map(|i| format!("d{i}")).collect();
        let docs = d(json!({"kind": "documents", "ids": many, "struct": "doc", "attr": "n"}));
        let t = definition(&docs, &mut none).unwrap();
        assert_eq!(t.matches("r:<doc>").count(), 3, "chunks of {IDS_PER_PATTERN}");
        assert!(t.starts_with("(r:<doc> :: r.doc_n = \"d0|") && t.contains(") | ("));
    }

    #[test]
    fn queries_are_sent_as_written() {
        let q = d(json!({"kind": "query", "query": "[upos=\"NOUN\"]"}));
        assert_eq!(definition(&q, &mut none).unwrap(), "[upos=\"NOUN\"]");
        let m = d(json!({"kind": "query", "query": "[tag=\"N.*\"]", "language": "manatee-cql"}));
        assert_eq!(definition(&m, &mut none).unwrap(), "MANATEE [tag=\"N.*\"]");
        let p = d(json!({"kind": "query", "query": "a-node []", "language": "pmltq"}));
        assert!(definition(&p, &mut none).is_err());
    }

    #[test]
    fn expressions_expand_names() {
        let mut lookup = |n: &str| -> Result<Definition> {
            Ok(match n {
                "fiction" => d(json!({"kind": "regions", "struct": "text", "conditions": [{"attr": "genre", "value": "fiction"}]})),
                "nouns" => d(json!({"kind": "query", "query": "[upos=\"NOUN\"]"})),
                "both" => d(json!({"kind": "expression", "expression": "fiction & nouns"})),
                "loop" => d(json!({"kind": "expression", "expression": "loop"})),
                _ => bail!("no subcorpus '{n}'"),
            })
        };
        let e = d(json!({"kind": "expression", "expression": "both - fiction"}));
        let f = r#"r:<text> :: r.text_genre = "fiction""#;
        let n = r#"[upos="NOUN"]"#;
        assert_eq!(definition(&e, &mut lookup).unwrap(), format!("(((({f}) & ({n}))) - ({f}))"));
        assert!(definition(&d(json!({"kind": "expression", "expression": "loop"})), &mut lookup).is_err());
        assert!(definition(&d(json!({"kind": "expression", "expression": "missing"})), &mut lookup).is_err());
    }
}
