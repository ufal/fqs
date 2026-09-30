//! FCS query (see `query`) → engine query language.
//!
//! Three dialects, all "CQL" in the corpus-linguistics sense:
//!
//! * `Pando` — pando-CQL with `strict_quoted_strings` (a quoted value is a
//!   literal, `/…/` a substring regex, `%c` / `%d` only on literals). pando does
//!   not have alternation between sequences nor groups with a quantifier, so a
//!   query becomes a *list* of flat sequences whose union is the result.
//! * `Cwb` — CWB CQP: quoted values are (anchored) regexes, `%c` / `%d` flags,
//!   `( … ) | ( … )`, groups with quantifiers, `within s`.
//! * `Manatee` — Manatee / KonText / NoSketch Engine: like CWB, but no `%c` /
//!   `%d` (case-insensitivity as `(?i)`), and `within <s/>`.
//!
//! Negation is pushed down to the attribute tests first (`!(a & b)` → `a!=… |
//! b!=…`), so every engine only sees `=` / `!=`.

use super::query::{Cond, Elem, Flags, Layer, Parsed, Query, QueryError, UNBOUNDED, regex_escape};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Pando,
    Cwb,
    Manatee,
}

/// FCS layer → engine attribute name, FCS scope → engine structure name.
#[derive(Clone, Debug, Default)]
pub struct Mapping {
    pub layers: Vec<(Layer, String)>,
    /// `sentence`, `paragraph`, `text`, `utterance`, `turn`, `session` → structure
    pub structures: Vec<(String, String)>,
}

impl Mapping {
    pub fn attr(&self, l: Layer) -> Option<&str> {
        self.layers.iter().find(|(k, _)| *k == l).map(|(_, v)| v.as_str())
    }
    pub fn structure(&self, scope: &str) -> Option<&str> {
        let canon = canonical_scope(scope)?;
        self.structures.iter().find(|(k, _)| k == canon).map(|(_, v)| v.as_str())
    }
}

/// FCS-QL scope names (and their short forms) → the canonical scope.
pub fn canonical_scope(s: &str) -> Option<&'static str> {
    Some(match s.to_ascii_lowercase().as_str() {
        "s" | "sentence" => "sentence",
        "p" | "paragraph" => "paragraph",
        "u" | "utterance" => "utterance",
        "t" | "turn" => "turn",
        "text" => "text",
        "session" => "session",
        _ => return None,
    })
}

/// Upper bound on the number of flat sequences one query may expand into
/// (pando alternations, Basic Search AND orders).
pub const MAX_ALTERNATIVES: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub struct Native {
    /// One query (CWB, Manatee) or several whose union is the result (pando).
    pub queries: Vec<String>,
}

fn unsupported<T>(m: impl Into<String>) -> Result<T, QueryError> {
    Err(QueryError::Unsupported(m.into()))
}

pub fn translate(p: &Parsed, d: Dialect, m: &Mapping) -> Result<Native, QueryError> {
    let within = match &p.within {
        None => None,
        Some(w) => match m.structure(w) {
            Some(s) => Some(s.to_string()),
            None => return unsupported(format!("'within {w}' is not available for this resource")),
        },
    };
    let q = nnf_query(&p.query);
    match d {
        Dialect::Pando => {
            let alts = flatten(&q, m)?;
            let queries = alts
                .iter()
                .map(|seq| {
                    let body = seq_pando(seq, m)?;
                    Ok(match (&within, seq.within.as_deref()) {
                        (Some(w), _) => format!("{body} within {w}"),
                        (None, Some(w)) => format!("{body} within {w}"),
                        _ => body,
                    })
                })
                .collect::<Result<Vec<_>, QueryError>>()?;
            Ok(Native { queries })
        }
        Dialect::Cwb | Dialect::Manatee => {
            let (body, needs_sentence) = query_regex(&q, d, m)?;
            let scope = within.or_else(|| needs_sentence.then(|| m.structure("sentence").unwrap_or("s").to_string()));
            let text = match scope {
                None => body,
                Some(w) if d == Dialect::Cwb => format!("{body} within {w}"),
                Some(w) => format!("{body} within <{w}/>"),
            };
            Ok(Native { queries: vec![text] })
        }
    }
}

// ── negation normal form ───────────────────────────────────────────────

