//! Stored subcorpora: the `subcorpora` table of FQS's catalogue database (next to `corpora`).
//! One row per (corpus, owner, name): the recipe (definition.rs), who sees it (policy.rs), and what
//! it resolved to last (size, pieces, encoding; the engine and the index it was resolved against,
//! pando's `index_id` or a Manatee data stamp; a Manatee `.subc` file). Random selections (later)
//! also keep their resolved document list (`documents_json`).

use super::definition::Definition;
use super::policy::Visibility;
use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::{json, Value};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS subcorpora (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    corpus_id TEXT NOT NULL,
    name TEXT NOT NULL,
    owner TEXT NOT NULL,
    visibility TEXT NOT NULL DEFAULT 'private' CHECK(visibility IN ('private','groups','public')),
    groups_json TEXT NOT NULL DEFAULT '[]',
    description TEXT,
    definition_json TEXT NOT NULL,
    engine TEXT,
    tokens INTEGER,
    ranges INTEGER,
    encoding TEXT,
    resolved_against TEXT,
    resolved_at TEXT,
    subc_path TEXT,
    documents_json TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(corpus_id, owner, name)
);
CREATE INDEX IF NOT EXISTS subcorpora_corpus ON subcorpora(corpus_id, name);
"#;

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA).context("creating the subcorpora table")
}

#[derive(Clone, Debug)]
pub struct Subcorpus {
    pub id: i64,
    pub corpus_id: String,
    pub name: String,
    pub owner: String,
    pub visibility: Visibility,
    pub groups: Vec<String>,
    pub description: Option<String>,
    pub definition: Definition,
    pub resolution: Option<Resolution>,
    pub created_at: String,
    pub updated_at: String,
}


