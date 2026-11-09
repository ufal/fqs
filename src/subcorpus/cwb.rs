//! Stored subcorpora on CWB: CQP's own subcorpora. A recipe part is a CQP query whose result is
//! the selection (`<text> [] :: match.text_genre = "fiction" expand to text`, as CQP users write a
//! text-type subcorpus); its ranges come back with `dump`. The joined ranges are loaded with
//! `undump` and kept with `save` in a data directory of FQS's; a query on the subcorpus activates
//! it first (`set DataDirectory "…"; CORPUS:Name;`), so CQP searches only inside it. CQP's rule
//! (a hit lies inside one range) is the same as Manatee's, as adjacent ranges are merged.
//!
//! Next to CQP's saved file, the ranges are kept in FQS's own `.subc` form (ranges.rs) for
//! expressions: `<dir>/<Name>.subc`, and CQP's `<dir>/<CORPUS>:<Name>`.
//!
//! Only FQS's own CQP route; a TEITOK project's flexicorp route has no subcorpora.

use super::definition::Definition;
use super::ranges::{self, Range};
use super::to_pando::literal_pattern;
use crate::{refuse, CorpusEntry};
use anyhow::{anyhow, bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The default document structure and id attribute on CWB corpora (TEITOK's `text_id`).
pub const DOC_STRUCT: &str = "text";
pub const DOC_ATTR: &str = "id";
const IDS_PER_PART: usize = 500;

/// A pattern as a CQP string: in double quotes, or single quotes when it has a double quote.
fn quoted(pattern: &str) -> Result<String> {
    if !pattern.contains('"') {
        return Ok(format!("\"{pattern}\""));
    }
    if !pattern.contains('\'') {
        return Ok(format!("'{pattern}'"));
    }
    bail!("a value with both kinds of quotes cannot be written for CQP")
}

fn region_query(structure: &str, conds: &[(String, String, Vec<String>)]) -> Result<String> {
    let mut q = format!("<{structure}> []");
    for (i, (attr, op, values)) in conds.iter().enumerate() {
        let pat = values.iter().map(|v| literal_pattern(v)).collect::<Vec<_>>().join("|");
        q.push_str(if i == 0 { " :: " } else { " & " });
        q.push_str(&format!("match.{structure}_{attr} {op} {}", quoted(&pat)?));
    }
    q.push_str(&format!(" expand to {structure}"));
    Ok(q)
}

/// The CQP queries whose results together are the subcorpus (not for expressions).
pub fn parts(def: &Definition) -> Result<Vec<String>> {
    Ok(match def {
        Definition::Regions { structure, conditions } => {
            let conds = conditions.iter().map(|c| Ok((c.attr.clone(), c.op.clone(), c.values()?))).collect::<Result<Vec<_>>>()?;
            vec![region_query(structure, &conds)?]
        }
        Definition::Documents { ids, structure, attr } => {
            let s = structure.as_deref().unwrap_or(DOC_STRUCT);
            let a = attr.as_deref().unwrap_or(DOC_ATTR);
            ids.chunks(IDS_PER_PART).map(|c| region_query(s, &[(a.to_string(), "=".to_string(), c.to_vec())])).collect::<Result<_>>()?
        }
        Definition::Query { query, language } => {
            match language.as_deref().map(str::trim).unwrap_or("") {
                "" | "cwb-cql" | "cqp" | "cwb" => {}
                other => bail!("a CWB subcorpus takes a token query in cwb-cql, not '{other}'"),
            }
            crate::check_single_cqp_statement(query)?;
            vec![query.trim().to_string()]
        }
        Definition::Expression { .. } => bail!("an expression is evaluated over its parts, not written as one"),
    })
}

/// CQP's corpus id (upper case) of a catalogue corpus.
fn cqp_id(name: &str) -> String {
    name.to_uppercase()
}

fn quoted_path(p: &Path) -> Result<String> {
    let s = p.display().to_string();
    if s.contains('"') {
        refuse!(500, "the subcorpus folder {s} has a '\"' in its path");
    }
    Ok(format!("\"{s}\""))
}

/// Run a CQP script on the corpus; its output.
fn run(corpus: &CorpusEntry, script: &str) -> Result<String> {
    let (cmd, name, _bin, _reg) = crate::cqp_base_command(corpus);
    let (out, _) = crate::run_cqp_script_with_id_fallback(&cmd, &name, script)?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if let Some(err) = stdout.lines().chain(stderr.lines()).find(|l| l.contains("CQP Error") || l.contains("syntax error")) {
        let detail: Vec<&str> = stdout.lines().chain(stderr.lines()).skip_while(|l| *l != err).take(3).collect();
        refuse!(400, "cqp: {}", detail.join(" ").trim());
    }
    Ok(stdout)
}

/// The ranges selected by one CQP query (`dump`: match, matchend inclusive).
pub fn dump_ranges(corpus: &CorpusEntry, query: &str) -> Result<Vec<Range>> {
    let (_, name, _, _) = crate::cqp_base_command(corpus);
    let script = format!("set PrettyPrint off;\n{};\nFqsPart = {query};\ndump FqsPart;\n", cqp_id(&name));
    let out = run(corpus, &script)?;
    let mut v = Vec::new();
    for line in out.lines().map(crate::strip_cqp_prompts) {
        let mut cells = line.split('\t');
        if let (Some(b), Some(e)) = (cells.next().and_then(|c| c.trim().parse::<i64>().ok()), cells.next().and_then(|c| c.trim().parse::<i64>().ok())) {
            v.push((b, e + 1));
        }
    }
    Ok(ranges::normalize(v))
}

/// Keep `rs` as CQP subcorpus `<dir>/<file stem>`: undump + save, and the `.subc` beside it.
/// Answers the `.subc` path (what the catalogue stores).
pub fn save(corpus: &CorpusEntry, dir: &Path, stem: &str, rs: &[Range]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let subc = dir.join(format!("{stem}.subc"));
    ranges::write_subc(&subc, rs)?;
    let pairs = dir.join(format!("{stem}.pairs"));
    {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&pairs)?);
        for (b, e) in rs {
            writeln!(f, "{b}\t{}", e - 1)?;
        }
    }
    let (_, name, _, _) = crate::cqp_base_command(corpus);
    let script = format!(
        "set DataDirectory {};\n{};\nundump {stem} < {};\nsave {stem};\n",
        quoted_path(dir)?,
        cqp_id(&name),
        quoted_path(&pairs)?
    );
    let r = run(corpus, &script);
    let _ = std::fs::remove_file(&pairs);
    r?;
    let saved = dir.join(format!("{}:{stem}", cqp_id(&name)));
    if !saved.is_file() {
        return Err(anyhow!("cqp did not save the subcorpus ({} is missing)", saved.display()));
    }
    Ok(subc)
}