fn nnf_cond(c: &Cond, neg: bool) -> Cond {
    match c {
        Cond::Attr { layer, negated, value, flags } => Cond::Attr {
            layer: *layer,
            negated: *negated != neg,
            value: value.clone(),
            flags: flags.clone(),
        },
        Cond::Not(a) => nnf_cond(a, !neg),
        Cond::And(a, b) if !neg => Cond::And(Box::new(nnf_cond(a, false)), Box::new(nnf_cond(b, false))),
        Cond::Or(a, b) if !neg => Cond::Or(Box::new(nnf_cond(a, false)), Box::new(nnf_cond(b, false))),
        Cond::And(a, b) => Cond::Or(Box::new(nnf_cond(a, true)), Box::new(nnf_cond(b, true))),
        Cond::Or(a, b) => Cond::And(Box::new(nnf_cond(a, true)), Box::new(nnf_cond(b, true))),
    }
}

fn nnf_query(q: &Query) -> Query {
    match q {
        Query::Seq(es) => Query::Seq(
            es.iter()
                .map(|e| match e {
                    Elem::Token { cond, min, max } => Elem::Token { cond: cond.as_ref().map(|c| nnf_cond(c, false)), min: *min, max: *max },
                    Elem::Group { query, min, max } => Elem::Group { query: Box::new(nnf_query(query)), min: *min, max: *max },
                })
                .collect(),
        ),
        Query::Or(a, b) => Query::Or(Box::new(nnf_query(a)), Box::new(nnf_query(b))),
        Query::And(a, b) => Query::And(Box::new(nnf_query(a)), Box::new(nnf_query(b))),
    }
}

// ── attribute tests ────────────────────────────────────────────────────

fn attr_name<'a>(m: &'a Mapping, l: Layer) -> Result<&'a str, QueryError> {
    m.attr(l).ok_or_else(|| QueryError::Unsupported(format!("layer '{}' is not available for this resource", l.name())))
}

fn quote(v: &str) -> String {
    let mut o = String::with_capacity(v.len() + 2);
    o.push('"');
    for ch in v.chars() {
        if ch == '"' {
            o.push('\\');
        }
        o.push(ch);
    }
    o.push('"');
    o
}

/// A literal inside double quotes for pando (strict quoted strings): escape `"` and `\`.
fn quote_literal(v: &str) -> String {
    let mut o = String::with_capacity(v.len() + 2);
    o.push('"');
    for ch in v.chars() {
        if ch == '"' || ch == '\\' {
            o.push('\\');
        }
        o.push(ch);
    }
    o.push('"');
    o
}

fn attr_test(layer: Layer, negated: bool, value: &str, flags: &Flags, d: Dialect, m: &Mapping) -> Result<String, QueryError> {
    let a = attr_name(m, layer)?;
    let op = if negated { "!=" } else { "=" };
    // a regex without metacharacters is a literal (a faster lookup for pando)
    let plain = !value.chars().any(|c| ".*+?()[]{}|^$\\".contains(c));
    let flags = &Flags { literal: flags.literal || plain, ..flags.clone() };
    match d {
        Dialect::Pando => {
            if flags.literal {
                let mut s = format!("{a}{op}{}", quote_literal(value));
                if flags.case_insensitive {
                    s.push_str(" %c");
                }
                if flags.diacritics_insensitive {
                    s.push_str(" %d");
                }
                Ok(s)
            } else {
                if flags.diacritics_insensitive {
                    return unsupported("diacritics-insensitive regular expressions (/d) are not supported by this engine");
                }
                let ci = if flags.case_insensitive { "(?i)" } else { "" };
                Ok(format!("{a}{op}/{ci}^(?:{})$/", value.replace('/', "\\/")))
            }
        }
        Dialect::Cwb => {
            let re = if flags.literal { regex_escape(value) } else { value.to_string() };
            let mut f = String::new();
            if flags.case_insensitive {
                f.push('c');
            }
            if flags.diacritics_insensitive {
                f.push('d');
            }
            let f = if f.is_empty() { String::new() } else { format!("%{f}") };
            Ok(format!("{a}{op}{}{f}", quote(&re)))
        }
        Dialect::Manatee => {
            if flags.diacritics_insensitive {
                return unsupported("diacritics-insensitive search (/d) is not supported by this engine");
            }
            let re = if flags.literal { regex_escape(value) } else { value.to_string() };
            let ci = if flags.case_insensitive { "(?i)" } else { "" };
            Ok(format!("{a}{op}{}", quote(&format!("{ci}{re}"))))
        }
    }
}

