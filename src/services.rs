//! Inventory of query backends and UI frontends for the admin console.
//!
//! Backends = engines FQS can drive (pando, cqp, flexicorp, …) with install/health probes.
//! Frontends = UIs that sit in front (KonText, TEITOK, FCS clients, …) discovered from
//! catalog settings + optional `frontends` in `/etc/fqs/fqs.json`. Restart actions are
//! only those explicitly configured (never arbitrary shell from the browser).

use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub fn probe_backends(pando_status: Value) -> Value {
    let mut backends = Vec::new();

    let pando_hot = pando_status.get("available").and_then(|v| v.as_bool()) != Some(false)
        && (pando_status.get("api_version").is_some() || pando_status.get("engine").is_some());
    backends.push(json!({
        "id": "pando",
        "label": "Pando (in-process libflexicorp_pando)",
        "role": "query_engine",
        "installed": pando_hot,
        "healthy": pando_hot,
        "version": pando_status.get("engine").and_then(|e| e.get("version"))
            .or_else(|| pando_status.get("engine_build"))
            .cloned()
            .unwrap_or(Value::Null),
        "detail": pando_status,
        "notes": if pando_hot {
            vec!["Hot path via HotCorpusManager"]
        } else {
            vec!["Library not loaded; cold CLI may still work"]
        },
    }));

    let pando_cli = which_bin(&[
        std::env::var("PANDO_BINARY").ok().as_deref(),
        Some("pando"),
        Some("flexicorp-pando"),
    ]);
    let pando_cli_ver = pando_cli
        .as_ref()
        .and_then(|p| version_from_cmd(p, &["--version"]));
    backends.push(json!({
        "id": "pando_cli",
        "label": "Pando / flexicorp-pando CLI",
        "role": "query_engine",
        "installed": pando_cli.is_some(),
        "healthy": pando_cli.is_some(),
        "path": pando_cli,
        "version": pando_cli_ver,
        "notes": ["Used when hot lib is missing or --pando-cli-only"],
    }));

    let cqp = which_bin(&[
        std::env::var("CQP_BINARY").ok().as_deref(),
        Some("cqp"),
    ]);
    // Classic CWB uses `-v` (copyright banner, often starts with a blank line).
    // Newer builds may also accept `--version`.
    let cqp_ver = cqp.as_ref().and_then(|p| {
        version_from_cmd(p, &["--version"]).or_else(|| version_from_cmd(p, &["-v"]))
    });
    backends.push(json!({
        "id": "cqp",
        "label": "CQP / CWB",
        "role": "query_engine",
        "installed": cqp.is_some(),
        "healthy": cqp_ver.is_some(),
        "path": cqp,
        "version": cqp_ver,
        "notes": ["Child-process searches take a process slot"],
    }));

    let py = which_bin(&[Some("python3"), Some("python")]);
    let flexicorp = py.as_ref().and_then(|p| {
        let out = Command::new(p)
            .args(["-m", "flexicorp", "--help"])
            .output()
            .ok()?;
        if out.status.success() {
            Some(true)
        } else {
            None
        }
    });
    let flexicorp_ver = py.as_ref().and_then(|p| {
        let out = Command::new(p)
            .args(["-c", "import flexicorp,sys; print(getattr(flexicorp,'__version__', 'unknown'))"])
            .output()
            .ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            None
        }
    });
    backends.push(json!({
        "id": "flexicorp",
        "label": "Python flexicorp",
        "role": "indexer_query_bridge",
        "installed": flexicorp.is_some(),
        "healthy": flexicorp.is_some(),
        "path": py.clone(),
        "version": flexicorp_ver,
        "notes": ["Reindex + TEITOK/CQP enrichment path"],
    }));

    let manatee = py.as_ref().and_then(|p| {
        let out = Command::new(p)
            .args(["-c", "import manatee; print(getattr(manatee,'__version__', 'ok'))"])
            .output()
            .ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            None
        }
    });
    backends.push(json!({
        "id": "manatee",
        "label": "Manatee (Python bindings)",
        "role": "query_engine",
        "installed": manatee.is_some(),
        "healthy": manatee.is_some(),
        "version": manatee,
        "notes": ["Often used via KonText rather than direct FQS; listed when importable"],
    }));

    json!({
        "ok": true,
        "backends": backends,
    })
}

/// Build frontend inventory: every kind FQS can handle, with status + instances.
///
/// Centralized UIs (KonText, CQPweb, Korp, …) nest the corpora they serve.
/// Per-corpus UIs (TEITOK) list related corpora for visibility only — not one shared service.
pub fn probe_frontends(catalog_hints: &[FrontendHint]) -> Value {
    let mut instances: Map<String, Value> = Map::new();

    for (id, mut obj) in configured_frontends() {
        if let Some(o) = obj.as_object_mut() {
            let kind = o
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let kind = normalize_frontend_kind(&kind);
            o.insert("kind".into(), json!(kind));
            o.entry("centralized")
                .or_insert(json!(is_centralized_kind(&kind)));
            o.entry("corpora").or_insert(json!([]));
        }
        instances.insert(id, obj);
    }

    // Per-kind corpus bags for non-centralized kinds (e.g. TEITOK).
    let mut kind_corpora: Map<String, Value> = Map::new();

    for hint in catalog_hints {
        if hint.centralized {
            let target_id = resolve_frontend_merge_id(&instances, hint);
            let entry = instances.entry(target_id.clone()).or_insert_with(|| {
                json!({
                    "id": target_id,
                    "kind": hint.kind,
                    "label": hint.label,
                    "url": hint.url,
                    "source": "catalog",
                    "centralized": true,
                    "restartable": false,
                    "corpora": [],
                })
            });
            if let Some(o) = entry.as_object_mut() {
                o.insert("centralized".into(), json!(true));
                o.entry("kind").or_insert(json!(hint.kind));
                if o.get("url").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                    if let Some(u) = &hint.url {
                        o.insert("url".into(), json!(u));
                    }
                }
                o.entry("corpora").or_insert(json!([]));
                if let Some(arr) = o.get_mut("corpora").and_then(|v| v.as_array_mut()) {
                    if !arr.iter().any(|x| x.as_str() == Some(hint.corpus_id.as_str())) {
                        arr.push(json!(hint.corpus_id));
                    }
                }
                if let Some(alias) = &hint.corpus_alias {
                    o.entry("corpus_aliases").or_insert(json!({}));
                    if let Some(map) = o.get_mut("corpus_aliases").and_then(|v| v.as_object_mut()) {
                        map.insert(hint.corpus_id.clone(), json!(alias));
                    }
                }
                let src = o.get("source").and_then(|v| v.as_str()).unwrap_or("");
                o.insert(
                    "source".into(),
                    json!(if src.starts_with("fqs.json") {
                        "fqs.json+catalog"
                    } else {
                        "catalog"
                    }),
                );
            }
        } else {
            let kind = normalize_frontend_kind(&hint.kind);
            let arr = kind_corpora.entry(kind).or_insert(json!([]));
            if let Some(a) = arr.as_array_mut() {
                if !a.iter().any(|x| x.as_str() == Some(hint.corpus_id.as_str())) {
                    a.push(json!(hint.corpus_id));
                }
            }
        }
    }

    let gunicorn = discover_gunicorn();

    // Finalize instances (health, restartable, sorted corpora).
    let mut instance_list = Vec::new();
    for (_, mut v) in instances {
        if let Some(o) = v.as_object_mut() {
            if let Some(arr) = o.get_mut("corpora").and_then(|v| v.as_array_mut()) {
                arr.sort_by(|a, b| {
                    a.as_str()
                        .unwrap_or("")
                        .cmp(b.as_str().unwrap_or(""))
                });
            }
            let url = o
                .get("health_url")
                .or_else(|| o.get("url"))
                .and_then(|u| u.as_str())
                .map(str::to_string);
            let health = if o.get("kind").and_then(|v| v.as_str()) == Some("fcs") && url.is_none()
            {
                json!({ "ok": true, "note": "served by this FQS /fcs endpoint" })
            } else {
                url.as_ref().map(|u| http_probe(u)).unwrap_or(json!({
                    "ok": false,
                    "error": "no url"
                }))
            };
            o.insert("health".into(), health);
            let restartable = o.get("restart").and_then(|r| r.as_object()).is_some();
            o.insert("restartable".into(), json!(restartable));
            let kind = o
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            o.entry("centralized")
                .or_insert(json!(is_centralized_kind(&kind)));
        }
        instance_list.push(v);
    }
    instance_list.sort_by(|a, b| {
        let ai = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let bi = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
        ai.cmp(bi)
    });

    // Known kinds FQS can handle (FCS hit_link presets + catalog integrations).
    let mut kinds = Vec::new();
    for spec in handled_frontend_kinds() {
        let kind_id = spec.id;
        let mut kind_instances: Vec<Value> = instance_list
            .iter()
            .filter(|i| {
                i.get("kind").and_then(|v| v.as_str()).map(normalize_frontend_kind)
                    == Some(kind_id.to_string())
            })
            .cloned()
            .collect();

        let mut corpora_for_kind: Vec<String> = Vec::new();
        if let Some(arr) = kind_corpora.get(kind_id).and_then(|v| v.as_array()) {
            for c in arr {
                if let Some(s) = c.as_str() {
                    corpora_for_kind.push(s.to_string());
                }
            }
        }
        for inst in &kind_instances {
            if let Some(arr) = inst.get("corpora").and_then(|v| v.as_array()) {
                for c in arr {
                    if let Some(s) = c.as_str() {
                        if !corpora_for_kind.iter().any(|x| x == s) {
                            corpora_for_kind.push(s.to_string());
                        }
                    }
                }
            }
        }
        corpora_for_kind.sort();

        let any_healthy = kind_instances.iter().any(|i| {
            i.get("health")
                .and_then(|h| h.get("ok"))
                .and_then(|v| v.as_bool())
                == Some(true)
        });
        let status = if !kind_instances.is_empty() {
            if any_healthy {
                "healthy"
            } else if kind_instances.iter().any(|i| i.get("url").is_some()) {
                "configured"
            } else {
                "present"
            }
        } else if !corpora_for_kind.is_empty() {
            if spec.centralized {
                "catalog_only"
            } else {
                "in_use"
            }
        } else {
            "not_configured"
        };

        // For non-centralized kinds without instances, still expose corpora list on the kind.
        if !spec.centralized && kind_instances.is_empty() && !corpora_for_kind.is_empty() {
            kind_instances.push(json!({
                "id": format!("{kind_id}:catalog"),
                "kind": kind_id,
                "label": format!("{} (per-corpus projects)", spec.label),
                "source": "catalog",
                "centralized": false,
                "restartable": false,
                "corpora": corpora_for_kind,
                "health": { "ok": true, "note": "per-corpus UI — no single shared health URL" },
            }));
        }

        kinds.push(json!({
            "id": kind_id,
            "label": spec.label,
            "centralized": spec.centralized,
            "handled": true,
            "status": status,
            "notes": spec.notes,
            "corpus_count": corpora_for_kind.len(),
            "corpora": if spec.centralized {
                // Prefer listing under instances; keep kind-level union for summary.
                Value::Array(corpora_for_kind.iter().cloned().map(Value::String).collect())
            } else {
                Value::Array(corpora_for_kind.iter().cloned().map(Value::String).collect())
            },
            "instances": kind_instances,
        }));
    }

    // Any configured/discovered instance whose kind is not in the handled list.
    let known: Vec<&str> = handled_frontend_kinds().iter().map(|k| k.id).collect();
    let other: Vec<Value> = instance_list
        .iter()
        .filter(|i| {
            let k = i
                .get("kind")
                .and_then(|v| v.as_str())
                .map(normalize_frontend_kind)
                .unwrap_or_else(|| "unknown".into());
            !known.iter().any(|n| *n == k)
        })
        .cloned()
        .collect();
    if !other.is_empty() {
        kinds.push(json!({
            "id": "other",
            "label": "Other / custom",
            "centralized": true,
            "handled": false,
            "status": "configured",
            "notes": "Configured in fqs.json or catalog but not a built-in FQS frontend preset.",
            "corpus_count": 0,
            "corpora": [],
            "instances": other,
        }));
    }

    json!({
        "ok": true,
        "kinds": kinds,
        "frontends": instance_list,
        "gunicorn_processes": gunicorn,
        "restart_policy": "Only frontends with a configured restart block in fqs.json can be restarted from the admin UI.",
        "corpora_note": "Centralized frontends nest the corpora they serve. TEITOK and similar per-corpus UIs list related catalog entries under the kind, not as one shared service.",
    })
}