/// Is the CQP file of a subcorpus (`.subc` path as stored) there?
pub fn saved(corpus_name: &str, subc: &Path) -> bool {
    let stem = subc.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    subc.is_file() && subc.with_file_name(format!("{}:{stem}", cqp_id(corpus_name))).is_file()
}

/// Remove a stored subcorpus's files (`.subc` path as stored): the `.subc` and CQP's saved file.
pub fn remove(corpus_name: &str, subc: &Path) {
    let stem = subc.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let _ = std::fs::remove_file(subc.with_file_name(format!("{}:{stem}", cqp_id(corpus_name))));
    let _ = std::fs::remove_file(subc);
}

/// The script lines that make a query run inside the stored subcorpus (`.subc` path as stored).
pub fn activate(corpus_name: &str, subc: &Path) -> Result<String> {
    let dir = subc.parent().ok_or_else(|| anyhow!("bad subcorpus path {}", subc.display()))?;
    let stem = subc.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(format!("set DataDirectory {};\n{}:{stem};\n", quoted_path(dir)?, cqp_id(corpus_name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(v: serde_json::Value) -> Definition {
        Definition::parse(&v).unwrap()
    }

    #[test]
    fn as_cqp_users_write_them() {
        let r = d(json!({"kind": "regions", "struct": "doc", "conditions": [
            {"attr": "genre", "value": ["fiction", "poetry"]}, {"attr": "year", "op": "!=", "value": "1990"}]}));
        assert_eq!(parts(&r).unwrap(), vec![r#"<doc> [] :: match.doc_genre = "fiction|poetry" & match.doc_year != "1990" expand to doc"#]);
        assert_eq!(parts(&d(json!({"kind": "regions", "struct": "text"}))).unwrap(), vec!["<text> [] expand to text"]);
        let lit = d(json!({"kind": "regions", "struct": "doc", "conditions": [{"attr": "title", "value": "Budget (1.2) \"x\""}]}));
        assert_eq!(parts(&lit).unwrap(), vec![r#"<doc> [] :: match.doc_title = 'Budget \(1\.2\) "x"' expand to doc"#]);
        let both = d(json!({"kind": "regions", "struct": "doc", "conditions": [{"attr": "t", "value": "a'b\""}]}));
        assert!(parts(&both).is_err());
        let docs = d(json!({"kind": "documents", "ids": ["d1", "d2"]}));
        assert_eq!(parts(&docs).unwrap(), vec![r#"<text> [] :: match.text_id = "d1|d2" expand to text"#]);
        let q = d(json!({"kind": "query", "query": "[word=\"the\"]"}));
        assert_eq!(parts(&q).unwrap(), vec!["[word=\"the\"]"]);
        assert!(parts(&d(json!({"kind": "query", "query": "[word=\"the\"];"}))).is_err(), "as on /query: no ';'");
        assert!(parts(&d(json!({"kind": "query", "query": "[word=\"a\"]; cat A > \"x\""}))).is_err(), "one statement only");
        assert!(parts(&d(json!({"kind": "query", "query": "[]", "language": "pando-cql"}))).is_err());
    }

    #[test]
    fn activation() {
        let p = Path::new("/var/lib/fqs/subcorpora/c/cwb/Fqs_ab_1.subc");
        assert_eq!(activate("susanne", p).unwrap(), "set DataDirectory \"/var/lib/fqs/subcorpora/c/cwb\";\nSUSANNE:Fqs_ab_1;\n");
    }
}