fn cond_str(c: &Cond, d: Dialect, m: &Mapping, top: bool) -> Result<String, QueryError> {
    match c {
        Cond::Attr { layer, negated, value, flags } => attr_test(*layer, *negated, value, flags, d, m),
        Cond::And(a, b) => {
            let s = format!("{} & {}", cond_str(a, d, m, false)?, cond_str(b, d, m, false)?);
            Ok(if top { s } else { format!("({s})") })
        }
        Cond::Or(a, b) => {
            let s = format!("{} | {}", cond_str(a, d, m, false)?, cond_str(b, d, m, false)?);
            Ok(if top { s } else { format!("({s})") })
        }
        Cond::Not(_) => unreachable!("negation normal form"),
    }
}

fn token_str(cond: &Option<Cond>, d: Dialect, m: &Mapping) -> Result<String, QueryError> {
    Ok(match cond {
        None => "[]".to_string(),
        Some(c) => format!("[{}]", cond_str(c, d, m, true)?),
    })
}

fn quant(min: u32, max: u32) -> String {
    match (min, max) {
        (1, 1) => String::new(),
        (0, 1) => "?".into(),
        (0, UNBOUNDED) => "*".into(),
        (1, UNBOUNDED) => "+".into(),
        (n, UNBOUNDED) => format!("{{{n},}}"),
        (n, m) if n == m => format!("{{{n}}}"),
        (n, m) => format!("{{{n},{m}}}"),
    }
}

// ── CWB / Manatee: one query with ( … ) | ( … ) ─────────────────────────

/// Returns the query and whether it needs a sentence scope (Basic Search AND).
fn query_regex(q: &Query, d: Dialect, m: &Mapping) -> Result<(String, bool), QueryError> {
    match q {
        Query::Seq(es) => {
            let mut parts = Vec::new();
            for e in es {
                parts.push(match e {
                    Elem::Token { cond, min, max } => format!("{}{}", token_str(cond, d, m)?, quant(*min, *max)),
                    Elem::Group { query, min, max } => {
                        let (inner, s) = query_regex(query, d, m)?;
                        if s {
                            return unsupported("AND inside a group");
                        }
                        format!("({inner}){}", quant(*min, *max))
                    }
                });
            }
            Ok((parts.join(" "), false))
        }
        Query::Or(a, b) => {
            let (x, sx) = query_regex(a, d, m)?;
            let (y, sy) = query_regex(b, d, m)?;
            Ok((format!("({x}) | ({y})"), sx || sy))
        }
        Query::And(a, b) => {
            if m.structure("sentence").is_none() {
                return unsupported("AND needs sentences, which this resource does not have");
            }
            let (x, _) = query_regex(a, d, m)?;
            let (y, _) = query_regex(b, d, m)?;
            Ok((format!("(({x}) []* ({y}) | ({y}) []* ({x}))"), true))
        }
    }
}

// ── pando: a union of flat sequences ───────────────────────────────────

#[derive(Clone, Debug)]
struct FlatSeq {
    toks: Vec<(Option<Cond>, u32, u32)>,
    within: Option<String>,
}

fn cap(n: usize) -> Result<(), QueryError> {
    if n > MAX_ALTERNATIVES {
        return unsupported(format!("the query expands into more than {MAX_ALTERNATIVES} alternatives for this engine"));
    }
    Ok(())
}

