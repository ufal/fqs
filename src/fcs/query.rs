//! FCS queries: CQL (Basic Search) and FCS-QL (Advanced Search) parsed into
//! one engine-neutral form, which `translate` turns into each engine's dialect.
//!
//! Basic Search (CQL, `queryType=cql`): search terms and "quoted phrases",
//! combined with AND (both in the same sentence), OR, parentheses; `*` / `?`
//! masking inside a term. Index / relation (`cql.serverChoice = x`, `text=x`) is
//! accepted for the text layer only.
//!
//! Advanced Search (FCS-QL, `queryType=fcs`): `"x"` (text layer), segments
//! `[lemma="x" & pos="NOUN"]` with `& | ! ( )`, `=` / `!=`, regular-expression
//! values with flags (`/i /c /I /C /l /d`), `[]`, quantifiers `? * + {n} {n,} {n,m}
//! {,m}`, sequences, `|` between sequences, groups `( … )` with quantifiers, and a
//! final `within s|sentence|p|paragraph|u|utterance|t|turn|text|session|e`.

use std::fmt;

pub const UNBOUNDED: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Layer {
    Text,
    Lemma,
    Pos,
    Orth,
    Norm,
    Phonetic,
}

impl Layer {
    pub fn parse(s: &str) -> Option<Layer> {
        // an optional qualifier (`q:layer`) is ignored: one layer of each type
        let t = s.rsplit(':').next().unwrap_or(s).trim().to_ascii_lowercase();
        Some(match t.as_str() {
            "text" | "word" => Layer::Text,
            "lemma" => Layer::Lemma,
            "pos" => Layer::Pos,
            "orth" => Layer::Orth,
            "norm" => Layer::Norm,
            "phonetic" => Layer::Phonetic,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Layer::Text => "text",
            Layer::Lemma => "lemma",
            Layer::Pos => "pos",
            Layer::Orth => "orth",
            Layer::Norm => "norm",
            Layer::Phonetic => "phonetic",
        }
    }
    pub const ALL: [Layer; 6] = [Layer::Text, Layer::Lemma, Layer::Pos, Layer::Orth, Layer::Norm, Layer::Phonetic];
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    pub case_insensitive: bool,
    pub diacritics_insensitive: bool,
    /// the value is a literal string, not a regular expression
    pub literal: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cond {
    Attr { layer: Layer, negated: bool, value: String, flags: Flags },
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    Not(Box<Cond>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Elem {
    /// one token position; `None` = `[]` (any token)
    Token { cond: Option<Cond>, min: u32, max: u32 },
    /// `( … )` with a quantifier
    Group { query: Box<Query>, min: u32, max: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Query {
    Seq(Vec<Elem>),
    Or(Box<Query>, Box<Query>),
    /// Basic Search AND: both parts inside the same sentence
    And(Box<Query>, Box<Query>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    pub query: Query,
    /// `within …` scope (FCS-QL) as written, lower-cased
    pub within: Option<String>,
}

/// A syntax error (SRU diagnostic 10) or a feature we do not support (SRU 48).
#[derive(Clone, Debug, PartialEq)]
pub enum QueryError {
    Syntax(String),
    Unsupported(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::Syntax(s) => write!(f, "{s}"),
            QueryError::Unsupported(s) => write!(f, "{s}"),
        }
    }
}

fn syn<T>(msg: impl Into<String>) -> Result<T, QueryError> {
    Err(QueryError::Syntax(msg.into()))
}

// ── Basic Search: CQL ──────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum CTok {
    LParen,
    RParen,
    Word(String),   // unquoted term / keyword
    Quoted(String), // "…"
    Rel(String),    // = == exact any all adj < > …
}

fn cql_lex(s: &str) -> Result<Vec<CTok>, QueryError> {
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        match ch {
            '(' => { out.push(CTok::LParen); i += 1; }
            ')' => { out.push(CTok::RParen); i += 1; }
            '"' => {
                let mut v = String::new();
                i += 1;
                let mut closed = false;
                while i < c.len() {
                    if c[i] == '\\' && i + 1 < c.len() {
                        // keep the escape: masking characters stay escaped
                        v.push('\\');
                        v.push(c[i + 1]);
                        i += 2;
                        continue;
                    }
                    if c[i] == '"' { closed = true; i += 1; break; }
                    v.push(c[i]);
                    i += 1;
                }
                if !closed {
                    return syn("unterminated quoted string");
                }
                out.push(CTok::Quoted(v));
            }
            '=' | '<' | '>' => {
                let mut r = String::new();
                while i < c.len() && matches!(c[i], '=' | '<' | '>') {
                    r.push(c[i]);
                    i += 1;
                }
                out.push(CTok::Rel(r));
            }
            '/' => return Err(QueryError::Unsupported("CQL relation modifiers are not supported".into())),
            _ => {
                let mut v = String::new();
                while i < c.len() && !c[i].is_whitespace() && !matches!(c[i], '(' | ')' | '"' | '=' | '<' | '>' | '/') {
                    if c[i] == '\\' && i + 1 < c.len() {
                        v.push('\\');
                        v.push(c[i + 1]);
                        i += 2;
                        continue;
                    }
                    v.push(c[i]);
                    i += 1;
                }
                out.push(CTok::Word(v));
            }
        }
    }
    Ok(out)
}

struct CqlParser {
    t: Vec<CTok>,
    i: usize,
}

impl CqlParser {
    fn peek(&self) -> Option<&CTok> {
        self.t.get(self.i)
    }
    fn keyword(&self, k: &str) -> bool {
        matches!(self.peek(), Some(CTok::Word(w)) if w.eq_ignore_ascii_case(k))
    }
    fn or(&mut self) -> Result<Query, QueryError> {
        let mut l = self.and()?;
        while self.keyword("or") {
            self.i += 1;
            let r = self.and()?;
            l = Query::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }
    fn and(&mut self) -> Result<Query, QueryError> {
        let mut l = self.prim()?;
        loop {
            if self.keyword("and") {
                self.i += 1;
                let r = self.prim()?;
                l = Query::And(Box::new(l), Box::new(r));
            } else if self.keyword("not") {
                return Err(QueryError::Unsupported("CQL NOT is not supported".into()));
            } else if self.keyword("prox") {
                return Err(QueryError::Unsupported("CQL PROX is not supported".into()));
            } else {
                break;
            }
        }
        Ok(l)
    }
    fn prim(&mut self) -> Result<Query, QueryError> {
        match self.peek().cloned() {
            Some(CTok::LParen) => {
                self.i += 1;
                let q = self.or()?;
                if self.peek() != Some(&CTok::RParen) {
                    return syn("missing ')'");
                }
                self.i += 1;
                Ok(q)
            }
            Some(CTok::Word(w)) | Some(CTok::Quoted(w)) => {
                self.i += 1;
                // index relation term?
                if let Some(CTok::Rel(r)) = self.peek().cloned() {
                    self.i += 1;
                    let idx = w.to_ascii_lowercase();
                    let ok_index = matches!(idx.as_str(), "cql.serverchoice" | "serverchoice" | "text" | "word" | "fcs.text");
                    if !ok_index {
                        return Err(QueryError::Unsupported(format!("unsupported index '{w}' (only the text layer)")));
                    }
                    if r != "=" && r != "==" && r != "any" {
                        return Err(QueryError::Unsupported(format!("unsupported relation '{r}'")));
                    }
                    let term = match self.peek().cloned() {
                        Some(CTok::Word(t)) | Some(CTok::Quoted(t)) => { self.i += 1; t }
                        _ => return syn("missing search term after relation"),
                    };
                    return term_query(&term);
                }
                if let Some(CTok::Word(k)) = self.peek() {
                    if ["any", "all", "adj", "exact", "within", "encloses"].iter().any(|x| k.eq_ignore_ascii_case(x)) {
                        return Err(QueryError::Unsupported(format!("unsupported relation '{k}'")));
                    }
                }
                term_query(&w)
            }
            Some(CTok::RParen) => syn("unexpected ')'"),
            Some(CTok::Rel(r)) => syn(format!("unexpected '{r}'")),
            None => syn("empty query"),
        }
    }
}

/// A CQL term (possibly a quoted phrase): one token per word, literal except for
/// the masking characters `*` (any characters) and `?` (one character).
fn term_query(term: &str) -> Result<Query, QueryError> {
    let words: Vec<&str> = term.split_whitespace().collect();
    if words.is_empty() {
        return syn("empty search term");
    }
    let mut elems = Vec::new();
    for w in words {
        let (value, has_mask) = cql_mask_to_regex(w);
        elems.push(Elem::Token {
            cond: Some(Cond::Attr {
                layer: Layer::Text,
                negated: false,
                value,
                flags: Flags { literal: !has_mask, ..Flags::default() },
            }),
            min: 1,
            max: 1,
        });
    }
    Ok(Query::Seq(elems))
}

/// CQL masking → a regular expression (when there is a mask), else the literal.
fn cql_mask_to_regex(w: &str) -> (String, bool) {
    let c: Vec<char> = w.chars().collect();
    let mut has_mask = false;
    let mut lit = String::new();
    let mut re = String::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\\' if i + 1 < c.len() => {
                lit.push(c[i + 1]);
                re.push_str(&regex_escape(&c[i + 1].to_string()));
                i += 2;
                continue;
            }
            '*' => { has_mask = true; re.push_str(".*"); }
            '?' => { has_mask = true; re.push('.'); }
            '^' => { /* anchoring: every term is a whole token anyway */ }
            ch => { lit.push(ch); re.push_str(&regex_escape(&ch.to_string())); }
        }
        i += 1;
    }
    if has_mask { (re, true) } else { (lit, false) }
}

pub fn regex_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for ch in s.chars() {
        if ".*+?()[]{}|^$\\".contains(ch) {
            o.push('\\');
        }
        o.push(ch);
    }
    o
}

pub fn parse_cql(s: &str) -> Result<Parsed, QueryError> {
    let toks = cql_lex(s)?;
    if toks.is_empty() {
        return syn("empty query");
    }
    let mut p = CqlParser { t: toks, i: 0 };
    let q = p.or()?;
    if p.i != p.t.len() {
        return syn(format!("unexpected input after position {}", p.i));
    }
    Ok(Parsed { query: q, within: None })
}

// ── Advanced Search: FCS-QL ────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum FTok {
    LBr, RBr, LPar, RPar, LBrace, RBrace, Comma,
    Amp, Bar, Bang, Eq, Neq,
    Plus, Star, Quest,
    Str(String, Flags),
    Ident(String),
    Num(u32),
}

