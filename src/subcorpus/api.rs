//! The subcorpus routes (public bind; the caller is who the request's token says, as for `/query`):
//!
//! - `GET    /corpora/{id}/subcorpora`          the ones the caller may use (`?owner=` only theirs)
//! - `POST   /corpora/{id}/subcorpora`          {"name", "definition", "visibility"?, "groups"?,
//!                                               "description"?, "backend"?} → 201 + the row (resolved once)
//! - `GET    /corpora/{id}/subcorpora/{name}`   one (`?owner=` when several are visible)
//! - `PATCH  /corpora/{id}/subcorpora/{name}`   {"visibility"?, "groups"?, "description"?} (owner, admin)
//! - `DELETE /corpora/{id}/subcorpora/{name}`   (owner, admin; refused while an expression uses it)
//!
//! Without a JWT secret (local use), `user` / `request_role` in the body or query string are
//! believed, as on `/query`.

use super::apply::{lookup_visible, Ask};
use super::definition::{valid_name, Definition, Expr};
use super::policy::{self, Publish, Visibility};
use super::resolve::{self, Ctx};
use super::store::{self, Subcorpus};
use super::Refusal;
use crate::limits::Caller;
use crate::{CorpusEntry, HttpAppState};
use anyhow::{anyhow, Result};
use axum::extract::{Path as AxumPath, Query as AxumQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

type HttpResult = std::result::Result<(StatusCode, Json<Value>), (StatusCode, String)>;

pub fn http_err(e: anyhow::Error) -> (StatusCode, String) {
    if let Some(r) = e.downcast_ref::<Refusal>() {
        return (StatusCode::from_u16(r.status).unwrap_or(StatusCode::BAD_REQUEST), r.msg.clone());
    }
    crate::to_http_err(e)
}

#[derive(Debug, Default, Deserialize)]
pub struct Who {
    pub owner: Option<String>,
    pub user: Option<String>,
    pub request_role: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateReq {
    pub name: String,
    pub definition: Value,
    pub visibility: Option<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    pub description: Option<String>,
    /// the engine to resolve on, when the corpus has several (pando | manatee)
    pub backend: Option<String>,
    pub user: Option<String>,
    pub request_role: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchReq {
    pub visibility: Option<String>,
    pub groups: Option<Vec<String>>,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub user: Option<String>,
    pub request_role: Option<String>,
}

/// The caller, and the corpus if they may use it.
fn gate(state: &HttpAppState, headers: &HeaderMap, corpus_id: &str, role: Option<&str>, user: Option<&str>, route: &str) -> std::result::Result<(CorpusEntry, Caller), (StatusCode, String)> {
    let corpus = state.catalog.get(corpus_id).map_err(crate::to_http_err)?;
    let caller = state.limits.caller(headers, role, user, None, &crate::header_client_ip(headers));
    crate::gate_corpus_role(state, &corpus, &caller, route)?;
    Ok((corpus, caller))
}

fn publish_policy(state: &HttpAppState) -> Publish {
    Publish::from_config(state.limits.config())
}

fn parse_visibility(v: Option<&str>) -> Result<Visibility> {
    match v {
        None => Ok(Visibility::Private),
        Some(s) => Visibility::parse(s).ok_or_else(|| anyhow!(Refusal::bad(format!("visibility must be private, groups or public, not '{s}'")))),
    }
}

fn clean_groups(groups: &[String]) -> Vec<String> {
    let mut g: Vec<String> = groups.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    g.sort();
    g.dedup();
    g
}

/// Blocking work off the async threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> std::result::Result<T, (StatusCode, String)> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("worker join: {e}")))?
        .map_err(http_err)
}

pub async fn list(State(state): State<HttpAppState>, headers: HeaderMap, AxumPath(id): AxumPath<String>, AxumQuery(who): AxumQuery<Who>) -> HttpResult {
    let (corpus, caller) = gate(&state, &headers, &id, who.request_role.as_deref(), who.user.as_deref(), "/corpora/subcorpora")?;
    let rows = blocking(move || {
        let conn = crate::open_db(&state.db_path)?;
        Ok(store::list(&conn, &corpus.id)?
            .into_iter()
            .filter(|s| policy::may_see(&s.owner, s.visibility, &s.groups, &caller))
            .filter(|s| who.owner.as_deref().is_none_or(|o| o == s.owner))
            .map(|s| s.to_json())
            .collect::<Vec<_>>())
    })
    .await?;
    Ok((StatusCode::OK, Json(json!({ "ok": true, "corpus": id, "subcorpora": rows }))))
}

pub async fn create(State(state): State<HttpAppState>, headers: HeaderMap, AxumPath(id): AxumPath<String>, Json(req): Json<CreateReq>) -> HttpResult {
    let (corpus, caller) = gate(&state, &headers, &id, req.request_role.as_deref(), req.user.as_deref(), "/corpora/subcorpora")?;
    let policy = publish_policy(&state);
    let (row, warnings) = blocking(move || create_blocking(&state, &corpus, &caller, policy, req)).await?;
    let mut v = row.to_json();
    v["ok"] = json!(true);
    if !warnings.is_empty() {
        v["warnings"] = json!(warnings);
    }
    Ok((StatusCode::CREATED, Json(v)))
}

fn create_blocking(state: &HttpAppState, corpus: &CorpusEntry, caller: &Caller, policy: Publish, req: CreateReq) -> Result<(Subcorpus, Vec<String>)> {
    let Some(owner) = policy::owner_of(caller) else {
        return Err(anyhow!(Refusal::forbidden("sign in to create a subcorpus (it needs an owner)")));
    };
    let name = req.name.trim();
    if !valid_name(name) {
        return Err(anyhow!(Refusal::bad("a subcorpus name is 1-64 letters, digits, '_' or '.'")));
    }
    let visibility = parse_visibility(req.visibility.as_deref())?;
    let groups = if visibility == Visibility::Groups { clean_groups(&req.groups) } else { Vec::new() };
    policy::check_publish(policy, visibility, &groups, caller).map_err(|e| anyhow!(Refusal::forbidden(format!("{e:#}"))))?;
    let def = Definition::parse(&req.definition).map_err(|e| anyhow!(Refusal::bad(format!("{e:#}"))))?;
    if let Definition::Expression { expression } = &def {
        if Expr::parse(expression)?.names().iter().any(|n| n == name) {
            return Err(anyhow!(Refusal::bad(format!("the expression of '{name}' uses '{name}' itself"))));
        }
    }
    let engine = resolve::engine(corpus, req.backend.as_deref())?;
    let conn = crate::open_db(&state.db_path)?;
    if store::get(&conn, &corpus.id, &owner, name)?.is_some() {
        return Err(anyhow!(Refusal::conflict(format!("you already have a subcorpus '{name}' on corpus '{}'", corpus.id))));
    }
    let ctx = Ctx { state, corpus, engine, conn: &conn };
    let def = ctx.normalize(&owner, def)?;
    let (res, warnings) = ctx.resolve(&owner, name, &def)?;
    let new = store::New { corpus_id: &corpus.id, name, owner: &owner, visibility, groups: &groups, description: req.description.as_deref(), definition: &def };
    Ok((store::insert(&conn, &new, &res)?, warnings))
}

pub async fn show(State(state): State<HttpAppState>, headers: HeaderMap, AxumPath((id, name)): AxumPath<(String, String)>, AxumQuery(who): AxumQuery<Who>) -> HttpResult {
    let (corpus, caller) = gate(&state, &headers, &id, who.request_role.as_deref(), who.user.as_deref(), "/corpora/subcorpora")?;
    let row = blocking(move || {
        let conn = crate::open_db(&state.db_path)?;
        lookup_visible(&conn, &corpus.id, &Ask { name, owner: who.owner, mode: None }, &caller)
    })
    .await?;
    let mut v = row.to_json();
    v["ok"] = json!(true);
    Ok((StatusCode::OK, Json(v)))
}

/// The subcorpus `name` of corpus `id` the caller may change: theirs (or `owner`'s, for an admin).
fn managed(conn: &rusqlite::Connection, corpus_id: &str, name: &str, owner: Option<&str>, caller: &Caller) -> Result<Subcorpus> {
    let s = lookup_visible(conn, corpus_id, &Ask { name: name.to_string(), owner: owner.map(str::to_string), mode: None }, caller)?;
    if !policy::may_manage(&s.owner, caller) {
        return Err(anyhow!(Refusal::forbidden(format!("subcorpus '{name}' is {}'s: only they or an admin may change it", s.owner))));
    }
    Ok(s)
}

pub async fn patch(State(state): State<HttpAppState>, headers: HeaderMap, AxumPath((id, name)): AxumPath<(String, String)>, Json(req): Json<PatchReq>) -> HttpResult {
    let (corpus, caller) = gate(&state, &headers, &id, req.request_role.as_deref(), req.user.as_deref(), "/corpora/subcorpora")?;
    let policy = publish_policy(&state);
    let row = blocking(move || {
        let conn = crate::open_db(&state.db_path)?;
        let s = managed(&conn, &corpus.id, &name, req.owner.as_deref(), &caller)?;
        let visibility = match req.visibility.as_deref() {
            Some(v) => parse_visibility(Some(v))?,
            None => s.visibility,
        };
        let groups = if visibility == Visibility::Groups { clean_groups(req.groups.as_ref().unwrap_or(&s.groups)) } else { Vec::new() };
        // withdrawing is always allowed; publishing more widely follows the current rule
        let widens = visibility != s.visibility || groups.iter().any(|g| !s.groups.contains(g));
        if widens {
            policy::check_publish(policy, visibility, &groups, &caller).map_err(|e| anyhow!(Refusal::forbidden(format!("{e:#}"))))?;
        }
        let description = req.description.or(s.description.clone());
        store::set_visibility(&conn, s.id, visibility, &groups, description.as_deref())?;
        store::get(&conn, &corpus.id, &s.owner, &s.name)?.ok_or_else(|| anyhow!("the subcorpus is gone"))
    })
    .await?;
    let mut v = row.to_json();
    v["ok"] = json!(true);
    Ok((StatusCode::OK, Json(v)))
}

pub async fn delete(State(state): State<HttpAppState>, headers: HeaderMap, AxumPath((id, name)): AxumPath<(String, String)>, AxumQuery(who): AxumQuery<Who>) -> HttpResult {
    let (corpus, caller) = gate(&state, &headers, &id, who.request_role.as_deref(), who.user.as_deref(), "/corpora/subcorpora")?;
    let gone = blocking(move || {
        let conn = crate::open_db(&state.db_path)?;
        let s = managed(&conn, &corpus.id, &name, who.owner.as_deref(), &caller)?;
        let users: Vec<String> = store::list(&conn, &corpus.id)?
            .into_iter()
            .filter(|o| o.id != s.id)
            .filter(|o| match &o.definition {
                Definition::Expression { expression } => {
                    let named = Expr::parse(expression).map(|e| e.names()).unwrap_or_default();
                    // a name in o's expression means s when it is o's owner's own or s is public and o's owner has none
                    named.contains(&s.name) && (o.owner == s.owner || (s.visibility == Visibility::Public && store::get(&conn, &corpus.id, &o.owner, &s.name).ok().flatten().is_none()))
                }
                _ => false,
            })
            .map(|o| format!("{} ({})", o.name, o.owner))
            .collect();
        if !users.is_empty() {
            return Err(anyhow!(Refusal::conflict(format!("subcorpus '{}' is used by {}: delete or change those first", s.name, users.join(", ")))));
        }
        store::delete(&conn, s.id)?;
        if let Some(p) = s.resolution.as_ref().and_then(|r| r.subc_path.as_deref()) {
            let _ = std::fs::remove_file(p);
        }
        Ok(s.to_json())
    })
    .await?;
    Ok((StatusCode::OK, Json(json!({ "ok": true, "deleted": gone }))))
}
