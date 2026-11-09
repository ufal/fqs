//! A subcorpus definition: the recipe FQS stores (JSON), the same for every engine.
//!
//! - `{"kind": "regions", "struct": "text", "conditions": [{"attr": "genre", "op": "=", "value": "fiction"}]}`:
//!   the regions of a structure whose attributes match every condition (`op` `=` or `!=`; `value` a
//!   string, or a list: any of them). Values are literal (not patterns). No conditions: every region.
//! - `{"kind": "documents", "ids": ["d1", "d2"], "struct"?: "doc", "attr"?: "id"}`: the documents
//!   with these ids (the structure / attribute default per engine, filled in when it is stored).
//! - `{"kind": "query", "query": "[upos=\"NOUN\"]", "language"?: "pando-cql"}`: the positions of
//!   the hits of a token query, in the query language named (sent to the engine as written).
//! - `{"kind": "expression", "expression": "fiction & 1990s - spoken"}`: other subcorpora of the
//!   same corpus by name, with `&` (intersection), `|` (union), `-` (difference) and parentheses;
//!   `&` binds tighter than `|` and `-`, which go left to right (as in pando).

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub attr: String,
    #[serde(default = "eq")]
    pub op: String,
    /// a string, or a list of strings (any of them)
    pub value: Value,
}

fn eq() -> String {
    "=".into()
}

impl Condition {
    /// The values (one or more), checked.
    pub fn values(&self) -> Result<Vec<String>> {
        let v: Vec<String> = match &self.value {
            Value::String(s) => vec![s.clone()],
            Value::Number(n) => vec![n.to_string()],
            Value::Array(a) => a
                .iter()
                .map(|x| match x {
                    Value::String(s) => Ok(s.clone()),
                    Value::Number(n) => Ok(n.to_string()),
                    _ => bail!("condition on '{}': values must be strings", self.attr),
                })
                .collect::<Result<_>>()?,
            _ => bail!("condition on '{}': value must be a string or a list of strings", self.attr),
        };
        if v.is_empty() {
            bail!("condition on '{}': an empty list of values selects nothing", self.attr);
        }
        Ok(v)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Definition {
    Regions {
        #[serde(rename = "struct")]
        structure: String,
        #[serde(default)]
        conditions: Vec<Condition>,
    },
    Documents {
        ids: Vec<String>,
        #[serde(rename = "struct", default, skip_serializing_if = "Option::is_none")]
        structure: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attr: Option<String>,
    },
    Query {
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    Expression {
        expression: String,
    },
}

/// A structure or attribute name as it may appear in a query: letters, digits, `_`.
pub fn valid_ident(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// A subcorpus name: 1–64 letters, digits, `_` or `.` (no `-`, `&`, `|`: they are operators).
pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.chars().count() <= 64 && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
}

impl Definition {
    pub fn parse(v: &Value) -> Result<Definition> {
        let d: Definition = serde_json::from_value(v.clone())
            .map_err(|e| anyhow::anyhow!("bad subcorpus definition: {e} (kind: regions | documents | query | expression)"))?;
        d.check()?;
        Ok(d)
    }

    fn check(&self) -> Result<()> {
        match self {
            Definition::Regions { structure, conditions } => {
                if !valid_ident(structure) {
                    bail!("bad structure name '{structure}'");
                }
                for c in conditions {
                    if !valid_ident(&c.attr) {
                        bail!("bad attribute name '{}'", c.attr);
                    }
                    if c.op != "=" && c.op != "!=" {
                        bail!("condition on '{}': op must be = or != (got '{}')", c.attr, c.op);
                    }
                    c.values()?;
                }
            }
            Definition::Documents { ids, structure, attr } => {
                if ids.is_empty() {
                    bail!("a documents subcorpus needs at least one id");
                }
                if structure.as_deref().is_some_and(|s| !valid_ident(s)) || attr.as_deref().is_some_and(|s| !valid_ident(s)) {
                    bail!("bad structure or attribute name");
                }
            }
            Definition::Query { query, .. } => {
                if query.trim().is_empty() {
                    bail!("a query subcorpus needs a query");
                }
            }
            Definition::Expression { expression } => {
                Expr::parse(expression)?;
            }
        }
        Ok(())
    }
}

/// A parsed expression over subcorpus names.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Name(String),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Minus(Box<Expr>, Box<Expr>),
}

#[derive(Debug, PartialEq)]
enum Tok {
    Name(String),
    Op(char),
}

fn lex(s: &str) -> Result<Vec<Tok>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if "&|-()".contains(c) {
            out.push(Tok::Op(c));
            chars.next();
        } else if c.is_alphanumeric() || c == '_' || c == '.' {
            let mut n = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_alphanumeric() || c == '_' || c == '.' {
                    n.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(Tok::Name(n));
        } else {
            bail!("unexpected '{c}' in the expression (names, & | - and parentheses)");
        }
    }
    Ok(out)
}

impl Expr {
    pub fn parse(s: &str) -> Result<Expr> {
        let toks = lex(s)?;
        let mut pos = 0;
        let e = Self::sum(&toks, &mut pos)?;
        if pos != toks.len() {
            bail!("unexpected {:?} in the expression", toks[pos]);
        }
        Ok(e)
    }

