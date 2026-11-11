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

/// Everything a native-Manatee request needs after the gates: the corpus, the caller, the pool, the
/// registry file and the stored subcorpus the request names.
pub(crate) struct Prepared {
    pub corpus: CorpusEntry,
    pub caller: limits::Caller,
    pub pool: Arc<manatee_pool::Pool>,
    pub registry: PathBuf,
    pub applied: Option<subcorpus::apply::Applied>,
}

pub(crate) async fn prepare(
    state: &HttpAppState,
    headers: &HeaderMap,
    req: &Value,
    rec: &mut QueryRec,
    route: &str,
) -> Result<Prepared, (StatusCode, String)> {
    let bad = |m: &str| (StatusCode::BAD_REQUEST, m.to_string());
    let s = |k: &str| req.get(k).and_then(Value::as_str).map(str::to_string);
    let corpus_id = s("corpus").filter(|c| !c.is_empty()).ok_or_else(|| bad("corpus is required"))?;
    let session_id = s("session_id").map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
    let caller = state.limits.caller(headers, s("request_role").as_deref(), s("user").as_deref(), session_id.as_deref(), &header_client_ip(headers));
    rec.caller(&caller);
    let corpus = state.catalog.get(&corpus_id).map_err(to_http_err)?;
    gate_corpus_role(state, &corpus, &caller, route)?;
    if resolve_effective_backend(&corpus).map_err(to_http_err)? != "manatee" {
        return Err(bad(&format!("{route} is for Manatee corpora (use /run for a pando corpus)")));
    }
    let pool = manatee_pool::global().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "native Manatee is not available (FQS_MANATEE_LIB)".to_string()))?;
    let registry = manatee_lib::registry_file(&corpus.settings, &corpus.id)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no Manatee registry for '{}'", corpus.id)))?;
    let applied = subcorpus::apply::for_request(
        state, &corpus, &caller, Some("manatee"),
        req.get("subcorpus"), req.get("subcorpus_mode"), req.get("subcorpus_owner"),
    ).await?;
    Ok(Prepared { corpus, caller, pool, registry, applied })
}

/// The worker request's base: registry and the subcorpus file.
pub(crate) fn base_request(p: &Prepared) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("registry".into(), json!(p.registry));
    if let Some(subc) = p.applied.as_ref().and_then(|a| a.subc_file.as_ref()) {
        m.insert("subc".into(), json!(subc));
    }
    m
}

/// Run a worker request off the async threads, behind the admission queue.
pub(crate) async fn run_worker(
    state: &HttpAppState,
    p: &Prepared,
    rec: &mut QueryRec,
    req: Value,
    stored: bool,
) -> Result<Value, (StatusCode, String)> {
    let t0 = Instant::now();
    let permit = state.limits.admit(&p.caller).await;
    rec.queued(t0);
    let permit = permit?;
    let (pool, id) = (p.pool.clone(), p.corpus.id.clone());
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if stored { pool.conc(&id, req) } else { pool.call(&id, req) }
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("worker join: {e}")))?
    .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))
}

fn answer(op: &str, result: Value, p: &Prepared) -> Json<Value> {
    let mut a = json!({ "ok": true, "operation": op, "result": result });
    if let Some(sc) = &p.applied {
        a["subcorpus"] = sc.report(None);
    }
    Json(a)
}

async fn conc_inner(
    state: HttpAppState,
    headers: HeaderMap,
    req: Value,
    rec: &mut QueryRec,
) -> Result<Json<Value>, (StatusCode, String)> {
    let bad = |m: &str| (StatusCode::BAD_REQUEST, m.to_string());
    let p = prepare(&state, &headers, &req, rec, "/conc").await?;
    let mut cql = req.get("cql").and_then(Value::as_str).filter(|c| !c.trim().is_empty()).ok_or_else(|| bad("cql is required"))?.to_string();
    // the query's language: Manatee CQL (a language word is allowed, no other)
    if let Some((lang, rest)) = query_lang::split_prefix(&cql) {
        let effective = query_lang::effective(&p.corpus, "manatee", lang).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
        if effective != "manatee-cql" {
            return Err(bad(&format!("a Manatee corpus takes Manatee CQL here, not {effective}")));
        }
        cql = rest.to_string();
    }
    let mut w = base_request(&p);
    for k in ["default_attr", "ops", "want", "start", "size", "view", "doc_attr", "arf", "crit", "limit", "attr",
              "sortfn", "minfreq", "minbgr", "fromw", "tow", "max_items", "bgrfns", "bins", "amplitude", "aligned"] {
        if let Some(v) = req.get(k) {
            w.insert(k.to_string(), v.clone());
        }
    }
    // `want` as a string or an object ({"kind": …}, its fields beside the others)
    if let Some(o) = w.get("want").cloned().and_then(|x| x.as_object().cloned()) {
        for (k, v) in &o {
            if k != "kind" {
                w.insert(k.clone(), v.clone());
            }
        }
        w.insert("want".into(), o.get("kind").cloned().unwrap_or(json!("size")));
    }
    w.insert("cql".into(), json!(cql));
    if !w.contains_key("doc_attr") {
        if let Some(d) = p.corpus.settings.pointer("/fcs/doc_attr").and_then(Value::as_str).map(fcs::engine::manatee_struct_attr).or_else(|| manatee_lib::fullref(&p.registry)) {
            w.insert("doc_attr".into(), json!(d));
        }
    }
    let attr_sizes = w.get("want").and_then(Value::as_str) == Some("attr_sizes");
    if attr_sizes {
        w.insert("op".into(), json!("attr_sizes"));
    }
    let mut result = run_worker(&state, &p, rec, Value::Object(w), !attr_sizes).await?;
    // aligned segments of a page, in the shape of /query's hits (manatee_aligned.rs)
    if let Some(hits) = result.get_mut("hits").and_then(Value::as_array_mut) {
        for h in hits {
            if let Some(al) = manatee_aligned::hit(h, &[]) {
                h["aligned"] = al;
            }
        }
    }
    Ok(answer("conc", result, &p))
}