/// Catalog row summary attached to frontend corpora lists (admin UI tables).
pub type CorpusDetailIndex = Map<String, Value>;

/// Build `corpus_details` on each kind/instance from id lists + catalog index.
pub fn attach_frontend_corpus_details(report: &mut Value, index: &CorpusDetailIndex) {
    let Some(root) = report.as_object_mut() else {
        return;
    };
    if let Some(kinds) = root.get_mut("kinds").and_then(|v| v.as_array_mut()) {
        for kind in kinds {
            attach_details_on_node(kind, index);
            if let Some(instances) = kind
                .get_mut("instances")
                .and_then(|v| v.as_array_mut())
            {
                for inst in instances {
                    attach_details_on_node(inst, index);
                }
            }
        }
    }
    if let Some(frontends) = root.get_mut("frontends").and_then(|v| v.as_array_mut()) {
        for fe in frontends {
            attach_details_on_node(fe, index);
        }
    }
}

fn attach_details_on_node(node: &mut Value, index: &CorpusDetailIndex) {
    let Some(o) = node.as_object_mut() else {
        return;
    };
    let ids: Vec<String> = o
        .get("corpora")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        o.insert("corpus_details".into(), json!([]));
        return;
    }
    let aliases = o
        .get("corpus_aliases")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let mut details = Vec::with_capacity(ids.len());
    for id in ids {
        let mut row = index.get(&id).cloned().unwrap_or_else(|| {
            json!({
                "id": id,
                "label": id,
            })
        });
        if let Some(obj) = row.as_object_mut() {
            if let Some(a) = aliases.get(&id).and_then(|v| v.as_str()) {
                obj.insert("alias".into(), json!(a));
            }
        }
        details.push(row);
    }
    o.insert("corpus_details".into(), Value::Array(details));
}

struct FrontendKindSpec {
    id: &'static str,
    label: &'static str,
    centralized: bool,
    notes: &'static str,
}

fn handled_frontend_kinds() -> &'static [FrontendKindSpec] {
    &[
        FrontendKindSpec {
            id: "kontext",
            label: "KonText",
            centralized: true,
            notes: "Manatee/NoSkE UI; settings.kontext or FCS hit_link frontend=kontext.",
        },
        FrontendKindSpec {
            id: "cqpweb",
            label: "CQPweb",
            centralized: true,
            notes: "CWB web UI; settings.cqpweb or FCS hit_link frontend=cqpweb.",
        },
        FrontendKindSpec {
            id: "korp",
            label: "Korp",
            centralized: true,
            notes: "Språkbanken Korp; settings.korp or FCS hit_link frontend=korp.",
        },
        FrontendKindSpec {
            id: "noske",
            label: "NoSketch Engine",
            centralized: true,
            notes: "NoSkE / older Manatee UI when configured explicitly.",
        },
        FrontendKindSpec {
            id: "teitok",
            label: "TEITOK",
            centralized: false,
            notes: "Per-project PHP UI — each corpus has its own site, not one shared frontend. Status reflects catalog corpora (project_url / supports_xml / TEITOK project tree), not whether TEITOK is installed on the server.",
        },
        FrontendKindSpec {
            id: "fcs",
            label: "CLARIN FCS (via FQS)",
            centralized: true,
            notes: "FQS /fcs endpoint and settings.fcs hit links — not a separate install.",
        },
    ]
}

#[derive(Debug, Clone)]
pub struct FrontendHint {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub url: Option<String>,
    pub corpus_id: String,
    /// Engine-side name when it differs from the FQS catalog id (e.g. KonText corpname).
    pub corpus_alias: Option<String>,
    pub centralized: bool,
}