fn flatten(q: &Query, m: &Mapping) -> Result<Vec<FlatSeq>, QueryError> {
    match q {
        Query::Seq(es) => {
            let mut acc = vec![FlatSeq { toks: vec![], within: None }];
            for e in es {
                match e {
                    Elem::Token { cond, min, max } => {
                        for s in &mut acc {
                            s.toks.push((cond.clone(), *min, *max));
                        }
                    }
                    Elem::Group { query, min, max } => {
                        let inner = flatten(query, m)?;
                        if inner.iter().any(|s| s.within.is_some()) {
                            return unsupported("AND inside a group");
                        }
                        if *max == UNBOUNDED {
                            return unsupported("groups with an unbounded quantifier are not supported by this engine");
                        }
                        // (X){min,max} → X repeated k times, min ≤ k ≤ max
                        let mut reps: Vec<FlatSeq> = Vec::new();
                        for k in *min..=*max {
                            let mut cur = vec![FlatSeq { toks: vec![], within: None }];
                            for _ in 0..k {
                                let mut next = Vec::new();
                                for c in &cur {
                                    for i in &inner {
                                        let mut t = c.toks.clone();
                                        t.extend(i.toks.iter().cloned());
                                        next.push(FlatSeq { toks: t, within: None });
                                    }
                                }
                                cap(next.len())?;
                                cur = next;
                            }
                            reps.extend(cur);
                            cap(reps.len())?;
                        }
                        let mut next = Vec::new();
                        for a in &acc {
                            for r in &reps {
                                let mut t = a.toks.clone();
                                t.extend(r.toks.iter().cloned());
                                next.push(FlatSeq { toks: t, within: None });
                            }
                        }
                        cap(next.len())?;
                        acc = next;
                    }
                }
            }
            // an empty repetition may leave a sequence without tokens
            acc.retain(|s| !s.toks.is_empty());
            if acc.is_empty() {
                return unsupported("the query can match an empty sequence");
            }
            Ok(acc)
        }
        Query::Or(a, b) => {
            let mut v = flatten(a, m)?;
            v.extend(flatten(b, m)?);
            cap(v.len())?;
            Ok(v)
        }
        Query::And(a, b) => {
            let s = match m.structure("sentence") {
                Some(s) => s.to_string(),
                None => return unsupported("AND needs sentences, which this resource does not have"),
            };
            // conjunctions of conjunctions: gather all conjuncts, then every order
            let mut conj = Vec::new();
            gather_and(q, &mut conj);
            let _ = (a, b);
            let mut per: Vec<Vec<FlatSeq>> = Vec::new();
            for c in &conj {
                per.push(flatten(c, m)?);
            }
            // choose one alternative per conjunct (cross product) …
            let mut choices: Vec<Vec<FlatSeq>> = vec![vec![]];
            for alts in &per {
                let mut next = Vec::new();
                for ch in &choices {
                    for a in alts {
                        let mut c = ch.clone();
                        c.push(a.clone());
                        next.push(c);
                    }
                }
                cap(next.len())?;
                choices = next;
            }
            // … and every order of the chosen parts, joined by []*
            let mut out = Vec::new();
            for ch in choices {
                for perm in permutations(ch.len()) {
                    let mut toks = Vec::new();
                    for (k, &i) in perm.iter().enumerate() {
                        if k > 0 {
                            toks.push((None, 0, UNBOUNDED));
                        }
                        toks.extend(ch[i].toks.iter().cloned());
                    }
                    out.push(FlatSeq { toks, within: Some(s.clone()) });
                    cap(out.len())?;
                }
            }
            Ok(out)
        }
    }
}

fn gather_and<'a>(q: &'a Query, out: &mut Vec<&'a Query>) {
    match q {
        Query::And(a, b) => {
            gather_and(a, out);
            gather_and(b, out);
        }
        other => out.push(other),
    }
}

fn permutations(n: usize) -> Vec<Vec<usize>> {
    fn rec(cur: &mut Vec<usize>, used: &mut Vec<bool>, out: &mut Vec<Vec<usize>>) {
        if cur.len() == used.len() {
            out.push(cur.clone());
            return;
        }
        for i in 0..used.len() {
            if !used[i] {
                used[i] = true;
                cur.push(i);
                rec(cur, used, out);
                cur.pop();
                used[i] = false;
            }
        }
    }
    let mut out = Vec::new();
    rec(&mut Vec::new(), &mut vec![false; n], &mut out);
    out
}

