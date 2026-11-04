//! `/info` for CWB and Manatee corpora: what a registry file declares, in the shape Pando's `/info`
//! answers (`attributes`, `structures` with `attrs` / `region_attrs`, a flat `region_attrs` list), so
//! front-ends read one shape for every engine. Read from the registry file alone: no engine is loaded.
//! File paths (`PATH`, `HOME`, …) stay out: they are paths on this server.
//!
//! Two registry styles:
//! - CWB (`cwb-encode`): `ATTRIBUTE word` lines, and structure attributes flattened as
//!   `STRUCTURE text_id` next to `STRUCTURE text`; a comment at the end of such a line is its label
//!   (`ATTRIBUTE pos  # POS Tag`, as TEITOK's tt-cwb-encode writes them: CWB has no LABEL);
//! - Manatee: blocks, `ATTRIBUTE word { LABEL "Word form" }`, and `STRUCTURE doc { ATTRIBUTE title }`.

use serde_json::{json, Value};
use std::path::Path;

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Open,
    Close,
}

/// The registry's tokens: words, quoted strings, braces; `#` starts a comment outside quotes.
fn tokens(text: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '#' => {
                while let Some(c) = chars.next() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '{' => {
                chars.next();
                out.push(Tok::Open);
            }
            '}' => {
                chars.next();
                out.push(Tok::Close);
            }
            '"' => {
                chars.next();
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => {
                            if let Some(n) = chars.next() {
                                s.push(n);
                            }
                        }
                        '"' => break,
                        _ => s.push(c),
                    }
                }
                out.push(Tok::Str(s));
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            _ => {
                let mut w = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || c == '{' || c == '}' || c == '"' || c == '#' {
                        break;
                    }
                    w.push(c);
                    chars.next();
                }
                out.push(Tok::Word(w));
            }
        }
    }
    out
}

/// One `KEY name [{ … }]` item: its name, its LABEL, and the items inside its block.
#[derive(Debug, Default)]
struct Item {
    key: String,
    name: String,
    label: Option<String>,
    inner: Vec<Item>,
}

/// Items up to the closing brace (or the end): `KEY value` and `KEY name { … }`.
fn items(toks: &[Tok], i: &mut usize) -> Vec<Item> {
    let mut out = Vec::new();
    while *i < toks.len() {
        let key = match &toks[*i] {
            Tok::Close => {
                *i += 1;
                return out;
            }
            Tok::Word(w) => w.to_uppercase(),
            _ => {
                *i += 1;
                continue;
            }
        };
        *i += 1;
        let mut item = Item { key, ..Default::default() };
        if let Some(Tok::Word(v) | Tok::Str(v)) = toks.get(*i) {
            item.name = v.clone();
            *i += 1;
        }
        if toks.get(*i) == Some(&Tok::Open) {
            *i += 1;
            item.inner = items(toks, i);
            item.label = item.inner.iter().find(|x| x.key == "LABEL").map(|x| x.name.clone());
        }
        out.push(item);
    }
    out
}

fn named(name: &str, label: &Option<String>) -> Value {
    match label {
        Some(l) if !l.is_empty() => json!({ "name": name, "label": l }),
        _ => json!({ "name": name }),
    }
}

/// CWB labels: `ATTRIBUTE name  # Label` / `STRUCTURE name  # Label` lines (no block on the line).
fn line_comment_labels(text: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for line in text.lines() {
        let Some((code, comment)) = line.split_once('#') else { continue };
        if code.contains('{') || code.contains('"') {
            continue;
        }
        let mut w = code.split_whitespace();
        if let (Some(k), Some(name), None) = (w.next(), w.next(), w.next()) {
            let label = comment.trim();
            if (k == "ATTRIBUTE" || k == "STRUCTURE") && !label.is_empty() {
                out.insert(format!("{k} {name}"), label.to_string());
            }
        }
    }
    out
}