pub fn hints_from_catalog_row(
    corpus_id: &str,
    project_url: Option<&str>,
    interface_preference: Option<&str>,
    settings: &Value,
    source_kind: &str,
    supports_xml: bool,
    project_root: Option<&str>,
    capabilities: &Value,
) -> Vec<FrontendHint> {
    let mut out = Vec::new();

    // Centralized UIs only — do not host-aggregate TEITOK / bare project_url.
    if let Some(k) = settings.get("kontext") {
        let url = k
            .get("public_url")
            .or_else(|| k.get("url"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(u) = url {
            let alias = k
                .get("corpname")
                .or_else(|| k.get("corpus"))
                .and_then(Value::as_str)
                .map(str::to_string);
            out.push(FrontendHint {
                id: format!("kontext:{}", host_key(u)),
                kind: "kontext".into(),
                label: "KonText".into(),
                url: Some(u.trim_end_matches('/').to_string()),
                corpus_id: corpus_id.to_string(),
                corpus_alias: alias,
                centralized: true,
            });
        }
    }

    if let Some(cw) = settings
        .get("cqpweb")
        .or_else(|| settings.get("cqp_web"))
    {
        let url = cw
            .get("public_url")
            .or_else(|| cw.get("url"))
            .or_else(|| cw.get("base"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(u) = url {
            let alias = cw
                .get("corpus")
                .or_else(|| cw.get("corpname"))
                .or_else(|| settings.get("corpus_name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            out.push(FrontendHint {
                id: format!("cqpweb:{}", host_key(u)),
                kind: "cqpweb".into(),
                label: "CQPweb".into(),
                url: Some(u.trim_end_matches('/').to_string()),
                corpus_id: corpus_id.to_string(),
                corpus_alias: alias,
                centralized: true,
            });
        }
    }

    if let Some(korp) = settings.get("korp") {
        let url = korp
            .get("public_url")
            .or_else(|| korp.get("url"))
            .or_else(|| korp.get("base"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(u) = url {
            out.push(FrontendHint {
                id: format!("korp:{}", host_key(u)),
                kind: "korp".into(),
                label: "Korp".into(),
                url: Some(u.trim_end_matches('/').to_string()),
                corpus_id: corpus_id.to_string(),
                corpus_alias: None,
                centralized: true,
            });
        }
    }

    if let Some(hit) = settings.get("fcs").and_then(|f| f.get("hit_link")) {
        let base = hit
            .get("base")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let frontend = hit
            .get("frontend")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(u) = base {
            if is_centralized_kind(frontend) {
                let alias = hit
                    .get("corpus")
                    .or_else(|| hit.get("corpname"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let kind = normalize_frontend_kind(frontend);
                out.push(FrontendHint {
                    id: format!("{kind}:{}", host_key(u)),
                    kind: kind.clone(),
                    label: centralized_label(&kind),
                    url: Some(u.trim_end_matches('/').to_string()),
                    corpus_id: corpus_id.to_string(),
                    corpus_alias: alias,
                    centralized: true,
                });
            } else if normalize_frontend_kind(frontend) == "teitok" {
                out.push(FrontendHint {
                    id: format!("teitok:{}", corpus_id),
                    kind: "teitok".into(),
                    label: "TEITOK".into(),
                    url: Some(u.trim_end_matches('/').to_string()),
                    corpus_id: corpus_id.to_string(),
                    corpus_alias: None,
                    centralized: false,
                });
            }
        }
    }

    // Any corpus with FCS settings is exposed via FQS's /fcs endpoint.
    if settings.get("fcs").is_some() {
        out.push(FrontendHint {
            id: "fcs:local".into(),
            kind: "fcs".into(),
            label: "CLARIN FCS (via FQS)".into(),
            url: None,
            corpus_id: corpus_id.to_string(),
            corpus_alias: None,
            centralized: true,
        });
    }

    if let Some(u) = project_url.map(str::trim).filter(|s| !s.is_empty()) {
        let pref = interface_preference
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        if is_centralized_kind(pref) && !looks_like_teitok_url(u) {
            let kind = normalize_frontend_kind(pref);
            out.push(FrontendHint {
                id: format!("{kind}:{}", host_key(u)),
                kind: kind.clone(),
                label: centralized_label(&kind),
                url: Some(u.trim_end_matches('/').to_string()),
                corpus_id: corpus_id.to_string(),
                corpus_alias: None,
                centralized: true,
            });
        } else if looks_like_teitok_url(u)
            || pref.eq_ignore_ascii_case("teitok")
            || pref.is_empty() && looks_like_teitok_url(u)
        {
            out.push(FrontendHint {
                id: format!("teitok:{}", corpus_id),
                kind: "teitok".into(),
                label: "TEITOK".into(),
                url: Some(u.trim_end_matches('/').to_string()),
                corpus_id: corpus_id.to_string(),
                corpus_alias: None,
                centralized: false,
            });
        }
    }

    // TEITOK is per-corpus PHP — not a single shared frontend in fqs.json.
    // Catalog rows often omit project_url; still count TEITOK-style corpora.
    if !out.iter().any(|h| normalize_frontend_kind(&h.kind) == "teitok")
        && corpus_is_teitok_listable(
            interface_preference,
            source_kind,
            supports_xml,
            project_root,
            project_url,
            settings,
            capabilities,
        )
    {
        let url = project_url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|u| u.trim_end_matches('/').to_string());
        out.push(FrontendHint {
            id: format!("teitok:{corpus_id}"),
            kind: "teitok".into(),
            label: "TEITOK".into(),
            url,
            corpus_id: corpus_id.to_string(),
            corpus_alias: None,
            centralized: false,
        });
    }

    out
}

/// Whether a catalog row should appear in TEITOK browse / Frontends TEITOK bag.
///
/// FCS-searchable alone is not enough: Susanne/Dickens-style KonText/CWB demos can
/// be queried via FQS/FCS but have no TEITOK project UI. Require an openable TEITOK
/// entry point (project URL and/or a real TEITOK project tree), not bare `supports_xml`.
pub fn corpus_is_teitok_listable(
    interface_preference: Option<&str>,
    source_kind: &str,
    supports_xml: bool,
    project_root: Option<&str>,
    project_url: Option<&str>,
    settings: &Value,
    capabilities: &Value,
) -> bool {
    let _ = supports_xml; // not sufficient for TEITOK display listing
    match capabilities.get("teitok_listing").and_then(Value::as_bool) {
        Some(false) => return false,
        Some(true) => {
            return has_teitok_open_target(project_url, project_root);
        }
        None => {}
    }

    let url_ok = project_url_openable_for_teitok(
        project_url,
        interface_preference,
        source_kind,
        capabilities,
    ) || project_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some_and(looks_like_teitok_url);
    let root_ok = project_root
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some_and(looks_like_teitok_project_root);

    if url_ok {
        return true;
    }
    // On-disk TEITOK tree (including dummy index.php+pando) is enough even
    // without catalogue teitok_* flags — those are filled by one-shot enrich.
    let _ = settings; // reserved for future settings.teitok-only signals without a tree
    root_ok
}

fn has_teitok_open_target(project_url: Option<&str>, project_root: Option<&str>) -> bool {
    project_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some_and(|u| looks_like_teitok_url(u) || !u.is_empty())
        || project_root
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some_and(looks_like_teitok_project_root)
}

fn catalog_row_looks_like_teitok(
    interface_preference: Option<&str>,
    source_kind: &str,
    settings: &Value,
    capabilities: &Value,
) -> bool {
    if interface_preference
        .map(str::trim)
        .is_some_and(|p| p.eq_ignore_ascii_case("teitok"))
    {
        return true;
    }
    if source_kind.to_ascii_lowercase().contains("teitok") {
        return true;
    }
    if capabilities
        .get("teitok_integration")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return true;
    }
    settings.get("teitok").is_some()
        || settings
            .get("use_flexicorp_cqp")
            .and_then(Value::as_bool)
            == Some(true)
}

/// Parse `group:value` / bare labels (+ settings/capabilities languages & browse) into facet map.
pub fn browse_facets_for_corpus(
    labels: &[String],
    settings: &Value,
    capabilities: &Value,
) -> std::collections::BTreeMap<String, Vec<String>> {
    use std::collections::BTreeMap;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let push = |map: &mut BTreeMap<String, Vec<String>>, group: &str, value: &str| {
        let g = group.trim().to_ascii_lowercase();
        let v = value.trim().to_ascii_lowercase();
        if g.is_empty() || v.is_empty() {
            return;
        }
        // ISO 639-3 "und" / unknowns are placeholders — not useful browse facets.
        if g == "lang" && matches!(v.as_str(), "und" | "unk" | "unknown" | "zxx" | "mul") {
            return;
        }
        let slot = map.entry(g).or_default();
        if !slot.iter().any(|x| x == &v) {
            slot.push(v);
        }
    };

    for lab in labels {
        if let Some((g, v)) = split_facet_label(lab) {
            push(&mut out, &g, &v);
        }
    }

    // Structured browse blob (optional).
    if let Some(browse) = capabilities
        .get("browse")
        .or_else(|| settings.get("browse"))
    {
        if let Some(arr) = browse.get("languages").and_then(Value::as_array) {
            for x in arr {
                if let Some(s) = x.as_str() {
                    push(&mut out, "lang", s);
                }
            }
        }
        if let Some(arr) = browse.get("features").and_then(Value::as_array) {
            for x in arr {
                if let Some(s) = x.as_str() {
                    push(&mut out, "feature", s);
                }
            }
        }
    }

    // Common language sources used by TEITOK registration / FCS.
    for key in ["languages", "language"] {
        if let Some(arr) = settings.get(key).and_then(Value::as_array) {
            for x in arr {
                if let Some(s) = x.as_str() {
                    push(&mut out, "lang", s);
                }
            }
        } else if let Some(s) = settings.get(key).and_then(Value::as_str) {
            for part in s.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
                push(&mut out, "lang", part);
            }
        }
    }
    if let Some(fcs) = capabilities.get("fcs") {
        if let Some(arr) = fcs.get("languages").and_then(Value::as_array) {
            for x in arr {
                if let Some(s) = x.as_str() {
                    push(&mut out, "lang", s);
                }
            }
        }
    }

    out
}

fn split_facet_label(raw: &str) -> Option<(String, String)> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if let Some((g, v)) = t.split_once(':') {
        let g = g.trim().to_ascii_lowercase();
        let g = match g.as_str() {
            "language" | "languages" => "lang".into(),
            "features" => "feature".into(),
            other => other.to_string(),
        };
        let v = v.trim().to_ascii_lowercase();
        if !g.is_empty() && !v.is_empty() {
            return Some((g, v));
        }
    }
    let lower = t.to_ascii_lowercase();
    match lower.as_str() {
        "spoken" | "facsimile" | "video" | "parallel" | "written" | "oral"
        | "geolocation" | "dependencies" | "ner" | "ud" => {
            Some(("feature".into(), lower))
        }
        "english" | "en" => Some(("lang".into(), "en".into())),
        "czech" | "cs" | "cz" => Some(("lang".into(), "cs".into())),
        "german" | "de" => Some(("lang".into(), "de".into())),
        "dutch" | "nl" => Some(("lang".into(), "nl".into())),
        "french" | "fr" => Some(("lang".into(), "fr".into())),
        "spanish" | "es" => Some(("lang".into(), "es".into())),
        "italian" | "it" => Some(("lang".into(), "it".into())),
        "portuguese" | "pt" => Some(("lang".into(), "pt".into())),
        "polish" | "pl" => Some(("lang".into(), "pl".into())),
        "russian" | "ru" => Some(("lang".into(), "ru".into())),
        "slovak" | "sk" => Some(("lang".into(), "sk".into())),
        _ => {
            // ISO-like 2–3 letter code
            if lower.len() <= 3 && lower.chars().all(|c| c.is_ascii_alphabetic()) {
                Some(("lang".into(), lower))
            } else {
                Some(("other".into(), lower))
            }
        }
    }
}

/// Requested facets: AND across groups, OR within the same group.
pub fn corpus_matches_requested_facets(
    corpus_facets: &std::collections::BTreeMap<String, Vec<String>>,
    requested: &[(String, String)],
) -> bool {
    if requested.is_empty() {
        return true;
    }
    use std::collections::BTreeMap;
    let mut by_group: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (g, v) in requested {
        by_group.entry(g.clone()).or_default().push(v.clone());
    }
    for (group, want) in by_group {
        let have = corpus_facets.get(&group).map(Vec::as_slice).unwrap_or(&[]);
        if !want.iter().any(|w| have.iter().any(|h| h == w)) {
            return false;
        }
    }
    true
}

pub fn parse_facet_params(raw_query: Option<&str>, facets_csv: Option<&str>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut push_token = |s: &str| {
        let t = s.trim();
        if t.is_empty() {
            return;
        }
        if let Some((g, v)) = split_facet_label(t) {
            out.push((g, v));
        }
    };
    if let Some(csv) = facets_csv {
        for part in csv.split(',') {
            push_token(part);
        }
    }
    if let Some(raw) = raw_query {
        for pair in raw.split('&') {
            let mut it = pair.splitn(2, '=');
            let key = it.next().unwrap_or("");
            let val = it.next().unwrap_or("");
            let key = percent_decode(key);
            if key != "facet" && key != "facets" {
                continue;
            }
            let val = percent_decode(val);
            if key == "facets" {
                for part in val.split(',') {
                    push_token(part);
                }
            } else {
                push_token(&val);
            }
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let h = |c: u8| -> Option<u8> {
                    match c {
                        b'0'..=b'9' => Some(c - b'0'),
                        b'a'..=b'f' => Some(c - b'a' + 10),
                        b'A'..=b'F' => Some(c - b'A' + 10),
                        _ => None,
                    }
                };
                if let (Some(a), Some(b)) = (h(bytes[i + 1]), h(bytes[i + 2])) {
                    out.push((a << 4) | b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn facet_dictionary(
    corpora_facets: &[std::collections::BTreeMap<String, Vec<String>>],
) -> Value {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for facets in corpora_facets {
        for (g, vals) in facets {
            let slot = counts.entry(g.clone()).or_default();
            for v in vals {
                *slot.entry(v.clone()).or_default() += 1;
            }
        }
    }
    let mut facets_obj = serde_json::Map::new();
    let mut flat_labels = Vec::new();
    for (g, vals) in counts {
        let mut arr = Vec::new();
        for (v, n) in vals {
            flat_labels.push(format!("{g}:{v}"));
            arr.push(json!({"value": v, "count": n}));
        }
        arr.sort_by(|a, b| {
            a.get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .cmp(b.get("value").and_then(Value::as_str).unwrap_or(""))
        });
        facets_obj.insert(g, Value::Array(arr));
    }
    flat_labels.sort();
    flat_labels.dedup();
    json!({
        "labels": flat_labels,
        "facets": facets_obj,
    })
}

fn looks_like_teitok_project_root(path: &str) -> bool {
    let dir = Path::new(path);
    // project_root may point at …/pando; TEITOK entry is often the parent.
    let parent = dir.parent().unwrap_or(dir);
    for cand in [dir, parent] {
        if teitok_project_dir(cand) {
            return true;
        }
    }
    false
}

fn teitok_project_dir(dir: &Path) -> bool {
    if !dir.is_dir() || !dir.join("index.php").is_file() {
        return false;
    }
    // Full TEITOK layout markers…
    let markers = [
        "Scripts",
        "Pages",
        "xmlfiles",
        "cqpsettings.xml",
        "Resources/settings.xml",
    ];
    if markers.iter().any(|m| dir.join(m).exists()) {
        return true;
    }
    // …or a “dummy” TEITOK wrapper: index.php + a query backend folder (e.g. ud_pando
    // without xmlfiles). Bare CWB/Manatee data dirs lack index.php, so they stay out.
    ["pando", "cqp", "manatee", "xidx"]
        .iter()
        .any(|m| dir.join(m).is_dir())
}

/// Non-empty project_url + TEITOK catalogue signal counts even when the URL
/// path does not contain the substring "teitok" (some vhosts omit it).
pub fn project_url_openable_for_teitok(
    project_url: Option<&str>,
    interface_preference: Option<&str>,
    source_kind: &str,
    capabilities: &Value,
) -> bool {
    let url = project_url.map(str::trim).filter(|s| !s.is_empty());
    let Some(u) = url else {
        return false;
    };
    if looks_like_teitok_url(u) {
        return true;
    }
    catalog_row_looks_like_teitok(interface_preference, source_kind, &json!({}), capabilities)
}

fn is_centralized_kind(kind: &str) -> bool {
    matches!(
        normalize_frontend_kind(kind).as_str(),
        "kontext" | "cqpweb" | "korp" | "noske" | "fcs"
    )
}

fn normalize_frontend_kind(kind: &str) -> String {
    match kind.trim().to_ascii_lowercase().as_str() {
        "cqp_web" | "cqp-web" => "cqpweb".into(),
        "kontext_first" | "manatee" => "kontext".into(),
        other => other.to_string(),
    }
}

fn centralized_label(kind: &str) -> String {
    match kind {
        "kontext" => "KonText".into(),
        "cqpweb" => "CQPweb".into(),
        "korp" => "Korp".into(),
        "noske" => "NoSketch Engine".into(),
        other => other.to_string(),
    }
}

fn looks_like_teitok_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("/teitok/")
        || lower.contains("/teitok-")
        || lower.contains("teitok.")
}

fn resolve_frontend_merge_id(by_id: &Map<String, Value>, hint: &FrontendHint) -> String {
    if by_id.contains_key(&hint.id) {
        return hint.id.clone();
    }
    let hint_url = hint.url.as_deref().unwrap_or("");
    for (id, v) in by_id {
        let kind = v
            .get("kind")
            .and_then(Value::as_str)
            .map(normalize_frontend_kind)
            .unwrap_or_default();
        if kind != normalize_frontend_kind(&hint.kind) {
            continue;
        }
        let url = v
            .get("url")
            .or_else(|| v.get("health_url"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !hint_url.is_empty() && urls_same_service(url, hint_url) {
            return id.clone();
        }
    }
    hint.id.clone()
}

fn urls_same_service(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let na = normalize_service_url(a);
    let nb = normalize_service_url(b);
    na == nb || na.starts_with(&nb) || nb.starts_with(&na) || host_key(a) == host_key(b)
}

fn normalize_service_url(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

fn configured_frontends() -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let path = fqs_config_path();
    let Ok(s) = fs::read_to_string(&path) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<Value>(&s) else {
        return out;
    };
    let Some(arr) = v.get("frontends").and_then(|x| x.as_array()) else {
        return out;
    };
    for item in arr {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                item.get("url")
                    .and_then(Value::as_str)
                    .map(host_key)
                    .unwrap_or_else(|| "frontend".into())
            });
        let mut obj = item.clone();
        if let Some(o) = obj.as_object_mut() {
            o.insert("id".into(), json!(id));
            o.insert("source".into(), json!("fqs.json"));
            o.entry("label").or_insert(json!(id));
            o.entry("kind").or_insert(json!("unknown"));
        }
        out.push((id, obj));
    }
    out
}

fn fqs_config_path() -> PathBuf {
    std::env::var("FQS_CONFIG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/fqs/fqs.json"))
}

fn host_key(url: &str) -> String {
    url.trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':' { c } else { '_' })
        .collect()
}

fn http_probe(url: &str) -> Value {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout_read(Duration::from_secs(3))
        .build();
    match agent.get(url).call() {
        Ok(resp) => json!({
            "ok": resp.status() < 500,
            "status": resp.status(),
            "url": url,
        }),
        Err(e) => json!({
            "ok": false,
            "error": e.to_string(),
            "url": url,
        }),
    }
}

fn discover_gunicorn() -> Vec<Value> {
    let out = Command::new("pgrep").args(["-af", "gunicorn"]).output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(20)
        .map(|line| {
            let mut parts = line.splitn(2, char::is_whitespace);
            let pid = parts.next().unwrap_or("");
            let cmd = parts.next().unwrap_or("").trim();
            json!({"pid": pid, "cmd": cmd})
        })
        .collect()
}

/// Run a configured restart. Returns JSON result; errors as Err(message).
pub fn restart_frontend(frontend_id: &str) -> Result<Value, String> {
    let configured = configured_frontends();
    let entry = configured
        .into_iter()
        .find(|(id, _)| id == frontend_id)
        .map(|(_, v)| v)
        .ok_or_else(|| {
            format!(
                "frontend '{frontend_id}' has no restart config in {} (catalog-only frontends cannot be restarted)",
                fqs_config_path().display()
            )
        })?;
    let restart = entry
        .get("restart")
        .and_then(|v| v.as_object())
        .ok_or_else(|| format!("frontend '{frontend_id}' has no restart block"))?;
    run_restart_block(restart)
}

/// Restart this FQS process via the optional `fqs.restart` block in fqs.json.
pub fn restart_fqs() -> Result<Value, String> {
    let restart = fqs_self_config()
        .get("restart")
        .and_then(|v| v.as_object())
        .cloned()
        .ok_or_else(|| {
            format!(
                "no fqs.restart block in {} — configure e.g. {{\"fqs\":{{\"restart\":{{\"method\":\"systemctl\",\"unit\":\"fqs\"}}}}}}",
                fqs_config_path().display()
            )
        })?;
    run_restart_block(&restart)
}

/// Inventory for this FQS binary: version, update check, restartability.
pub fn probe_fqs_self(server_name: Option<&str>) -> Value {
    let version = env!("CARGO_PKG_VERSION").to_string();
    let cfg = fqs_self_config();
    let restartable = cfg.get("restart").and_then(|v| v.as_object()).is_some();
    let update = check_fqs_update(&version, &cfg);
    json!({
        "ok": true,
        "id": "fqs",
        "label": "FQS (this process)",
        "version": version,
        "pkg_name": env!("CARGO_PKG_NAME"),
        "pid": std::process::id(),
        "server_name": server_name,
        "config_path": fqs_config_path().display().to_string(),
        "restartable": restartable,
        "restart_policy": if restartable {
            "Restart uses the fqs.restart block in fqs.json (same methods as frontends)."
        } else {
            "Configure fqs.restart in fqs.json to enable Restart from the admin UI."
        },
        "update": update,
    })
}

fn fqs_self_config() -> Value {
    let Ok(s) = fs::read_to_string(fqs_config_path()) else {
        return json!({});
    };
    let Ok(v) = serde_json::from_str::<Value>(&s) else {
        return json!({});
    };
    v.get("fqs").cloned().unwrap_or(json!({}))
}

fn check_fqs_update(local_version: &str, cfg: &Value) -> Value {
    if cfg.get("update_check").and_then(|v| v.as_bool()) == Some(false) {
        return json!({
            "checked": false,
            "disabled": true,
            "local": local_version,
        });
    }

    let default_url =
        "https://raw.githubusercontent.com/ufal/flexicorp/main/fqs/Cargo.toml";
    let url = cfg
        .get("update_check_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("FQS_UPDATE_CHECK_URL")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| default_url.to_string());

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout_read(Duration::from_secs(4))
        .build();
    match agent.get(&url).call() {
        Ok(resp) => {
            let body = resp.into_string().unwrap_or_default();
            let remote = parse_version_from_update_body(&body);
            match remote {
                Some(remote) => {
                    let newer = semver_is_newer(&remote, local_version);
                    json!({
                        "checked": true,
                        "url": url,
                        "local": local_version,
                        "latest": remote,
                        "update_available": newer == Some(true),
                        "comparable": newer.is_some(),
                    })
                }
                None => json!({
                    "checked": true,
                    "url": url,
                    "local": local_version,
                    "error": "could not parse a version from the update-check response",
                }),
            }
        }
        Err(e) => json!({
            "checked": false,
            "url": url,
            "local": local_version,
            "error": e.to_string(),
        }),
    }
}

fn parse_version_from_update_body(body: &str) -> Option<String> {
    let t = body.trim();
    if t.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(t) {
            if let Some(s) = v
                .get("version")
                .or_else(|| v.get("tag_name"))
                .and_then(Value::as_str)
            {
                return Some(s.trim().trim_start_matches('v').to_string());
            }
        }
    }
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("version") {
            let rest = rest.trim().trim_start_matches('=').trim();
            if let Some(q) = rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                return Some(q.to_string());
            }
            if let Some(q) = rest.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
                return Some(q.to_string());
            }
        }
    }
    None
}

fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()
        .unwrap_or("0")
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

fn semver_is_newer(remote: &str, local: &str) -> Option<bool> {
    let r = parse_semver(remote)?;
    let l = parse_semver(local)?;
    Some(r > l)
}

fn run_restart_block(restart: &serde_json::Map<String, Value>) -> Result<Value, String> {
    let method = restart
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();

    match method.as_str() {
        "systemctl" => {
            let unit = restart
                .get("unit")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "restart.unit required for systemctl".to_string())?;
            if !unit
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '@')
            {
                return Err("restart.unit contains invalid characters".into());
            }
            let action = restart
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("restart");
            if !matches!(action, "restart" | "reload" | "try-reload-or-restart") {
                return Err("restart.action must be restart|reload|try-reload-or-restart".into());
            }
            let out = Command::new("systemctl")
                .args([action, unit])
                .output()
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "ok": out.status.success(),
                "method": "systemctl",
                "unit": unit,
                "action": action,
                "stdout": String::from_utf8_lossy(&out.stdout),
                "stderr": String::from_utf8_lossy(&out.stderr),
                "exit_code": out.status.code(),
            }))
        }
        "kill_hup_pidfile" | "hup_pidfile" => {
            let pidfile = restart
                .get("pidfile")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "restart.pidfile required".to_string())?;
            let pid = fs::read_to_string(pidfile)
                .map_err(|e| format!("read pidfile: {e}"))?
                .trim()
                .parse::<i32>()
                .map_err(|_| "pidfile did not contain an integer pid".to_string())?;
            let out = Command::new("kill")
                .args(["-HUP", &pid.to_string()])
                .output()
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "ok": out.status.success(),
                "method": "kill_hup_pidfile",
                "pidfile": pidfile,
                "pid": pid,
                "stderr": String::from_utf8_lossy(&out.stderr),
                "exit_code": out.status.code(),
            }))
        }
        "command" => {
            let argv = restart
                .get("argv")
                .and_then(Value::as_array)
                .ok_or_else(|| "restart.argv must be a string array".to_string())?;
            let args: Vec<&str> = argv.iter().filter_map(|v| v.as_str()).collect();
            if args.is_empty() {
                return Err("restart.argv empty".into());
            }
            for a in &args {
                if a.chars().any(|c| matches!(c, ';' | '|' | '&' | '`' | '$' | '\n')) {
                    return Err("restart.argv entry contains forbidden characters".into());
                }
            }
            let mut cmd = Command::new(args[0]);
            if args.len() > 1 {
                cmd.args(&args[1..]);
            }
            let out = cmd.output().map_err(|e| e.to_string())?;
            Ok(json!({
                "ok": out.status.success(),
                "method": "command",
                "argv": args,
                "stdout": String::from_utf8_lossy(&out.stdout),
                "stderr": String::from_utf8_lossy(&out.stderr),
                "exit_code": out.status.code(),
            }))
        }
        "" => Err("restart.method required (systemctl | kill_hup_pidfile | command)".into()),
        other => Err(format!("unknown restart.method '{other}'")),
    }
}