/// `POST /wordlist`: a word list of an attribute (manatee/fqs_manatee_wordlist.cc). Body: `corpus`,
/// `attr`, `pattern`?, `nonword`? (the expression of non-words to leave out), `include_nonwords`? (false: leave out what the registry's NONWORDRE matches), `nums`? (`frq`, `docf`,
/// `arf`, `doc sizes`, `doc counts`), `minfreq`?, `pfilter`?, `nfilter`? (lists of words), and the
/// subcorpus fields. Answer `result`: `items` ([[word, frequency], …]) and `total`.
pub(crate) async fn http_manatee_wordlist(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut rec = QueryRec::new("/wordlist", req.get("corpus").and_then(Value::as_str).unwrap_or(""), req.get("attr").and_then(Value::as_str).unwrap_or(""));
    let r = wordlist_inner(&state, headers, req, &mut rec).await;
    rec.finish(&state, &r);
    r
}

async fn wordlist_inner(state: &HttpAppState, headers: HeaderMap, req: Value, rec: &mut QueryRec) -> Result<Json<Value>, (StatusCode, String)> {
    let p = prepare(state, &headers, &req, rec, "/wordlist").await?;
    let mut w = base_request(&p);
    w.insert("op".into(), json!("wordlist"));
    for k in ["attr", "pattern", "nonword", "include_nonwords", "nums", "minfreq", "pfilter", "nfilter"] {
        if let Some(v) = req.get(k) {
            w.insert(k.to_string(), v.clone());
        }
    }
    let result = run_worker(state, &p, rec, Value::Object(w), false).await?;
    Ok(answer("wordlist", result, &p))
}

/// `POST /structure`: the regions of a structure with the values of some attributes (text types).
/// Body: `corpus`, `name`, `attrs` (a list), `max_regions`?, `ranges`? (false: no positions); or `at`: a position, for the one region that contains it (`num`, `beg`, `end`). `result`: `size`, `begs`, `ends`, `attrs`.
pub(crate) async fn http_manatee_structure(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut rec = QueryRec::new("/structure", req.get("corpus").and_then(Value::as_str).unwrap_or(""), req.get("name").and_then(Value::as_str).unwrap_or(""));
    let r = structure_inner(&state, headers, req, &mut rec).await;
    rec.finish(&state, &r);
    r
}

async fn structure_inner(state: &HttpAppState, headers: HeaderMap, req: Value, rec: &mut QueryRec) -> Result<Json<Value>, (StatusCode, String)> {
    let p = prepare(state, &headers, &req, rec, "/structure").await?;
    let mut w = base_request(&p);
    // `at`: only the region containing that position (a structure of millions of regions)
    w.insert("op".into(), json!(if req.get("at").is_some() { "region_at" } else { "structure" }));
    if let Some(at) = req.get("at") {
        w.insert("pos".into(), at.clone());
    }
    for k in ["name", "attrs", "max_regions", "ranges"] {
        if let Some(v) = req.get(k) {
            w.insert(k.to_string(), v.clone());
        }
    }
    let result = run_worker(state, &p, rec, Value::Object(w), false).await?;
    Ok(answer("structure", result, &p))
}

/// `POST /keywords`: the keywords of `attr` in `corpus` against `ref_corpus` (both Manatee corpora of
/// this FQS; whole corpora). Body: `corpus`, `ref_corpus`, `attr`, `pattern`?, `include_nonwords`?,
/// `nums`?, `minfreq`? (of the word list), `simple_n`?, `max_items`?, `kw_minfreq`?, `kw_maxfreq`?.
/// `result`: `items` ([{str, score, freqs: [frq1, frq2, rel1, rel2]}]) and `totals`.
pub(crate) async fn http_manatee_keywords(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut rec = QueryRec::new("/keywords", req.get("corpus").and_then(Value::as_str).unwrap_or(""), req.get("attr").and_then(Value::as_str).unwrap_or(""));
    let r = keywords_inner(&state, headers, req, &mut rec).await;
    rec.finish(&state, &r);
    r
}

async fn keywords_inner(state: &HttpAppState, headers: HeaderMap, req: Value, rec: &mut QueryRec) -> Result<Json<Value>, (StatusCode, String)> {
    let p = prepare(state, &headers, &req, rec, "/keywords").await?;
    if p.applied.is_some() {
        return Err((StatusCode::BAD_REQUEST, "keywords of a subcorpus are not available yet (use the whole corpus)".to_string()));
    }
    let ref_id = req.get("ref_corpus").and_then(Value::as_str).filter(|c| !c.is_empty()).unwrap_or(&p.corpus.id).to_string();
    let ref_req = json!({ "corpus": ref_id, "request_role": req.get("request_role"), "user": req.get("user") });
    let rp = prepare(state, &headers, &ref_req, rec, "/keywords").await?;
    let mut w = base_request(&p);
    w.insert("op".into(), json!("keywords"));
    w.insert("ref_corpus".into(), json!(rp.corpus.id));
    w.insert("ref_registry".into(), json!(rp.registry));
    for k in ["attr", "pattern", "include_nonwords", "nums", "minfreq", "simple_n", "max_items", "kw_minfreq", "kw_maxfreq"] {
        if let Some(v) = req.get(k) {
            w.insert(k.to_string(), v.clone());
        }
    }
    let result = run_worker(state, &p, rec, Value::Object(w), false).await?;
    Ok(answer("keywords", result, &p))
}