fn fcs_lex(s: &str) -> Result<Vec<FTok>, QueryError> {
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() { i += 1; continue; }
        match ch {
            '[' => { out.push(FTok::LBr); i += 1; }
            ']' => { out.push(FTok::RBr); i += 1; }
            '(' => { out.push(FTok::LPar); i += 1; }
            ')' => { out.push(FTok::RPar); i += 1; }
            '{' => { out.push(FTok::LBrace); i += 1; }
            '}' => { out.push(FTok::RBrace); i += 1; }
            ',' => { out.push(FTok::Comma); i += 1; }
            '&' => { out.push(FTok::Amp); i += 1; }
            '|' => { out.push(FTok::Bar); i += 1; }
            '+' => { out.push(FTok::Plus); i += 1; }
            '*' => { out.push(FTok::Star); i += 1; }
            '?' => { out.push(FTok::Quest); i += 1; }
            '!' => {
                if i + 1 < c.len() && c[i + 1] == '=' { out.push(FTok::Neq); i += 2; } else { out.push(FTok::Bang); i += 1; }
            }
            '=' => {
                i += 1;
                if i < c.len() && c[i] == '=' { i += 1; }
                out.push(FTok::Eq);
            }
            '"' | '\'' => {
                let q = ch;
                i += 1;
                let mut v = String::new();
                let mut closed = false;
                while i < c.len() {
                    if c[i] == '\\' && i + 1 < c.len() {
                        let n = c[i + 1];
                        if n == q || n == '\\' && false {
                            v.push(n);
                        } else {
                            // keep regex escapes (\. \d …) as written
                            v.push('\\');
                            v.push(n);
                        }
                        i += 2;
                        continue;
                    }
                    if c[i] == q { closed = true; i += 1; break; }
                    v.push(c[i]);
                    i += 1;
                }
                if !closed { return syn("unterminated string"); }
                let mut flags = Flags::default();
                if i < c.len() && c[i] == '/' {
                    i += 1;
                    let mut any = false;
                    while i < c.len() && c[i].is_ascii_alphabetic() {
                        match c[i] {
                            'i' | 'c' => flags.case_insensitive = true,
                            'I' | 'C' => flags.case_insensitive = false,
                            'l' => flags.literal = true,
                            'd' => flags.diacritics_insensitive = true,
                            f => return syn(format!("unknown flag '/{f}'")),
                        }
                        any = true;
                        i += 1;
                    }
                    if !any { return syn("missing flag after '/'"); }
                }
                out.push(FTok::Str(v, flags));
            }
            d if d.is_ascii_digit() => {
                let mut n = 0u64;
                while i < c.len() && c[i].is_ascii_digit() {
                    n = n * 10 + c[i].to_digit(10).unwrap() as u64;
                    if n > 100_000 { return syn("number too large"); }
                    i += 1;
                }
                out.push(FTok::Num(n as u32));
            }
            a if a.is_alphabetic() || a == '_' => {
                let mut v = String::new();
                while i < c.len() && (c[i].is_alphanumeric() || matches!(c[i], '_' | '-' | ':' | '.')) {
                    v.push(c[i]);
                    i += 1;
                }
                out.push(FTok::Ident(v));
            }
            _ => return syn(format!("unexpected character '{ch}'")),
        }
    }
    Ok(out)
}