fn seq_pando(s: &FlatSeq, m: &Mapping) -> Result<String, QueryError> {
    let mut parts = Vec::new();
    for (cond, min, max) in &s.toks {
        parts.push(format!("{}{}", token_str(cond, Dialect::Pando, m)?, quant(*min, *max)));
    }
    Ok(parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::super::query::{parse_cql, parse_fcsql};
    use super::*;

    fn map(text: &str) -> Mapping {
        Mapping {
            layers: vec![(Layer::Text, text.into()), (Layer::Lemma, "lemma".into()), (Layer::Pos, "upos".into())],
            structures: vec![("sentence".into(), "s".into())],
        }
    }

    fn t(q: &str, fcs: bool, d: Dialect) -> Vec<String> {
        let p = if fcs { parse_fcsql(q) } else { parse_cql(q) }.unwrap();
        let m = map(if d == Dialect::Pando { "form" } else { "word" });
        translate(&p, d, &m).unwrap().queries
    }

    #[test]
    fn basic() {
        assert_eq!(t("house", false, Dialect::Pando), vec![r#"[form="house"]"#]);
        assert_eq!(t("house", false, Dialect::Cwb), vec![r#"[word="house"]"#]);
        assert_eq!(t("a.b", false, Dialect::Cwb), vec![r#"[word="a\.b"]"#]);
        assert_eq!(t("hous*", false, Dialect::Pando), vec![r#"[form=/^(?:hous.*)$/]"#]);
        assert_eq!(t("\"the house\"", false, Dialect::Manatee), vec![r#"[word="the"] [word="house"]"#]);
        assert_eq!(t("cat OR dog", false, Dialect::Pando), vec![r#"[form="cat"]"#, r#"[form="dog"]"#]);
        assert_eq!(t("cat OR dog", false, Dialect::Cwb), vec![r#"([word="cat"]) | ([word="dog"])"#]);
        assert_eq!(t("cat AND dog", false, Dialect::Pando),
                   vec![r#"[form="cat"] []* [form="dog"] within s"#, r#"[form="dog"] []* [form="cat"] within s"#]);
        assert_eq!(t("cat AND dog", false, Dialect::Cwb),
                   vec![r#"(([word="cat"]) []* ([word="dog"]) | ([word="dog"]) []* ([word="cat"])) within s"#]);
        assert_eq!(t("cat AND dog", false, Dialect::Manatee),
                   vec![r#"(([word="cat"]) []* ([word="dog"]) | ([word="dog"]) []* ([word="cat"])) within <s/>"#]);
    }

    #[test]
    fn advanced() {
        assert_eq!(t(r#"[lemma="walk"/c & !pos="VERB"] []{1,3} "x"/l within s"#, true, Dialect::Pando),
                   vec![r#"[lemma="walk" %c & upos!="VERB"] []{1,3} [form="x"] within s"#]);
        assert_eq!(t(r#"[lemma="walk.*"/c]"#, true, Dialect::Pando), vec![r#"[lemma=/(?i)^(?:walk.*)$/]"#]);
        assert_eq!(t(r#"[lemma="walk"/lc & !pos="VERB"]"#, true, Dialect::Pando),
                   vec![r#"[lemma="walk" %c & upos!="VERB"]"#]);
        assert_eq!(t(r#"[lemma="walk"/c & !pos="VERB"] within s"#, true, Dialect::Cwb),
                   vec![r#"[lemma="walk"%c & upos!="VERB"] within s"#]);
        assert_eq!(t(r#"[lemma="walk"/c]"#, true, Dialect::Manatee), vec![r#"[lemma="(?i)walk"]"#]);
        assert_eq!(t(r#"[!(lemma="a" | pos="B")]"#, true, Dialect::Cwb), vec![r#"[lemma!="a" & upos!="B"]"#]);
        assert_eq!(t(r#"([pos="DET"] [pos="NOUN"])+"#, true, Dialect::Cwb), vec![r#"([upos="DET"] [upos="NOUN"])+"#]);
        assert_eq!(t(r#"[pos="A"] ([pos="B"] | [pos="C"]) [pos="D"]?"#, true, Dialect::Pando),
                   vec![r#"[upos="A"] [upos="B"] [upos="D"]?"#, r#"[upos="A"] [upos="C"] [upos="D"]?"#]);
        assert_eq!(t(r#"([pos="B"]){1,2}"#, true, Dialect::Pando).len(), 2);
        let p = parse_fcsql(r#"([pos="B"])+"#).unwrap();
        assert!(matches!(translate(&p, Dialect::Pando, &map("form")), Err(QueryError::Unsupported(_))));
        let p = parse_fcsql(r#"[orth="x"]"#).unwrap();
        assert!(matches!(translate(&p, Dialect::Cwb, &map("word")), Err(QueryError::Unsupported(_))));
        let p = parse_fcsql(r#"[lemma="x"] within p"#).unwrap();
        assert!(matches!(translate(&p, Dialect::Cwb, &map("word")), Err(QueryError::Unsupported(_))));
        assert_eq!(t(r#""a\"b""#, true, Dialect::Cwb), vec![r#"[word="a\"b"]"#]);
        assert_eq!(t(r#""and/or"/l"#, true, Dialect::Pando), vec![r#"[form="and/or"]"#]);
        assert_eq!(t(r#""and/or""#, true, Dialect::Pando), vec![r#"[form="and/or"]"#]);
        assert_eq!(t(r#""a/b.""#, true, Dialect::Pando), vec![r#"[form=/^(?:a\/b.)$/]"#]);
    }
}
