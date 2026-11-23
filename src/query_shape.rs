//! Grouping logged queries into *shapes* for the usage report: literals in
//! quotes and bare numbers become `?`, so `[lemma="dog"]` and `[lemma="cat"]`
//! count as one. Also splits `;`-separated statements, telling the front-end's
//! aggregation clauses (`freq … by …`) from the pattern the user typed, and
//! checks bracket / quote balance for sorting syntax errors. No query is
//! parsed for real: this only reads the surface, and never rewrites a query
//! that is sent anywhere.

/// First words that make a statement an aggregation over a result, not a pattern.
const AGG_WORDS: &[&str] = &["freq", "count", "group", "sort", "size", "distinct"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// one pattern statement
    Search,
    /// pattern(s) plus an aggregation clause (`freq … by …`), as the front-ends write them
    Aggregation,
    /// several pattern / assignment statements (`A = …; B = …`) without an aggregation
    Multi,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Search => "search",
            Kind::Aggregation => "aggregation",
            Kind::Multi => "multi-statement",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Analysis {
    /// the pattern statements only (aggregation clauses left out): what makes
    /// a search and its `freq` follow-up the same search
    pub base: String,
    /// `base` with literals replaced by `?`, whitespace collapsed
    pub shape: String,
    pub kind: Kind,
    pub brackets_ok: bool,
    pub quotes_ok: bool,
}

/// Split on `;` outside double quotes (backslash escapes a quote).
pub fn split_statements(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut esc = false;
    for c in q.chars() {
        if in_q {
            cur.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_q = false;
            }
        } else if c == '"' {
            in_q = true;
            cur.push(c);
        } else if c == ';' {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn is_aggregation(stmt: &str) -> bool {
    let first = stmt.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    AGG_WORDS.contains(&first.as_str())
}

/// Literals → `?` (quoted strings, standalone numbers); whitespace collapsed.
fn shape_of(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_q = false;
    let mut esc = false;
    let mut prev_word = false;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_q {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_q = false;
                out.push_str("\"?\"");
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_q = true;
            prev_word = false;
            i += 1;
            continue;
        }
        if c.is_ascii_digit() && !prev_word {
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            // a number inside an identifier (`s2`, `a1`) is not a literal
            if i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                out.push('?');
                continue;
            }
            out.push('?');
            prev_word = false;
            continue;
        }
        prev_word = c.is_alphanumeric() || c == '_';
        out.push(c);
        i += 1;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn balance(s: &str) -> (bool, bool) {
    let (mut open, mut close) = (0i64, 0i64);
    let mut in_q = false;
    let mut esc = false;
    let mut quotes_ok = true;
    for c in s.chars() {
        if in_q {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_q = false;
            }
        } else {
            match c {
                '"' => in_q = true,
                '[' => open += 1,
                ']' => close += 1,
                _ => {}
            }
        }
    }
    if in_q {
        quotes_ok = false;
    }
    (open == close, quotes_ok)
}

pub fn analyse(q: &str) -> Analysis {
    let stmts = split_statements(q);
    let (aggs, pats): (Vec<&String>, Vec<&String>) = stmts.iter().partition(|s| is_aggregation(s));
    let kind = if !aggs.is_empty() {
        Kind::Aggregation
    } else if pats.len() > 1 {
        Kind::Multi
    } else {
        Kind::Search
    };
    let base = pats.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("; ");
    let (brackets_ok, quotes_ok) = balance(q);
    Analysis { shape: shape_of(&base), base, kind, brackets_ok, quotes_ok }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_become_placeholders() {
        let a = analyse(r#"[lemma="dog" & upos="NOUN"]"#);
        assert_eq!(a.shape, r#"[lemma="?" & upos="?"]"#);
        assert_eq!(analyse(r#"[lemma="cat" & upos="VERB"]"#).shape, a.shape);
    }

    #[test]
    fn freq_clause_is_split_off() {
        let s = r#"a:[lemma="feel"] b:[upos!="PUNCT"] within s"#;
        let q = format!("{s}; freq by b.form;");
        let a = analyse(&q);
        assert_eq!(a.kind, Kind::Aggregation);
        assert_eq!(a.base, s);
        assert_eq!(analyse(s).base, s);
        assert_eq!(analyse(s).kind, Kind::Search);
    }

    #[test]
    fn semicolon_inside_quotes_does_not_split() {
        assert_eq!(split_statements(r#"[word=";"]; freq by x"#).len(), 2);
    }

    #[test]
    fn multi_statement_assignments() {
        let a = analyse(r#"A = v:[upos="VERB"]; B = v:[upos="NOUN"]; freq A, B by text_lang;"#);
        assert_eq!(a.kind, Kind::Aggregation);
        assert_eq!(a.base, r#"A = v:[upos="VERB"]; B = v:[upos="NOUN"]"#);
    }

    #[test]
    fn balance_checks() {
        assert!(!analyse(r#"[upos="VERB""#).brackets_ok);
        assert!(!analyse(r#"[upos="VERB]"#).quotes_ok);
        assert!(analyse(r#"[upos="VERB"]"#).brackets_ok);
    }

    #[test]
    fn numbers_are_literals_identifiers_are_not() {
        assert_eq!(shape_of("a1:[] within 5 s"), "a1:[] within ? s");
    }
}
