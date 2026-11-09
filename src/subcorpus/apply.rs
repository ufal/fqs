//! A request's subcorpus: `"subcorpus": "<name>"` on `/query`, `/run`, `/info`, with
//! `"subcorpus_mode"`: `use` (default: the subcorpus becomes the corpus, statistics relative to it)
//! or `within` (a filter, statistics of the whole corpus), and `"subcorpus_owner"` when the name is
//! not the caller's own and several users published one of that name.
//!
//! Hits are the same in both modes (only hits whose tokens all lie inside count); the modes differ
//! in what the statistics are relative to. pando gets the definition per request and computes the
//! statistics as the mode says. Manatee runs the query on the corpus opened as the `.subc`
//! (Manatee's SubCorpus); its answers carry counts, not relative values, so `within` is the same
//! query with the whole corpus as the base the front end divides by.
//!
//! Answers report it additively: `"subcorpus": {"name", "mode", "tokens", "owner"}` at the top level.

use super::policy;
use super::resolve::{self, Ctx};
use super::store::{self, Subcorpus};
use crate::limits::Caller;
use crate::{refuse, CorpusEntry, HttpAppState};
use anyhow::Result;
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Use,
    Within,
}

impl Mode {
    pub fn parse(s: Option<&str>) -> Result<Mode> {
        match s.map(str::trim).unwrap_or("") {
            "" | "use" => Ok(Mode::Use),
            "within" => Ok(Mode::Within),
            other => refuse!(400, "subcorpus_mode must be use or within, not '{other}'"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Use => "use",
            Mode::Within => "within",
        }
    }
}

/// What a request asked for.
#[derive(Clone, Debug, Default)]
pub struct Ask {
    pub name: String,
    pub owner: Option<String>,
    pub mode: Option<String>,
}

impl Ask {
    /// From a request's fields (`subcorpus` must be a name).
    pub fn from_fields(subcorpus: Option<&Value>, mode: Option<&Value>, owner: Option<&Value>) -> Result<Option<Ask>> {
        let name = match subcorpus {
            None | Some(Value::Null) => return Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => return Ok(None),
            Some(Value::String(s)) => s.trim().to_string(),
            Some(_) => refuse!(400, "'subcorpus' is the name of a stored subcorpus (GET /corpora/{{id}}/subcorpora lists them)"),
        };
        let s = |v: Option<&Value>| v.and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string);
        Ok(Some(Ask { name, owner: s(owner), mode: s(mode) }))
    }
}

/// A subcorpus ready for one request.
#[derive(Clone, Debug)]
pub struct Applied {
    pub name: String,
    pub owner: String,
    pub mode: Mode,
    pub tokens: u64,
    /// pando: the definition to send
    pub pando_definition: Option<String>,
    /// Manatee: the .subc to run under; CWB: the .subc beside CQP's saved subcorpus
    pub subc_file: Option<String>,
}

impl Applied {
    /// pando's per-request field.
    pub fn pando_field(&self) -> Value {
        json!({ "definition": self.pando_definition, "mode": self.mode.as_str(), "name": self.name })
    }

    /// The answer's `subcorpus` field; `engine_answer`: the engine's own report, whose `tokens`
    /// wins (counted on the index the request ran on).
    pub fn report(&self, engine_answer: Option<&Value>) -> Value {
        let tokens = engine_answer.and_then(|s| s.get("tokens")).and_then(Value::as_u64).unwrap_or(self.tokens);
        json!({ "name": self.name, "mode": self.mode.as_str(), "tokens": tokens, "owner": self.owner })
    }
}