fn which_bin(candidates: &[Option<&str>]) -> Option<String> {
    for c in candidates.iter().flatten() {
        let t = c.trim();
        if t.is_empty() {
            continue;
        }
        let p = Path::new(t);
        if p.is_file() {
            return Some(p.display().to_string());
        }
        if let Ok(out) = Command::new("which").arg(t).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn version_from_cmd(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    parse_version_banner(&text)
}

/// Prefer an explicit `Version:` / `version` line (CQP `-v` prints a long
/// copyright banner and puts `Version: 3.5.0` near the end).
fn parse_version_banner(text: &str) -> Option<String> {
    let lower_reject = |line: &str| {
        let lower = line.to_ascii_lowercase();
        lower.contains("illegal option")
            || lower.starts_with("usage:")
            || lower.starts_with("invalid option")
    };

    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if lower_reject(line) {
            return None;
        }
        // CWB: "Version:   3.5.0" or "Version 3.0 developed by: …" (ignore the latter)
        if let Some(rest) = line.strip_prefix("Version:") {
            let v = rest.trim();
            if !v.is_empty() && v.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                return Some(v.to_string());
            }
        }
        if let Some(rest) = line.strip_prefix("version:") {
            let v = rest.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }

    // Fallback: first non-empty, non-noise line (other tools' --version).
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !lower_reject(l))
        .map(str::to_string)
}

