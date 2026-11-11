//! `POST /conc`: KonText's concordance operations on a native Manatee corpus, through stored
//! concordances in the Manatee workers (manatee_conc.rs; dev/MANATEE-SESSIONS.md). The pando
//! counterpart is `/run` (programs on a stored hit set); a Manatee corpus takes KonText's own
//! operation strings, which Manatee's `Concordance` runs as KonText does.
//!
//! Body: `corpus`, `cql` (Manatee CQL; a leading `MANATEE` is allowed), `default_attr`?, `ops`? (the
//! operations after the query: `s…`, `p…`, `n…`, `P…`, `N…`, `r…`, `f`, `D`, `F…`), `want`: `"size"` |
//! `"arf"` | `"page"` (`start`, `size`, `view` = `kwic_view`, `doc_attr`?, `arf`?) | `"freq"` (`crit`,
//! `limit`) | `"coll"` (`attr`, `sortfn`, `minfreq`, `minbgr`, `fromw`, `tow`, `max_items`, `bgrfns`) |
//! `"attr_sizes"` (`attr`); and the usual `subcorpus` / `subcorpus_mode` / `subcorpus_owner`,
//! `request_role`, `user`, `session_id` (as `/run`). Answer: `{"ok": true, "operation": "conc",
//! "result": {…}}` (the worker's JSON; `subcorpus` when one applied).

use crate::*;

pub(crate) async fn http_manatee_conc(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let corpus_id = req.get("corpus").and_then(Value::as_str).unwrap_or("").to_string();
    let cql = req.get("cql").and_then(Value::as_str).unwrap_or("").to_string();
    let mut rec = QueryRec::new("/conc", &corpus_id, &cql);
    let r = conc_inner(state.clone(), headers, req, &mut rec).await;
    rec.finish(&state, &r);
    r
}

async fn conc_inner(
    state: HttpAppState,
    headers: HeaderMap,
    mut req: Value,
    rec: &mut QueryRec,
) -> Result<Json<Value>, (StatusCode, String)> {
    let bad = |m: &str| (StatusCode::BAD_REQUEST, m.to_string());
    let s = |k: &str| req.get(k).and_then(Value::as_str).map(str::to_string);
    let corpus_id = s("corpus").filter(|c| !c.is_empty()).ok_or_else(|| bad("corpus is required"))?;
    let mut cql = s("cql").filter(|c| !c.trim().is_empty()).ok_or_else(|| bad("cql is required"))?;
    let session_id = s("session_id").map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
    let caller = state.limits.caller(
        &headers,
        s("request_role").as_deref(),
        s("user").as_deref(),
        session_id.as_deref(),
        &header_client_ip(&headers),
    );
    rec.caller(&caller);
    let corpus = state.catalog.get(&corpus_id).map_err(to_http_err)?;
    gate_corpus_role(&state, &corpus, &caller, "/conc")?;
    if resolve_effective_backend(&corpus).map_err(to_http_err)? != "manatee" {
        return Err(bad("/conc is for Manatee corpora (use /run for a pando corpus)"));
    }
    let pool = manatee_pool::global().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "native Manatee is not available (FQS_MANATEE_LIB)".to_string()))?;
    let registry = manatee_lib::registry_file(&corpus.settings, &corpus.id)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no Manatee registry for '{}'", corpus.id)))?;
    // the query's language: Manatee CQL (a language word is allowed, no other)
    if let Some((lang, rest)) = query_lang::split_prefix(&cql) {
        let effective = query_lang::effective(&corpus, "manatee", lang).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
        if effective != "manatee-cql" {
            return Err(bad(&format!("a Manatee corpus takes Manatee CQL here, not {effective}")));
        }
        cql = rest.to_string();
    }
    let applied = subcorpus::apply::for_request(
        &state, &corpus, &caller, Some("manatee"),
        req.get("subcorpus"), req.get("subcorpus_mode"), req.get("subcorpus_owner"),
    ).await?;
    let mut worker_req = serde_json::Map::new();
    for k in ["default_attr", "ops", "want", "start", "size", "view", "doc_attr", "arf", "crit", "limit", "attr",
              "sortfn", "minfreq", "minbgr", "fromw", "tow", "max_items", "bgrfns"] {
        if let Some(v) = req.get(k) {
            worker_req.insert(k.to_string(), v.clone());
        }
    }
    // `want` as a string or an object ({"kind": …}, its fields beside the others)
    if let Some(w) = worker_req.get("want").cloned() {
        if let Some(o) = w.as_object() {
            for (k, v) in o {
                if k != "kind" {
                    worker_req.insert(k.clone(), v.clone());
                }
            }
            worker_req.insert("want".into(), o.get("kind").cloned().unwrap_or(json!("size")));
        }
    }
    worker_req.insert("cql".into(), json!(cql));
    worker_req.insert("registry".into(), json!(registry));
    if worker_req.get("doc_attr").is_none() {
        if let Some(d) = corpus.settings.pointer("/fcs/doc_attr").and_then(Value::as_str).map(fcs::engine::manatee_struct_attr).or_else(|| manatee_lib::fullref(&registry)) {
            worker_req.insert("doc_attr".into(), json!(d));
        }
    }
    if let Some(subc) = applied.as_ref().and_then(|a| a.subc_file.as_ref()) {
        worker_req.insert("subc".into(), json!(subc));
    }
    let attr_sizes = worker_req.get("want").and_then(Value::as_str) == Some("attr_sizes");
    if attr_sizes {
        worker_req.insert("op".into(), json!("attr_sizes"));
    }
    let mut worker_req = Value::Object(worker_req);
    let t0 = Instant::now();
    let permit = state.limits.admit(&caller).await;
    rec.queued(t0);
    let permit = permit?;
    let id = corpus.id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if attr_sizes {
            pool.call(&id, std::mem::take(&mut worker_req))
        } else {
            pool.conc(&id, std::mem::take(&mut worker_req))
        }
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("worker join: {e}")))?
    .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    req = json!({ "ok": true, "operation": "conc", "result": result });
    if let Some(a) = &applied {
        req["subcorpus"] = a.report(None);
    }
    Ok(Json(req))
}