/// The subcorpus `ask.name` of corpus `corpus_id` as `caller` may use it: theirs, else
/// `ask.owner`'s, else the only one of that name they can see.
pub fn lookup_visible(conn: &rusqlite::Connection, corpus_id: &str, ask: &Ask, caller: &Caller) -> Result<Subcorpus> {
    let candidates: Vec<Subcorpus> = match (&ask.owner, policy::owner_of(caller)) {
        (Some(o), _) => store::get(conn, corpus_id, o, &ask.name)?.into_iter().collect(),
        (None, Some(me)) if store::get(conn, corpus_id, &me, &ask.name)?.is_some() => store::get(conn, corpus_id, &me, &ask.name)?.into_iter().collect(),
        _ => store::by_name(conn, corpus_id, &ask.name)?,
    };
    let mut visible: Vec<Subcorpus> = candidates.into_iter().filter(|s| policy::may_see(&s.owner, s.visibility, &s.groups, caller)).collect();
    match visible.len() {
        0 => refuse!(404, "no subcorpus '{}' on corpus '{corpus_id}' that you can use", ask.name),
        1 => Ok(visible.remove(0)),
        _ => refuse!(
            409,
            "several subcorpora '{}' on corpus '{corpus_id}' (owners: {}): name one with subcorpus_owner",
            ask.name,
            visible.iter().map(|s| s.owner.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Prepare `ask` for a request on `corpus` (engine: the request's backend override or the corpus's).
/// Blocking (database, maybe a re-resolution): call it off the async threads.
pub fn prepare(state: &HttpAppState, corpus: &CorpusEntry, caller: &Caller, backend: Option<&str>, ask: &Ask) -> Result<Applied> {
    let mode = Mode::parse(ask.mode.as_deref())?;
    let engine = resolve::engine(corpus, backend)?;
    let conn = crate::open_db(&state.db_path)?;
    let s = lookup_visible(&conn, &corpus.id, ask, caller)?;
    let ctx = Ctx { state, corpus, engine: engine.clone(), conn: &conn };
    let s = ctx.fresh(s)?;
    let res = s.resolution.clone().unwrap_or_else(|| store::resolution_now(&engine, 0, 0, "", "", None));
    let pando_definition = (engine == "pando").then(|| ctx.pando_definition(&s.owner, &s.definition)).transpose()?;
    Ok(Applied {
        name: s.name,
        owner: s.owner,
        mode,
        tokens: res.tokens,
        pando_definition,
        subc_file: res.subc_path,
    })
}

/// A request's subcorpus fields (`subcorpus`, `subcorpus_mode`, `subcorpus_owner`), prepared off
/// the async threads; None when the request names none.
pub async fn for_request(
    state: &HttpAppState,
    corpus: &CorpusEntry,
    caller: &Caller,
    backend: Option<&str>,
    subcorpus: Option<&Value>,
    mode: Option<&Value>,
    owner: Option<&Value>,
) -> std::result::Result<Option<Applied>, (axum::http::StatusCode, String)> {
    let Some(ask) = Ask::from_fields(subcorpus, mode, owner).map_err(super::api::http_err)? else { return Ok(None) };
    let (state, corpus, caller, backend) = (state.clone(), corpus.clone(), caller.clone(), backend.map(str::to_string));
    tokio::task::spawn_blocking(move || prepare(&state, &corpus, &caller, backend.as_deref(), &ask))
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("worker join: {e}")))?
        .map(Some)
        .map_err(super::api::http_err)
}

/// `/info` under a subcorpus: `subcorpus` added; with `use` the sizes are the subcorpus's
/// (`size`, `tokens` of the result) and the whole corpus's stays in `subcorpus.corpus_tokens`.
pub fn info(mut answer: Value, applied: &Applied) -> Value {
    let mut report = applied.report(None);
    let result = answer.get_mut("result").filter(|r| r.is_object());
    if let Some(r) = result {
        let whole = r.get("size").cloned().or_else(|| r.get("tokens").cloned()).unwrap_or(Value::Null);
        report["corpus_tokens"] = whole;
        if applied.mode == Mode::Use {
            for k in ["size", "tokens"] {
                if r.get(k).is_some() {
                    r[k] = json!(applied.tokens);
                }
            }
        }
    }
    if let Some(o) = answer.as_object_mut() {
        o.insert("subcorpus".into(), report);
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subcorpus::definition::Definition;
    use crate::subcorpus::policy::Visibility;
    use rusqlite::Connection;

    fn caller(role: &str, user: &str, groups: &[&str]) -> Caller {
        Caller { role: role.into(), tier: role.into(), user: user.into(), verified: true, groups: groups.iter().map(|g| g.to_string()).collect(), owned_groups: vec![] }
    }

    #[test]
    fn asks_and_modes() {
        assert!(Ask::from_fields(None, None, None).unwrap().is_none());
        assert!(Ask::from_fields(Some(&json!("")), None, None).unwrap().is_none());
        let a = Ask::from_fields(Some(&json!(" fic ")), Some(&json!("within")), Some(&json!("bob"))).unwrap().unwrap();
        assert_eq!((a.name.as_str(), a.owner.as_deref(), a.mode.as_deref()), ("fic", Some("bob"), Some("within")));
        assert!(Ask::from_fields(Some(&json!({"definition": "x"})), None, None).is_err(), "a name, not a definition");
        assert_eq!(Mode::parse(None).unwrap(), Mode::Use);
        assert_eq!(Mode::parse(Some("within")).unwrap(), Mode::Within);
        assert!(Mode::parse(Some("into")).is_err());
    }

    #[test]
    fn lookup_rules() {
        let conn = Connection::open_in_memory().unwrap();
        store::init(&conn).unwrap();
        let d = Definition::parse(&json!({"kind": "regions", "struct": "text"})).unwrap();
        let res = store::resolution_now("pando", 5, 1, "ranges", "i", None);
        let groups = vec!["acad".to_string()];
        for (owner, vis, g) in [("alice", Visibility::Private, &[][..]), ("bob", Visibility::Groups, &groups[..]), ("carol", Visibility::Public, &[][..])] {
            store::insert(&conn, &store::New { corpus_id: "c", name: "fic", owner, visibility: vis, groups: g, description: None, definition: &d }, &res).unwrap();
        }
        let ask = |owner: Option<&str>| Ask { name: "fic".into(), owner: owner.map(str::to_string), mode: None };
        let status = |r: Result<Subcorpus>| r.unwrap_err().downcast::<crate::subcorpus::Refusal>().unwrap().status;
        assert_eq!(lookup_visible(&conn, "c", &ask(None), &caller("user", "alice", &[])).unwrap().owner, "alice", "own first");
        assert_eq!(lookup_visible(&conn, "c", &ask(None), &caller("user", "dan", &[])).unwrap().owner, "carol", "the only visible one");
        assert_eq!(status(lookup_visible(&conn, "c", &ask(None), &caller("user", "dan", &["acad"]))), 409, "bob's and carol's");
        assert_eq!(lookup_visible(&conn, "c", &ask(Some("bob")), &caller("user", "dan", &["acad"])).unwrap().owner, "bob");
        assert_eq!(status(lookup_visible(&conn, "c", &ask(Some("alice")), &caller("user", "dan", &[]))), 404, "private");
        assert_eq!(status(lookup_visible(&conn, "other", &ask(None), &caller("user", "dan", &[]))), 404);
    }

    #[test]
    fn info_under_a_subcorpus() {
        let a = Applied { name: "fic".into(), owner: "alice".into(), mode: Mode::Use, tokens: 40, pando_definition: None, subc_file: None };
        let v = info(json!({"ok": true, "result": {"size": 1000, "tokens": 1000, "attributes": []}}), &a);
        assert_eq!(v["result"]["size"], 40);
        assert_eq!(v["result"]["tokens"], 40);
        assert_eq!(v["subcorpus"], json!({"name": "fic", "mode": "use", "tokens": 40, "owner": "alice", "corpus_tokens": 1000}));
        let w = Applied { mode: Mode::Within, ..a.clone() };
        let v = info(json!({"ok": true, "result": {"size": 1000}}), &w);
        assert_eq!(v["result"]["size"], 1000, "within: the corpus's statistics");
        assert_eq!(a.pando_field(), json!({"definition": null, "mode": "use", "name": "fic"}));
        assert_eq!(a.report(Some(&json!({"tokens": 41}))), json!({"name": "fic", "mode": "use", "tokens": 41, "owner": "alice"}));
    }
}