/// One catalog row for coverage / gap analysis (admin Frontends tab).
pub struct CoverageCorpus<'a> {
    pub id: &'a str,
    pub label: &'a str,
    pub preferred_backend: &'a str,
    pub is_current: bool,
    pub http_policy_mode: &'a str,
    pub interface_preference: Option<&'a str>,
    pub source_kind: &'a str,
    pub supports_xml: bool,
    pub project_root: Option<&'a str>,
    pub project_url: Option<&'a str>,
    pub settings: &'a Value,
    pub capabilities: &'a Value,
}

/// Compare FQS catalog to configured frontend inventories (KonText corplist, FCS opt-in).
pub fn compute_frontend_coverage(corpora: &[CoverageCorpus<'_>]) -> Value {
    let mut kontext_reports = Vec::new();
    for (id, cfg) in configured_frontends() {
        let kind = cfg
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if kind != "kontext" && id.to_ascii_lowercase() != "kontext" {
            continue;
        }
        kontext_reports.push(kontext_coverage_for_frontend(&id, &cfg, corpora));
    }

    // Catalog may show KonText even when fqs.json has no frontends[] entry — still
    // surface TEITOK corpora that are not in a corplist.
    if kontext_reports.is_empty() {
        let url = corpora.iter().find_map(|c| {
            c.settings
                .get("kontext")
                .and_then(|k| k.get("url").or_else(|| k.get("base")))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        });
        let any_eligible = corpora
            .iter()
            .any(|c| c.is_current && corpus_eligible_for_kontext(c));
        let discovered = resolve_kontext_corplist(None).0.is_some();
        if url.is_some() || any_eligible || discovered {
            let mut cfg = Map::new();
            cfg.insert("kind".into(), json!("kontext"));
            if let Some(u) = url {
                cfg.insert("url".into(), json!(u));
            }
            kontext_reports.push(kontext_coverage_for_frontend(
                "kontext",
                &Value::Object(cfg),
                corpora,
            ));
        }
    }

    // FCS gaps: queryable / TEITOK corpora with no explicit settings.fcs.enabled yet.
    // Opt-out is only when enabled is false; absence means “not added”, same as KonText.
    let mut missing_fcs = Vec::new();
    for c in corpora {
        if !c.is_current {
            continue;
        }
        if !corpus_eligible_for_fcs_suggest(c) {
            continue;
        }
        match fcs_enabled_flag(c.settings, c.capabilities) {
            Some(true) => continue,  // already in FCS
            Some(false) => continue, // explicitly excluded
            None => {}
        }
        missing_fcs.push(json!({
            "id": c.id,
            "label": c.label,
            "preferred_backend": c.preferred_backend,
            "project_url": c.project_url,
            "reason": "fcs_not_enabled",
        }));
    }

    json!({
        "ok": true,
        "kontext": kontext_reports,
        "fcs": {
            "missing": missing_fcs.clone(),
            // Alias kept for older admin UI builds.
            "undecided": missing_fcs,
        },
        "help": {
            "kontext_corplist": "Optional: set frontends[].corplist in fqs.json (or FQS_KONTEXT_CORPLIST). Otherwise FQS looks under /opt/kontext/conf/.",
            "fcs": "Add to FCS sets settings.fcs.enabled=true; Exclude sets false.",
        },
    })
}

fn kontext_coverage_for_frontend(frontend_id: &str, cfg: &Value, corpora: &[CoverageCorpus<'_>]) -> Value {
    let frontend_url = cfg
        .get("url")
        .or_else(|| cfg.get("base"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let (corplist_path, path_source) = resolve_kontext_corplist(Some(cfg));

    let (idents, path_display, setup_hint, appendable) = match &corplist_path {
        Some(path) => match read_kontext_corplist_idents(path) {
            Ok(ids) => (
                ids,
                Some(path.display().to_string()),
                None,
                path.is_file(),
            ),
            Err(e) => (
                Vec::new(),
                Some(path.display().to_string()),
                Some(e),
                false,
            ),
        },
        None => (
            Vec::new(),
            None,
            Some(
                "No KonText corplist.xml found (set frontends[].corplist or FQS_KONTEXT_CORPLIST)."
                    .into(),
            ),
            false,
        ),
    };

    let ident_set: std::collections::HashSet<String> =
        idents.iter().map(|s| s.to_ascii_lowercase()).collect();
    let have_corplist = corplist_path.is_some() && setup_hint.is_none();

    let mut missing = Vec::new();
    for c in corpora {
        if !c.is_current {
            continue;
        }
        if !corpus_eligible_for_kontext(c) {
            continue;
        }
        let suggested = suggested_kontext_ident(c);
        let catalog_corpname = c
            .settings
            .get("kontext")
            .and_then(|k| k.get("corpname").or_else(|| k.get("corpus")))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let covered = if have_corplist {
            ident_set.contains(&suggested.to_ascii_lowercase())
                || catalog_corpname
                    .map(|n| ident_set.contains(&n.to_ascii_lowercase()))
                    .unwrap_or(false)
        } else {
            // Without a readable corplist we cannot claim coverage — list gaps.
            false
        };
        if covered {
            continue;
        }

        let xml = format!(
            "<corpus ident=\"{}\" sentence_struct=\"s\"/>",
            xml_escape_attr(&suggested)
        );
        missing.push(json!({
            "id": c.id,
            "label": c.label,
            "preferred_backend": c.preferred_backend,
            "project_url": c.project_url,
            "suggested_ident": suggested,
            "catalog_corpname": catalog_corpname,
            "reason": if have_corplist {
                "teitok_not_in_corplist"
            } else {
                "teitok_corplist_unresolved"
            },
            "suggested_xml": xml,
            "frontend_url": frontend_url,
        }));
    }

    json!({
        "frontend_id": frontend_id,
        "url": frontend_url,
        "corplist_path": path_display,
        "corplist_source": path_source,
        "configured": true,
        "setup_hint": setup_hint,
        "idents": idents,
        "missing": missing,
        "appendable": appendable,
    })
}

/// TEITOK-listable corpora that can be published into a KonText corplist.
fn corpus_eligible_for_kontext(c: &CoverageCorpus<'_>) -> bool {
    corpus_is_teitok_listable(
        c.interface_preference,
        c.source_kind,
        c.supports_xml,
        c.project_root,
        c.project_url,
        c.settings,
        c.capabilities,
    )
}

fn corpus_looks_pando_servable(c: &CoverageCorpus<'_>) -> bool {
    let b = c.preferred_backend.trim().to_ascii_lowercase();
    if b == "pando" {
        return true;
    }
    if let Some(arr) = c.settings.get("available_backends").and_then(|v| v.as_array()) {
        if arr.iter().any(|v| v.as_str() == Some("pando")) {
            return true;
        }
    }
    if c.settings
        .get("query_backend")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("pando"))
    {
        return true;
    }
    if let Some(root) = c.project_root.map(str::trim).filter(|s| !s.is_empty()) {
        let p = Path::new(root);
        if p.join("pando").is_dir() || p.join("Resources").join("pando").is_dir() {
            return true;
        }
    }
    false
}

fn corpus_can_serve_fcs(c: &CoverageCorpus<'_>) -> bool {
    if c.http_policy_mode.trim().eq_ignore_ascii_case("disabled") {
        return false;
    }
    let b = c.preferred_backend.trim().to_ascii_lowercase();
    if b == "pando" || b == "cqp" || b == "cwb" {
        return true;
    }
    if let Some(arr) = c.settings.get("available_backends").and_then(|v| v.as_array()) {
        return arr
            .iter()
            .any(|v| matches!(v.as_str(), Some("pando") | Some("cqp") | Some("cwb")));
    }
    corpus_looks_pando_servable(c)
}

/// Corpora to suggest for FCS opt-in: anything queryable via FQS, plus TEITOK
/// corpora (same set people expect to publish to KonText) when backend is auto.
fn corpus_eligible_for_fcs_suggest(c: &CoverageCorpus<'_>) -> bool {
    if c.http_policy_mode.trim().eq_ignore_ascii_case("disabled") {
        return false;
    }
    if corpus_can_serve_fcs(c) {
        return true;
    }
    let b = c.preferred_backend.trim().to_ascii_lowercase();
    if b == "auto" || b.is_empty() {
        return corpus_eligible_for_kontext(c) || corpus_looks_pando_servable(c);
    }
    corpus_eligible_for_kontext(c)
}

fn fcs_enabled_flag(settings: &Value, capabilities: &Value) -> Option<bool> {
    settings
        .get("fcs")
        .and_then(|f| f.get("enabled"))
        .and_then(Value::as_bool)
        .or_else(|| {
            capabilities
                .get("fcs")
                .and_then(|f| f.get("enabled"))
                .and_then(Value::as_bool)
        })
}

fn suggested_kontext_ident(c: &CoverageCorpus<'_>) -> String {
    c.settings
        .get("kontext")
        .and_then(|k| k.get("corpname").or_else(|| k.get("corpus")))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(c.id)
        .to_string()
}

/// Resolve a KonText corplist.xml path: fqs.json → env → discovery under common installs.
/// Returns `(path, source)` where source is `fqs.json`, `env`, `config.xml`, or `discovered`.
fn resolve_kontext_corplist(cfg: Option<&Value>) -> (Option<PathBuf>, Option<&'static str>) {
    if let Some(cfg) = cfg {
        if let Some(p) = cfg
            .get("corplist")
            .or_else(|| cfg.get("corplist_path"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
        {
            return (Some(p), Some("fqs.json"));
        }
    }
    if let Ok(p) = std::env::var("FQS_KONTEXT_CORPLIST") {
        let p = p.trim();
        if !p.is_empty() {
            return (Some(PathBuf::from(p)), Some("env"));
        }
    }
    for (path, src) in kontext_corplist_candidates() {
        if path.is_file() {
            return (Some(path), Some(src));
        }
    }
    (None, None)
}

fn kontext_corplist_candidates() -> Vec<(PathBuf, &'static str)> {
    let mut out: Vec<(PathBuf, &'static str)> = Vec::new();
    let mut push = |p: PathBuf, src: &'static str| {
        if !out.iter().any(|(x, _)| x == &p) {
            out.push((p, src));
        }
    };

    // Paths referenced from config.xml next to common KonText installs.
    for conf_dir in [
        "/opt/kontext/conf",
        "/opt/kontext/installation/conf",
        "/var/www/kontext/conf",
        "/usr/local/share/kontext/conf",
    ] {
        let config = Path::new(conf_dir).join("config.xml");
        if let Some(p) = corplist_path_from_kontext_config(&config) {
            push(p, "config.xml");
        }
        push(Path::new(conf_dir).join("corplist.xml"), "discovered");
    }

    // Sibling of gunicorn cwd when KonText was started from its install root.
    if let Ok(out_cmd) = Command::new("pgrep").args(["-af", "gunicorn"]).output() {
        if out_cmd.status.success() {
            for line in String::from_utf8_lossy(&out_cmd.stdout).lines() {
                for token in line.split_whitespace() {
                    if let Some(idx) = token.find("/conf/") {
                        let root = Path::new(&token[..idx]);
                        push(root.join("conf").join("corplist.xml"), "discovered");
                    }
                    if token.ends_with("corplist.xml") {
                        push(PathBuf::from(token), "discovered");
                    }
                }
            }
        }
    }

    out
}

fn corplist_path_from_kontext_config(config_xml: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(config_xml).ok()?;
    // Prefer <file …>…corplist.xml</file> (lindat / tree_corparch).
    let lower = text.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find("<file") {
        let start = search_from + rel;
        let after = &text[start..];
        let Some(close) = after.find("</file>") else {
            break;
        };
        let inner = &after[..close];
        if let Some(gt) = inner.find('>') {
            let path = inner[gt + 1..].trim();
            if path.to_ascii_lowercase().contains("corplist") && !path.is_empty() {
                let p = PathBuf::from(path);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        search_from = start + close.max(1);
    }
    None
}

fn corplist_path_is_allowed(path: &Path) -> bool {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    for (id, cfg) in configured_frontends() {
        let kind = cfg
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if kind != "kontext" && id.to_ascii_lowercase() != "kontext" {
            continue;
        }
        if let Some(p) = cfg
            .get("corplist")
            .or_else(|| cfg.get("corplist_path"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
        {
            let pc = p.canonicalize().unwrap_or(p);
            if pc == canon {
                return true;
            }
        }
    }
    if let Ok(p) = std::env::var("FQS_KONTEXT_CORPLIST") {
        let p = PathBuf::from(p.trim());
        if !p.as_os_str().is_empty() {
            let pc = p.canonicalize().unwrap_or(p);
            if pc == canon {
                return true;
            }
        }
    }
    for (cand, _) in kontext_corplist_candidates() {
        let cc = cand.canonicalize().unwrap_or(cand);
        if cc == canon {
            return true;
        }
    }
    false
}

fn read_kontext_corplist_idents(path: &Path) -> Result<Vec<String>, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("Cannot read corplist {}: {}", path.display(), e))?;
    Ok(parse_kontext_corplist_idents(&text))
}

fn parse_kontext_corplist_idents(text: &str) -> Vec<String> {
    let mut idents = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find("<corpus") {
        let start = search_from + rel;
        let after = &text[start..];
        let end_rel = after.find('>').unwrap_or(after.len());
        let tag = &after[..end_rel];
        if let Some(id) = attr_value_from_tag(tag, "ident").or_else(|| attr_value_from_tag(tag, "id"))
        {
            let id = id.trim();
            if !id.is_empty() && !idents.iter().any(|x: &String| x == id) {
                idents.push(id.to_string());
            }
        }
        search_from = start + end_rel.max(1);
    }
    idents
}

fn attr_value_from_tag<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let key = format!("{name}=");
    let idx = lower.find(&key)?;
    let rest = &tag[idx + key.len()..];
    let rest = rest.trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let inner = &rest[1..];
    let end = inner.find(quote)?;
    Some(&inner[..end])
}

fn xml_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Append a corpus line to an allowlisted KonText corplist for `frontend_id`.
///
/// Path resolution: `frontends[].corplist` → `FQS_KONTEXT_CORPLIST` → discovered
/// `/opt/kontext/conf/corplist.xml` (and config.xml references). Works for a
/// synthesized `kontext` id when discovery finds a file.
pub fn append_kontext_corplist_corpus(
    frontend_id: &str,
    ident: &str,
    sentence_struct: Option<&str>,
) -> Result<Value, String> {
    let ident = ident.trim();
    if ident.is_empty() || ident.contains(['<', '>', '"', '\'', '/', '\\']) {
        return Err("invalid corpus ident".into());
    }
    let ss = sentence_struct
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("s");
    if ss
        .chars()
        .any(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
    {
        return Err("invalid sentence_struct".into());
    }

    let configured = configured_frontends();
    let cfg = configured
        .iter()
        .find(|(id, _)| id == frontend_id)
        .map(|(_, v)| v);
    if let Some(cfg) = cfg {
        let kind = cfg
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if kind != "kontext" && frontend_id.to_ascii_lowercase() != "kontext" {
            return Err("append is only supported for KonText frontends".into());
        }
    } else if frontend_id.to_ascii_lowercase() != "kontext" {
        return Err(format!(
            "frontend '{frontend_id}' not in fqs.json and is not the default KonText id"
        ));
    }

    let (path, source) = resolve_kontext_corplist(cfg);
    let path = path.ok_or_else(|| {
        "No KonText corplist.xml found. Set frontends[].corplist in fqs.json or FQS_KONTEXT_CORPLIST."
            .to_string()
    })?;
    if !corplist_path_is_allowed(&path) {
        return Err(format!(
            "corplist path {} is not allowlisted",
            path.display()
        ));
    }

    let text = fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read {}: {}", path.display(), e))?;
    let idents = parse_kontext_corplist_idents(&text);
    if idents.iter().any(|i| i.eq_ignore_ascii_case(ident)) {
        return Ok(json!({
            "ok": true,
            "already_present": true,
            "ident": ident,
            "corplist_path": path.display().to_string(),
            "corplist_source": source,
        }));
    }

    let line = format!("        <corpus ident=\"{ident}\" sentence_struct=\"{ss}\"/>\n");
    let updated = if let Some(idx) = text.rfind("</corplist>") {
        let mut out = String::with_capacity(text.len() + line.len());
        out.push_str(&text[..idx]);
        out.push_str(&line);
        out.push_str(&text[idx..]);
        out
    } else {
        return Err("corplist.xml has no </corplist> closing tag".into());
    };

    let tmp = path.with_extension("xml.fqs-tmp");
    fs::write(&tmp, &updated).map_err(|e| format!("Cannot write temp file: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("Cannot replace {}: {e}", path.display())
    })?;

    Ok(json!({
        "ok": true,
        "already_present": false,
        "ident": ident,
        "corplist_path": path.display().to_string(),
        "corplist_source": source,
        "appended_xml": line.trim(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn teitok_project_url_is_per_corpus_hint() {
        let hints = hints_from_catalog_row(
            "infoveillance_pando",
            Some("http://127.0.0.1/teitok/easycorp/infoveillance/index.php"),
            None,
            &json!({}),
            "generic",
            false,
            None,
            &json!({}),
        );
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].kind, "teitok");
        assert!(!hints[0].centralized);
    }

    #[test]
    fn teitok_supports_xml_alone_is_not_listable() {
        assert!(!corpus_is_teitok_listable(
            None,
            "cwb_registry",
            true,
            None,
            None,
            &json!({}),
            &json!({}),
        ));
        let hints = hints_from_catalog_row(
            "dickens",
            None,
            None,
            &json!({}),
            "cwb_registry",
            true,
            None,
            &json!({}),
        );
        assert!(hints.iter().all(|h| h.kind != "teitok"));
    }

    #[test]
    fn teitok_source_kind_needs_open_target() {
        assert!(!corpus_is_teitok_listable(
            Some("teitok"),
            "teitok",
            false,
            None,
            None,
            &json!({}),
            &json!({"teitok_integration": true}),
        ));
        assert!(corpus_is_teitok_listable(
            Some("teitok"),
            "teitok",
            false,
            None,
            Some("https://example.org/teitok/ud_pando/"),
            &json!({}),
            &json!({"teitok_integration": true}),
        ));
    }

    #[test]
    fn dummy_teitok_index_plus_pando_is_listable_without_catalog_flags() {
        let tmp = std::env::temp_dir().join(format!(
            "fqs_dummy_teitok_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("pando")).expect("mkdir pando");
        std::fs::write(tmp.join("index.php"), "<?php\n").expect("index.php");
        let root = tmp.to_string_lossy().to_string();
        assert!(corpus_is_teitok_listable(
            None,
            "pando_index",
            false,
            Some(&root),
            None,
            &json!({}),
            &json!({}),
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn browse_facets_and_match() {
        let facets = browse_facets_for_corpus(
            &["spoken".into(), "lang:cs".into(), "UD".into()],
            &json!({"languages": ["en"]}),
            &json!({}),
        );
        assert!(facets.get("feature").unwrap().contains(&"spoken".to_string()));
        assert!(facets.get("feature").unwrap().contains(&"ud".to_string()));
        assert!(facets.get("lang").unwrap().contains(&"cs".to_string()));
        assert!(facets.get("lang").unwrap().contains(&"en".to_string()));

        let req = parse_facet_params(
            Some("facet=lang%3Acs&facet=feature:spoken"),
            None,
        );
        assert!(corpus_matches_requested_facets(&facets, &req));
        let req2 = vec![("lang".into(), "de".into())];
        assert!(!corpus_matches_requested_facets(&facets, &req2));
    }

    #[test]
    fn kontext_settings_yield_corpus_list_hint() {
        let hints = hints_from_catalog_row(
            "ud_pando",
            None,
            None,
            &json!({"kontext": {"url": "http://127.0.0.1:8080/kontext", "corpname": "ud"}}),
            "generic",
            false,
            None,
            &json!({}),
        );
        assert_eq!(hints.len(), 1);
        assert!(hints[0].centralized);
        assert_eq!(hints[0].kind, "kontext");
        assert_eq!(hints[0].corpus_alias.as_deref(), Some("ud"));
    }

    #[test]
    fn cqpweb_settings_yield_hint() {
        let hints = hints_from_catalog_row(
            "dickens",
            None,
            None,
            &json!({"cqpweb": {"url": "https://cqpweb.example/cqpweb", "corpus": "DICKENS"}}),
            "generic",
            false,
            None,
            &json!({}),
        );
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].kind, "cqpweb");
        assert!(hints[0].centralized);
    }

    #[test]
    fn attach_corpus_details_joins_catalog_index() {
        let mut report = json!({
            "kinds": [{
                "id": "kontext",
                "corpora": ["migrantstories", "ud_pando"],
                "instances": [{
                    "id": "kontext",
                    "corpora": ["ud_pando"],
                    "corpus_aliases": { "ud_pando": "ud" }
                }]
            }],
            "frontends": []
        });
        let mut index = Map::new();
        index.insert(
            "migrantstories".into(),
            json!({"id": "migrantstories", "label": "Migrant Stories", "preferred_backend": "pando"}),
        );
        index.insert(
            "ud_pando".into(),
            json!({"id": "ud_pando", "label": "UD Pando", "preferred_backend": "pando"}),
        );
        attach_frontend_corpus_details(&mut report, &index);
        let kind_details = report["kinds"][0]["corpus_details"].as_array().unwrap();
        assert_eq!(kind_details.len(), 2);
        assert_eq!(kind_details[0]["label"], "Migrant Stories");
        let inst = &report["kinds"][0]["instances"][0]["corpus_details"][0];
        assert_eq!(inst["alias"], "ud");
        assert_eq!(inst["preferred_backend"], "pando");
    }

    #[test]
    fn corplist_idents_parse() {
        let xml = r#"<?xml version="1.0"?>
<kontext><corplist>
  <corpus ident="ud_pando" sentence_struct="s"/>
  <corpus ident="SUSANNE"/>
</corplist></kontext>"#;
        let ids = parse_kontext_corplist_idents(xml);
        assert!(ids.iter().any(|i| i == "ud_pando"));
        assert!(ids.iter().any(|i| i == "SUSANNE"));
    }

    #[test]
    fn corplist_path_from_config_xml() {
        let dir = std::env::temp_dir().join(format!(
            "fqs-corplist-cfg-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let corplist = dir.join("corplist.xml");
        fs::write(
            &corplist,
            r#"<?xml version="1.0"?><corplist name="root">
  <corpus ident="ud_pando" sentence_struct="s"/>
</corplist>"#,
        )
        .unwrap();
        let config = dir.join("config.xml");
        fs::write(
            &config,
            format!(
                r#"<kontext><plugins><corparch>
            <file extension-by="lindat">{}</file>
            <root_elm_path extension-by="lindat">/corplist</root_elm_path>
        </corparch></plugins></kontext>"#,
                corplist.display()
            ),
        )
        .unwrap();
        let found = corplist_path_from_kontext_config(&config).unwrap();
        assert_eq!(found, corplist);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn coverage_lists_missing_teitok_when_corplist_configured() {
        let dir = std::env::temp_dir().join(format!(
            "fqs-coverage-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let corplist = dir.join("corplist.xml");
        fs::write(
            &corplist,
            r#"<?xml version="1.0"?><corplist name="root">
  <corpus ident="ud_pando" sentence_struct="s"/>
</corplist>"#,
        )
        .unwrap();

        let settings = json!({});
        let caps = json!({});
        let rows = [CoverageCorpus {
            id: "migrantstories",
            label: "Migrant Stories",
            preferred_backend: "pando",
            is_current: true,
            http_policy_mode: "public_query",
            interface_preference: Some("teitok"),
            source_kind: "teitok",
            supports_xml: true,
            project_root: None,
            project_url: Some("https://example/teitok/migrantstories/index.php"),
            settings: &settings,
            capabilities: &caps,
        }];
        let cfg = json!({
            "kind": "kontext",
            "corplist": corplist.display().to_string(),
        });
        let report = kontext_coverage_for_frontend("kontext", &cfg, &rows);
        let missing = report["missing"].as_array().unwrap();
        assert!(
            missing.iter().any(|m| m["id"] == "migrantstories"),
            "expected migrantstories in missing: {report}"
        );
        assert_eq!(report["appendable"], true);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn teitok_corpus_is_kontext_eligible_without_pando_flag() {
        let settings = json!({});
        let caps = json!({});
        let c = CoverageCorpus {
            id: "migrantstories",
            label: "Migrant Stories",
            preferred_backend: "auto",
            is_current: true,
            http_policy_mode: "public_query",
            interface_preference: Some("teitok"),
            source_kind: "teitok",
            supports_xml: true,
            project_root: None,
            project_url: Some("https://example/teitok/migrantstories/index.php"),
            settings: &settings,
            capabilities: &caps,
        };
        assert!(corpus_eligible_for_kontext(&c));
    }

    #[test]
    fn fcs_undecided_only_when_flag_absent() {
        assert_eq!(fcs_enabled_flag(&json!({}), &json!({})), None);
        assert_eq!(
            fcs_enabled_flag(&json!({"fcs": {"enabled": true}}), &json!({})),
            Some(true)
        );
        assert_eq!(
            fcs_enabled_flag(&json!({"fcs": {"enabled": false}}), &json!({})),
            Some(false)
        );
    }

    #[test]
    fn fcs_suggests_teitok_auto_without_explicit_backend() {
        let settings = json!({});
        let caps = json!({});
        let c = CoverageCorpus {
            id: "migrantstories",
            label: "Migrant Stories",
            preferred_backend: "auto",
            is_current: true,
            http_policy_mode: "public_query",
            interface_preference: Some("teitok"),
            source_kind: "teitok",
            supports_xml: true,
            project_root: None,
            project_url: Some("https://example/teitok/migrantstories/index.php"),
            settings: &settings,
            capabilities: &caps,
        };
        assert!(corpus_eligible_for_fcs_suggest(&c));
        let report = compute_frontend_coverage(&[c]);
        let missing = report["fcs"]["missing"].as_array().unwrap();
        assert!(
            missing.iter().any(|m| m["id"] == "migrantstories"),
            "expected migrantstories in FCS missing: {report}"
        );
    }

    #[test]
    fn parse_cargo_toml_version() {
        let body = "[package]\nname = \"fqs\"\nversion = \"0.1.12\"\n";
        assert_eq!(parse_version_from_update_body(body).as_deref(), Some("0.1.12"));
        assert_eq!(
            parse_version_from_update_body(r#"{"version":"0.2.0"}"#).as_deref(),
            Some("0.2.0")
        );
        assert_eq!(semver_is_newer("0.1.12", "0.1.11"), Some(true));
        assert_eq!(semver_is_newer("0.1.11", "0.1.11"), Some(false));
    }

    #[test]
    fn parse_cqp_v_banner() {
        let banner = r#"

The IMS Open Corpus Workbench (CWB)

Copyright (C) 1993-2006 by IMS, University of Stuttgart
Version 3.0 developed by: Stefan Evert

Compiled:  Sun 24 Jul 2022 21:21:08 CEST
Version:   3.5.0
"#;
        assert_eq!(parse_version_banner(banner).as_deref(), Some("3.5.0"));
    }
}