    // sum := product (('|' | '-') product)*
    fn sum(t: &[Tok], pos: &mut usize) -> Result<Expr> {
        let mut left = Self::product(t, pos)?;
        while let Some(Tok::Op(op @ ('|' | '-'))) = t.get(*pos) {
            *pos += 1;
            let right = Self::product(t, pos)?;
            left = if *op == '|' { Expr::Or(left.into(), right.into()) } else { Expr::Minus(left.into(), right.into()) };
        }
        Ok(left)
    }

    // product := atom ('&' atom)*
    fn product(t: &[Tok], pos: &mut usize) -> Result<Expr> {
        let mut left = Self::atom(t, pos)?;
        while let Some(Tok::Op('&')) = t.get(*pos) {
            *pos += 1;
            left = Expr::And(left.into(), Self::atom(t, pos)?.into());
        }
        Ok(left)
    }

    fn atom(t: &[Tok], pos: &mut usize) -> Result<Expr> {
        match t.get(*pos) {
            Some(Tok::Name(n)) => {
                *pos += 1;
                if !valid_name(n) {
                    bail!("bad subcorpus name '{n}'");
                }
                Ok(Expr::Name(n.clone()))
            }
            Some(Tok::Op('(')) => {
                *pos += 1;
                let e = Self::sum(t, pos)?;
                if t.get(*pos) != Some(&Tok::Op(')')) {
                    bail!("missing ')' in the expression");
                }
                *pos += 1;
                Ok(e)
            }
            Some(other) => bail!("unexpected {other:?} in the expression"),
            None => bail!("the expression ends too early"),
        }
    }

    /// The names it uses, each once, in order of appearance.
    pub fn names(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<String>) {
        match self {
            Expr::Name(n) => {
                if !out.contains(n) {
                    out.push(n.clone());
                }
            }
            Expr::And(a, b) | Expr::Or(a, b) | Expr::Minus(a, b) => {
                a.collect(out);
                b.collect(out);
            }
        }
    }

    /// Evaluate with `leaf` giving each name's value and `op` combining two ('&', '|', '-').
    pub fn eval<T>(&self, leaf: &mut impl FnMut(&str) -> Result<T>, op: &impl Fn(char, T, T) -> T) -> Result<T> {
        Ok(match self {
            Expr::Name(n) => leaf(n)?,
            Expr::And(a, b) => op('&', a.eval(leaf, op)?, b.eval(leaf, op)?),
            Expr::Or(a, b) => op('|', a.eval(leaf, op)?, b.eval(leaf, op)?),
            Expr::Minus(a, b) => op('-', a.eval(leaf, op)?, b.eval(leaf, op)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn definitions_parse_and_check() {
        let r = Definition::parse(&json!({"kind": "regions", "struct": "text", "conditions": [{"attr": "genre", "value": "fiction"}]})).unwrap();
        if let Definition::Regions { conditions, .. } = &r {
            assert_eq!(conditions[0].op, "=", "op defaults to =");
        }
        assert!(Definition::parse(&json!({"kind": "regions", "struct": "te xt"})).is_err());
        assert!(Definition::parse(&json!({"kind": "regions", "struct": "text", "conditions": [{"attr": "g", "op": "~", "value": "x"}]})).is_err());
        assert!(Definition::parse(&json!({"kind": "regions", "struct": "text", "conditions": [{"attr": "g", "value": []}]})).is_err());
        assert!(Definition::parse(&json!({"kind": "documents", "ids": []})).is_err());
        assert!(Definition::parse(&json!({"kind": "query", "query": " "})).is_err());
        assert!(Definition::parse(&json!({"kind": "expression", "expression": "a & (b"})).is_err());
        assert!(Definition::parse(&json!({"kind": "nonsense"})).is_err());
        assert!(Definition::parse(&json!({"kind": "query", "query": "[]", "language": "manatee-cql"})).is_ok());
    }

    #[test]
    fn expressions() {
        let e = Expr::parse("a | b & c - d").unwrap();
        // & first; | and - left to right
        let want = Expr::Minus(
            Expr::Or(Expr::Name("a".into()).into(), Expr::And(Expr::Name("b".into()).into(), Expr::Name("c".into()).into()).into()).into(),
            Expr::Name("d".into()).into(),
        );
        assert_eq!(e, want);
        assert_eq!(Expr::parse("(fiction | poetry) & 1990s").unwrap().names(), vec!["fiction", "poetry", "1990s"]);
        assert!(Expr::parse("a &").is_err());
        assert!(Expr::parse("a b").is_err());
        assert!(Expr::parse("a ; b").is_err());
        let v = Expr::parse("a - (b | c)").unwrap().eval(&mut |n| Ok(n.to_string()), &|o, x, y| format!("({x}{o}{y})")).unwrap();
        assert_eq!(v, "(a-(b|c))");
    }

    #[test]
    fn names() {
        assert!(valid_name("1990s"));
        assert!(valid_name("fiction.v2"));
        assert!(!valid_name("a-b"));
        assert!(!valid_name(""));
        assert!(!valid_name("a b"));
    }
}