/// What a definition resolved to, on one engine and index.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolution {
    pub engine: String,
    pub tokens: u64,
    pub ranges: u64,
    pub encoding: String,
    /// pando: the index's `index_id`; Manatee: a stamp of its registry and data files
    pub against: String,
    pub at: String,
    /// Manatee: the .subc file
    pub subc_path: Option<String>,
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn from_row(r: &Row<'_>) -> rusqlite::Result<Subcorpus> {
    let def_json: String = r.get("definition_json")?;
    let groups_json: String = r.get("groups_json")?;
    let vis: String = r.get("visibility")?;
    let definition = serde_json::from_str(&def_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let engine: Option<String> = r.get("engine")?;
    let resolution = match engine {
        Some(engine) => Some(Resolution {
            engine,
            tokens: r.get::<_, Option<i64>>("tokens")?.unwrap_or(0) as u64,
            ranges: r.get::<_, Option<i64>>("ranges")?.unwrap_or(0) as u64,
            encoding: r.get::<_, Option<String>>("encoding")?.unwrap_or_default(),
            against: r.get::<_, Option<String>>("resolved_against")?.unwrap_or_default(),
            at: r.get::<_, Option<String>>("resolved_at")?.unwrap_or_default(),
            subc_path: r.get("subc_path")?,
        }),
        None => None,
    };
    Ok(Subcorpus {
        id: r.get("id")?,
        corpus_id: r.get("corpus_id")?,
        name: r.get("name")?,
        owner: r.get("owner")?,
        visibility: Visibility::parse(&vis).unwrap_or(Visibility::Private),
        groups: serde_json::from_str(&groups_json).unwrap_or_default(),
        description: r.get("description")?,
        definition,
        resolution,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

pub struct New<'a> {
    pub corpus_id: &'a str,
    pub name: &'a str,
    pub owner: &'a str,
    pub visibility: Visibility,
    pub groups: &'a [String],
    pub description: Option<&'a str>,
    pub definition: &'a Definition,
}

/// Insert; Err when the owner already has a subcorpus of that name on the corpus.
pub fn insert(conn: &Connection, n: &New<'_>, res: &Resolution) -> Result<Subcorpus> {
    if get(conn, n.corpus_id, n.owner, n.name)?.is_some() {
        return Err(anyhow!(super::Refusal::conflict(format!("you already have a subcorpus '{}' on corpus '{}'", n.name, n.corpus_id))));
    }
    let t = now();
    conn.execute(
        "INSERT INTO subcorpora (corpus_id, name, owner, visibility, groups_json, description, definition_json,
            engine, tokens, ranges, encoding, resolved_against, resolved_at, subc_path, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15)",
        params![
            n.corpus_id,
            n.name,
            n.owner,
            n.visibility.as_str(),
            serde_json::to_string(n.groups)?,
            n.description,
            serde_json::to_string(n.definition)?,
            res.engine,
            res.tokens as i64,
            res.ranges as i64,
            res.encoding,
            res.against,
            res.at,
            res.subc_path,
            t
        ],
    )
    .context("storing the subcorpus")?;
    get(conn, n.corpus_id, n.owner, n.name)?.ok_or_else(|| anyhow!("the stored subcorpus is gone"))
}

pub fn set_resolution(conn: &Connection, id: i64, res: &Resolution) -> Result<()> {
    conn.execute(
        "UPDATE subcorpora SET engine = ?2, tokens = ?3, ranges = ?4, encoding = ?5, resolved_against = ?6,
            resolved_at = ?7, subc_path = ?8 WHERE id = ?1",
        params![id, res.engine, res.tokens as i64, res.ranges as i64, res.encoding, res.against, res.at, res.subc_path],
    )?;
    Ok(())
}

pub fn set_visibility(conn: &Connection, id: i64, vis: Visibility, groups: &[String], description: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE subcorpora SET visibility = ?2, groups_json = ?3, description = ?4, updated_at = ?5 WHERE id = ?1",
        params![id, vis.as_str(), serde_json::to_string(groups)?, description, now()],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, corpus_id: &str, owner: &str, name: &str) -> Result<Option<Subcorpus>> {
    Ok(conn
        .query_row(
            "SELECT * FROM subcorpora WHERE corpus_id = ?1 AND owner = ?2 AND name = ?3",
            params![corpus_id, owner, name],
            from_row,
        )
        .optional()?)
}

/// Every subcorpus of a corpus with this name (any owner).
pub fn by_name(conn: &Connection, corpus_id: &str, name: &str) -> Result<Vec<Subcorpus>> {
    let mut st = conn.prepare("SELECT * FROM subcorpora WHERE corpus_id = ?1 AND name = ?2 ORDER BY owner")?;
    let rows = st.query_map(params![corpus_id, name], from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn list(conn: &Connection, corpus_id: &str) -> Result<Vec<Subcorpus>> {
    let mut st = conn.prepare("SELECT * FROM subcorpora WHERE corpus_id = ?1 ORDER BY name, owner")?;
    let rows = st.query_map(params![corpus_id], from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM subcorpora WHERE id = ?1", params![id])?;
    Ok(())
}

impl Subcorpus {
    /// As the API shows it.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "corpus": self.corpus_id,
            "name": self.name,
            "owner": self.owner,
            "visibility": self.visibility.as_str(),
            "groups": self.groups,
            "description": self.description,
            "definition": self.definition,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        });
        if let Some(r) = &self.resolution {
            v["tokens"] = json!(r.tokens);
            v["ranges"] = json!(r.ranges);
            v["encoding"] = json!(r.encoding);
            v["resolved"] = json!({ "engine": r.engine, "against": r.against, "at": r.at });
        }
        v
    }
}

pub fn resolution_now(engine: &str, tokens: u64, ranges: u64, encoding: &str, against: &str, subc_path: Option<String>) -> Resolution {
    Resolution { engine: engine.into(), tokens, ranges, encoding: encoding.into(), against: against.into(), at: now(), subc_path }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def() -> Definition {
        Definition::parse(&json!({"kind": "regions", "struct": "text", "conditions": [{"attr": "genre", "value": "fiction"}]})).unwrap()
    }

    #[test]
    fn rows() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        init(&conn).unwrap();
        let d = def();
        let groups = vec!["acad".to_string()];
        let res = resolution_now("pando", 120, 3, "ranges", "idx1", None);
        let n = New { corpus_id: "c", name: "fiction", owner: "alice", visibility: Visibility::Private, groups: &[], description: Some("d"), definition: &d };
        let row = insert(&conn, &n, &res).unwrap();
        assert_eq!(row.definition, d);
        assert_eq!(row.resolution.as_ref().unwrap().tokens, 120);
        let e = insert(&conn, &n, &res).unwrap_err();
        assert_eq!(e.downcast_ref::<super::super::Refusal>().unwrap().status, 409, "unique per corpus, owner, name");
        let bob = New { owner: "bob", visibility: Visibility::Groups, groups: &groups, ..n };
        insert(&conn, &bob, &res).unwrap();
        assert_eq!(by_name(&conn, "c", "fiction").unwrap().len(), 2);
        assert_eq!(list(&conn, "c").unwrap().len(), 2);
        assert!(list(&conn, "other").unwrap().is_empty());
        let b = get(&conn, "c", "bob", "fiction").unwrap().unwrap();
        assert_eq!(b.visibility, Visibility::Groups);
        assert_eq!(b.groups, groups);
        set_visibility(&conn, b.id, Visibility::Public, &[], None).unwrap();
        set_resolution(&conn, b.id, &resolution_now("manatee", 7, 1, "ranges", "st", Some("/x.subc".into()))).unwrap();
        let b = get(&conn, "c", "bob", "fiction").unwrap().unwrap();
        assert_eq!(b.visibility, Visibility::Public);
        assert_eq!(b.resolution.as_ref().unwrap().subc_path.as_deref(), Some("/x.subc"));
        assert_eq!(b.to_json()["tokens"], 7);
        assert_eq!(b.to_json()["definition"]["kind"], "regions");
        delete(&conn, b.id).unwrap();
        assert_eq!(by_name(&conn, "c", "fiction").unwrap().len(), 1);
    }
}
