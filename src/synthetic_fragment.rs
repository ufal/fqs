//! Synthetic TEITOK-style fragments for Manatee and CWB hits, in the shape Pando gives with
//! `"fragment": true` (pando src/api/query_json.cpp, build_fragment): `<s>` around sentence parts,
//! `<tok id=… attr=…>form</tok>` separated by spaces, and a `highlight_map` with the matched ids. Front
//! ends then show every engine's hits the same way (token pop-ups, highlighting by token id).
//!
//! A token's id is its `id` attribute when the corpus has one (TEITOK's ids), else `w-<position + 1>`.

use serde_json::{json, Value};

/// One piece of a hit's context: a token, or a sentence boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    Tok { pos: i64, form: String, attrs: Vec<(String, String)> },
    SOpen,
    SClose,
}

fn esc(v: &str) -> String {
    let mut o = String::with_capacity(v.len() + 8);
    for c in v.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            _ => o.push(c),
        }
    }
    o
}

/// An attribute name usable in XML (as Pando's xml_name_ok).
pub fn xml_name_ok(n: &str) -> bool {
    let mut cs = n.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// A value that stands for "none" (left out of the fragment): empty, `_`, Manatee's `===NONE===`.
fn is_none(v: &str) -> bool {
    v.is_empty() || v == "_" || v == "===NONE==="
}

/// The token's id: its `id` attribute, else `w-<pos + 1>`.
pub fn tok_id(pos: i64, attrs: &[(String, String)]) -> String {
    attrs
        .iter()
        .find(|(k, v)| k == "id" && !is_none(v))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| format!("w-{}", pos + 1))
}

/// The fragment for `pieces`. Sentences open at a context's start or end are closed.
pub fn build(pieces: &[Piece]) -> String {
    let mut out = String::new();
    let mut open = false;
    let mut just_opened = false;
    for p in pieces {
        match p {
            Piece::SOpen => {
                if open {
                    out.push_str("</s>");
                }
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str("<s>");
                open = true;
                just_opened = true;
            }
            Piece::SClose => {
                if open {
                    out.push_str("</s>");
                    open = false;
                }
            }
            Piece::Tok { pos, form, attrs } => {
                if !out.is_empty() && !just_opened {
                    out.push(' ');
                }
                just_opened = false;
                out.push_str("<tok id=\"");
                out.push_str(&esc(&tok_id(*pos, attrs)));
                out.push('"');
                for (k, v) in attrs {
                    if k == "id" || k == "word" || k == "form" || is_none(v) || !xml_name_ok(k) {
                        continue;
                    }
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    out.push_str(&esc(v));
                    out.push('"');
                }
                out.push('>');
                out.push_str(&esc(form));
                out.push_str("</tok>");
            }
        }
    }
    if open {
        out.push_str("</s>");
    }
    out
}

/// The `highlight_map` of a hit (as Pando's): the ids of the matched tokens, one group for the match.
pub fn highlight_map(match_ids: &[String]) -> Value {
    json!({
        "default": { "tok_ids": match_ids },
        "match": match_ids,
        "groups": [{ "id": "t1", "name": "t1", "tok_ids": match_ids, "result_group": null }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(pos: i64, form: &str, attrs: &[(&str, &str)]) -> Piece {
        Piece::Tok { pos, form: form.into(), attrs: attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }

    #[test]
    fn tokens_sentences_and_escapes() {
        let f = build(&[
            tok(9, "AT&T", &[("word", "AT&T"), ("lemma", "AT&T"), ("pos", "NNP")]),
            Piece::SClose,
            Piece::SOpen,
            tok(10, "\"", &[("word", "\""), ("lemma", "_"), ("id", "w-11")]),
            tok(11, "x", &[("lc", "===NONE==="), ("bad name", "v")]),
        ]);
        assert_eq!(
            f,
            "<tok id=\"w-10\" lemma=\"AT&amp;T\" pos=\"NNP\">AT&amp;T</tok> <s><tok id=\"w-11\">&quot;</tok> <tok id=\"w-12\">x</tok></s>"
        );
    }

    #[test]
    fn highlight_map_shape() {
        let m = highlight_map(&["w-3".to_string()]);
        assert_eq!(m["match"], json!(["w-3"]));
        assert_eq!(m["groups"][0]["tok_ids"], json!(["w-3"]));
    }
}