struct FcsParser {
    t: Vec<FTok>,
    i: usize,
}

impl FcsParser {
    fn peek(&self) -> Option<&FTok> { self.t.get(self.i) }
    fn eat(&mut self, t: &FTok) -> bool {
        if self.peek() == Some(t) { self.i += 1; true } else { false }
    }
    fn at_within(&self) -> bool {
        matches!(self.peek(), Some(FTok::Ident(w)) if w.eq_ignore_ascii_case("within"))
    }
    fn disjunction(&mut self) -> Result<Query, QueryError> {
        let mut l = self.sequence()?;
        while self.eat(&FTok::Bar) {
            let r = self.sequence()?;
            l = Query::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }
    fn sequence(&mut self) -> Result<Query, QueryError> {
        let mut elems = Vec::new();
        loop {
            match self.peek() {
                Some(FTok::LBr) | Some(FTok::LPar) | Some(FTok::Str(..)) => elems.push(self.element()?),
                _ => break,
            }
            if self.at_within() { break; }
        }
        if elems.is_empty() {
            return syn("expected a segment, a string or '('");
        }
        Ok(Query::Seq(elems))
    }
    fn element(&mut self) -> Result<Elem, QueryError> {
        match self.peek().cloned() {
            Some(FTok::LPar) => {
                self.i += 1;
                let q = self.disjunction()?;
                if !self.eat(&FTok::RPar) { return syn("missing ')'"); }
                let (min, max) = self.quantifier()?;
                if min == 1 && max == 1 {
                    // a plain group: splice a single sequence, keep alternations
                    return Ok(Elem::Group { query: Box::new(q), min, max });
                }
                Ok(Elem::Group { query: Box::new(q), min, max })
            }
            Some(FTok::Str(v, flags)) => {
                self.i += 1;
                let (min, max) = self.quantifier()?;
                Ok(Elem::Token {
                    cond: Some(Cond::Attr { layer: Layer::Text, negated: false, value: v, flags }),
                    min,
                    max,
                })
            }
            Some(FTok::LBr) => {
                self.i += 1;
                let cond = if self.peek() == Some(&FTok::RBr) { None } else { Some(self.expr_or()?) };
                if !self.eat(&FTok::RBr) { return syn("missing ']'"); }
                let (min, max) = self.quantifier()?;
                Ok(Elem::Token { cond, min, max })
            }
            _ => syn("expected a segment"),
        }
    }
    fn quantifier(&mut self) -> Result<(u32, u32), QueryError> {
        match self.peek() {
            Some(FTok::Plus) => { self.i += 1; Ok((1, UNBOUNDED)) }
            Some(FTok::Star) => { self.i += 1; Ok((0, UNBOUNDED)) }
            Some(FTok::Quest) => { self.i += 1; Ok((0, 1)) }
            Some(FTok::LBrace) => {
                self.i += 1;
                let min = match self.peek() { Some(FTok::Num(n)) => { let n = *n; self.i += 1; n } _ => 0 };
                let max = if self.eat(&FTok::Comma) {
                    match self.peek() { Some(FTok::Num(n)) => { let n = *n; self.i += 1; n } _ => UNBOUNDED }
                } else {
                    min
                };
                if !self.eat(&FTok::RBrace) { return syn("missing '}'"); }
                if max < min || max == 0 { return syn("bad quantifier bounds"); }
                Ok((min, max))
            }
            _ => Ok((1, 1)),
        }
    }
    fn expr_or(&mut self) -> Result<Cond, QueryError> {
        let mut l = self.expr_and()?;
        while self.eat(&FTok::Bar) {
            let r = self.expr_and()?;
            l = Cond::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }
    fn expr_and(&mut self) -> Result<Cond, QueryError> {
        let mut l = self.expr_not()?;
        while self.eat(&FTok::Amp) {
            let r = self.expr_not()?;
            l = Cond::And(Box::new(l), Box::new(r));
        }
        Ok(l)
    }
    fn expr_not(&mut self) -> Result<Cond, QueryError> {
        if self.eat(&FTok::Bang) {
            return Ok(Cond::Not(Box::new(self.expr_not()?)));
        }
        if self.eat(&FTok::LPar) {
            let e = self.expr_or()?;
            if !self.eat(&FTok::RPar) { return syn("missing ')'"); }
            return Ok(e);
        }
        let id = match self.peek().cloned() {
            Some(FTok::Ident(s)) => { self.i += 1; s }
            _ => return syn("expected a layer name (text, lemma, pos, …)"),
        };
        let layer = Layer::parse(&id).ok_or_else(|| QueryError::Unsupported(format!("unknown layer '{id}'")))?;
        let negated = match self.peek() {
            Some(FTok::Eq) => { self.i += 1; false }
            Some(FTok::Neq) => { self.i += 1; true }
            _ => return syn(format!("expected '=' or '!=' after '{id}'")),
        };
        match self.peek().cloned() {
            Some(FTok::Str(v, flags)) => { self.i += 1; Ok(Cond::Attr { layer, negated, value: v, flags }) }
            _ => syn("expected a quoted value"),
        }
    }
}

pub fn parse_fcsql(s: &str) -> Result<Parsed, QueryError> {
    let toks = fcs_lex(s)?;
    if toks.is_empty() {
        return syn("empty query");
    }
    let mut p = FcsParser { t: toks, i: 0 };
    let q = p.disjunction()?;
    let mut within = None;
    if p.at_within() {
        p.i += 1;
        match p.peek().cloned() {
            Some(FTok::Ident(w)) => { p.i += 1; within = Some(w.to_ascii_lowercase()); }
            _ => return syn("expected a scope after 'within'"),
        }
    }
    if p.i != p.t.len() {
        return syn("unexpected input at the end of the query");
    }
    Ok(Parsed { query: simplify(q), within })
}

/// Splice groups without quantifier that hold one sequence into the outer one.
fn simplify(q: Query) -> Query {
    match q {
        Query::Seq(elems) => {
            let mut out = Vec::new();
            for e in elems {
                match e {
                    Elem::Group { query, min: 1, max: 1 } => match simplify(*query) {
                        Query::Seq(inner) => out.extend(inner),
                        other => out.push(Elem::Group { query: Box::new(other), min: 1, max: 1 }),
                    },
                    Elem::Group { query, min, max } => out.push(Elem::Group { query: Box::new(simplify(*query)), min, max }),
                    t => out.push(t),
                }
            }
            Query::Seq(out)
        }
        Query::Or(a, b) => Query::Or(Box::new(simplify(*a)), Box::new(simplify(*b))),
        Query::And(a, b) => Query::And(Box::new(simplify(*a)), Box::new(simplify(*b))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(layer: Layer, v: &str, literal: bool) -> Elem {
        Elem::Token {
            cond: Some(Cond::Attr { layer, negated: false, value: v.into(), flags: Flags { literal, ..Default::default() } }),
            min: 1,
            max: 1,
        }
    }

    #[test]
    fn cql_terms() {
        assert_eq!(parse_cql("house").unwrap().query, Query::Seq(vec![tok(Layer::Text, "house", true)]));
        assert_eq!(parse_cql("\"big house\"").unwrap().query,
                   Query::Seq(vec![tok(Layer::Text, "big", true), tok(Layer::Text, "house", true)]));
        assert_eq!(parse_cql("hous*").unwrap().query, Query::Seq(vec![tok(Layer::Text, "hous.*", false)]));
        assert_eq!(parse_cql("a.b").unwrap().query, Query::Seq(vec![tok(Layer::Text, "a.b", true)]));
        assert!(matches!(parse_cql("cat AND dog").unwrap().query, Query::And(..)));
        assert!(matches!(parse_cql("cat or (dog and mouse)").unwrap().query, Query::Or(..)));
        assert_eq!(parse_cql("cql.serverChoice = house").unwrap().query, Query::Seq(vec![tok(Layer::Text, "house", true)]));
        assert!(matches!(parse_cql("dc.title = x"), Err(QueryError::Unsupported(_))));
        assert!(matches!(parse_cql("cat NOT dog"), Err(QueryError::Unsupported(_))));
        assert!(matches!(parse_cql("(cat"), Err(QueryError::Syntax(_))));
        assert!(matches!(parse_cql(""), Err(QueryError::Syntax(_))));
    }

    #[test]
    fn fcsql_basic() {
        let p = parse_fcsql(r#"[lemma="walk" & pos="VERB"] "the"/c []{1,3} [pos="NOUN"]+ within s"#).unwrap();
        assert_eq!(p.within.as_deref(), Some("s"));
        match p.query {
            Query::Seq(es) => {
                assert_eq!(es.len(), 4);
                assert!(matches!(&es[1], Elem::Token { cond: Some(Cond::Attr { flags, .. }), .. } if flags.case_insensitive));
                assert!(matches!(&es[2], Elem::Token { cond: None, min: 1, max: 3 }));
                assert!(matches!(&es[3], Elem::Token { min: 1, max: UNBOUNDED, .. }));
            }
            _ => panic!(),
        }
        assert!(matches!(parse_fcsql(r#"[word != "x"]"#).unwrap().query,
                         Query::Seq(ref v) if matches!(&v[0], Elem::Token { cond: Some(Cond::Attr { negated: true, .. }), .. })));
        assert!(matches!(parse_fcsql(r#"[pos="A"] | [pos="B"] [pos="C"]"#).unwrap().query, Query::Or(..)));
        assert!(matches!(parse_fcsql(r#"([pos="A"] [pos="B"])+"#).unwrap().query,
                         Query::Seq(ref v) if matches!(&v[0], Elem::Group { min: 1, .. })));
        // a plain group is spliced
        assert_eq!(parse_fcsql(r#"[pos="A"] ([pos="B"])"#).unwrap().query,
                   Query::Seq(vec![tok(Layer::Pos, "A", false), tok(Layer::Pos, "B", false)]));
        assert!(matches!(parse_fcsql(r#"[foo="x"]"#), Err(QueryError::Unsupported(_))));
        assert!(matches!(parse_fcsql(r#"[lemma="x""#), Err(QueryError::Syntax(_))));
        assert!(matches!(parse_fcsql(r#"[lemma="x"]{3,1}"#), Err(QueryError::Syntax(_))));
        assert_eq!(parse_fcsql(r#"'it\'s'"#).unwrap().query, Query::Seq(vec![tok(Layer::Text, "it's", false)]));
        assert!(matches!(parse_fcsql(r#"[!(lemma="a" | pos="B")]"#).unwrap().query, Query::Seq(_)));
    }
}