/// The `/info` result for a registry's text. `size` is given by the caller (the catalogue knows it).
pub fn info_from_registry_text(text: &str, size: Option<i64>, engine: &str) -> Value {
    let toks = tokens(text);
    let mut i = 0;
    let mut top = items(&toks, &mut i);
    let comments = line_comment_labels(text);
    for it in top.iter_mut().filter(|x| x.label.is_none()) {
        if let Some(l) = comments.get(&format!("{} {}", it.key, it.name)) {
            it.label = Some(l.clone());
        }
    }
    let corpus_name = top.iter().find(|x| x.key == "NAME").map(|x| x.name.clone()).unwrap_or_default();
    let attributes: Vec<Value> = top.iter().filter(|x| x.key == "ATTRIBUTE").map(|x| named(&x.name, &x.label)).collect();
    let structs: Vec<&Item> = top.iter().filter(|x| x.key == "STRUCTURE").collect();
    // CWB: `text_id` is an attribute of `text` when `text` is a structure of its own
    let is_flat_attr = |n: &str| {
        structs.iter().any(|s| n.len() > s.name.len() + 1 && n.starts_with(&s.name) && n.as_bytes()[s.name.len()] == b'_')
    };
    let mut structures = Vec::new();
    let mut flat = Vec::new();
    for s in structs.iter().filter(|s| !is_flat_attr(&s.name)) {
        let mut ras: Vec<(String, Option<String>)> =
            s.inner.iter().filter(|x| x.key == "ATTRIBUTE").map(|x| (x.name.clone(), x.label.clone())).collect();
        let prefix = format!("{}_", s.name);
        for f in structs.iter().filter(|f| f.name.starts_with(&prefix)) {
            // the longest owning structure takes it (np_head under np, not under n)
            let owner = structs.iter().filter(|o| f.name.starts_with(&format!("{}_", o.name))).map(|o| o.name.len()).max();
            if owner == Some(s.name.len()) {
                ras.push((f.name[prefix.len()..].to_string(), f.label.clone()));
            }
        }
        flat.extend(ras.iter().map(|(n, _)| Value::String(format!("{}_{}", s.name, n))));
        structures.push(json!({
            "name": s.name,
            "label": s.label,
            "attrs": ras.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            "region_attrs": ras.iter().map(|(n, l)| named(n, l)).collect::<Vec<_>>(),
        }));
        if s.label.is_none() {
            structures.last_mut().unwrap().as_object_mut().unwrap().remove("label");
        }
    }
    json!({
        "name": corpus_name,
        "size": size,
        "tokens": size,
        "attributes": attributes,
        "structures": structures,
        "region_attrs": flat,
        "engine": engine,
        "source": "registry",
    })
}

/// The `/info` result for a registry file; an error when it can't be read.
pub fn info_from_registry_file(file: &Path, size: Option<i64>, engine: &str) -> Result<Value, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("registry of this corpus can't be read ({e})"))?;
    Ok(info_from_registry_text(&text, size, engine))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwb_flat_structure_attributes() {
        let reg = "NAME \"Test corpus\"\nID test\nPATH /data/test\n##:: charset = \"utf8\"\n\
                   ATTRIBUTE word\nATTRIBUTE lemma\nSTRUCTURE s\nSTRUCTURE text\nSTRUCTURE text_id\nSTRUCTURE text_title\n";
        let v = info_from_registry_text(reg, Some(1000), "cqp");
        assert_eq!(v["name"], "Test corpus");
        assert_eq!(v["size"], 1000);
        assert_eq!(v["attributes"], json!([{"name": "word"}, {"name": "lemma"}]));
        let st = v["structures"].as_array().unwrap();
        assert_eq!(st.len(), 2, "text_id and text_title are attributes, not structures");
        assert_eq!(st[1]["name"], "text");
        assert_eq!(st[1]["attrs"], json!(["id", "title"]));
        assert_eq!(v["region_attrs"], json!(["text_id", "text_title"]));
        assert!(v.to_string().find("/data/test").is_none(), "no server paths");
    }

    #[test]
    fn manatee_blocks_and_labels() {
        let reg = r#"
NAME "SiR 1.0"
PATH "/opt/manatee/sir"   # the data
ATTRIBUTE word {
    LABEL "Word form"
}
ATTRIBUTE lc {
    LABEL "word (lowercase)"
    DYNAMIC "utf8lowercase"
    FROMATTR word
}
STRUCTURE doc {
    LABEL "Document"
    ATTRIBUTE id
    ATTRIBUTE title {
        LABEL "Title"
    }
}
STRUCTURE s
"#;
        let v = info_from_registry_text(reg, None, "manatee");
        assert_eq!(v["attributes"][0], json!({"name": "word", "label": "Word form"}));
        assert_eq!(v["attributes"][1]["name"], "lc");
        let doc = &v["structures"][0];
        assert_eq!(doc["label"], "Document");
        assert_eq!(doc["region_attrs"], json!([{"name": "id"}, {"name": "title", "label": "Title"}]));
        assert_eq!(v["structures"][1], json!({"name": "s", "attrs": [], "region_attrs": []}));
        assert_eq!(v["region_attrs"], json!(["doc_id", "doc_title"]));
        assert!(v["size"].is_null());
    }

    #[test]
    fn cwb_labels_from_line_comments() {
        let reg = "## Registry file for the corpus tt-ntrex\nNAME \"NTREX 128\"\nATTRIBUTE pos  # POS Tag\n\
                   ATTRIBUTE word  # \nSTRUCTURE text\nSTRUCTURE text_title  # Title\n";
        let v = info_from_registry_text(reg, None, "cqp");
        assert_eq!(v["attributes"], json!([{"name": "pos", "label": "POS Tag"}, {"name": "word"}]));
        assert_eq!(v["structures"][0]["region_attrs"], json!([{"name": "title", "label": "Title"}]));
    }

    #[test]
    fn nested_owner_is_the_longest() {
        let reg = "STRUCTURE n\nSTRUCTURE np\nSTRUCTURE np_head\n";
        let v = info_from_registry_text(reg, None, "cqp");
        let st = v["structures"].as_array().unwrap();
        assert_eq!(st[0]["attrs"], json!([]), "n gets nothing");
        assert_eq!(st[1]["attrs"], json!(["head"]));
    }
}
