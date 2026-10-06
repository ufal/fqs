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

    // hints with an address first, so that one without (a KonText corpname only) can join
    // the instance they make
    let mut ordered: Vec<&FrontendHint> = catalog_hints.iter().collect();
    ordered.sort_by_key(|h| h.url.is_none());
    for hint in ordered {
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
            if o.get("restart").is_none() {
                let kind = normalize_frontend_kind(o.get("kind").and_then(Value::as_str).unwrap_or(""));
                if let Some(r) = default_restart_block(&kind) {
                    o.insert("restart".into(), r);
                }
            }
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

    // processes belong to the card of their frontend (kind and its instances)
    let mut processes = Map::new();
    for k in kinds.iter_mut() {
        let Some(id) = k.get("id").and_then(Value::as_str).map(str::to_string) else { continue };
        let Some(module) = crate::frontends::module_for(&id) else { continue };
        let procs = json!(module.processes());
        processes.insert(id.clone(), procs.clone());
        if let Some(o) = k.as_object_mut() {
            o.insert("processes".into(), procs.clone());
            if let Some(insts) = o.get_mut("instances").and_then(Value::as_array_mut) {
                for i in insts.iter_mut() {
                    if let Some(io) = i.as_object_mut() {
                        io.insert("processes".into(), procs.clone());
                    }
                }
            }
        }
    }
    let kontext_processes = processes.get("kontext").cloned().unwrap_or_else(|| json!([]));
    json!({
        "ok": true,
        "kinds": kinds,
        "frontends": instance_list,
        "kontext_processes": kontext_processes,
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
        let alias = k
            .get("corpname")
            .or_else(|| k.get("corpus"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // a corpus added to a KonText whose address FQS did not know has only a corpname:
        // it still belongs to that KonText (the admin's KonText card); public corpus lists
        // leave URL-less hints out
        if url.is_some() || alias.is_some() {
            out.push(FrontendHint {
                id: match url {
                    Some(u) => format!("kontext:{}", host_key(u)),
                    None => "kontext".into(),
                },
                kind: "kontext".into(),
                label: "KonText".into(),
                url: url.map(|u| u.trim_end_matches('/').to_string()),
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

    // Corpora FCS serves through FQS's /fcs endpoint: the effective `enabled` flag
    // (settings.fcs over capabilities.fcs), the same rule as the endpoint itself.
    if fcs_enabled_flag(settings, capabilities) == Some(true) {
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

pub(crate) fn normalize_frontend_kind(kind: &str) -> String {
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
    let hint_kind = normalize_frontend_kind(&hint.kind);
    let hint_url = hint.url.as_deref().unwrap_or("");
    // configured instances (fqs.json) of the same kind
    let same_kind: Vec<(&String, &Value)> = by_id
        .iter()
        .filter(|(_, v)| {
            v.get("kind")
                .and_then(Value::as_str)
                .map(normalize_frontend_kind)
                .unwrap_or_default()
                == hint_kind
        })
        .collect();
    // the catalogue holds the public address (behind a proxy, say), fqs.json often the
    // internal one (127.0.0.1:8080) next to `public_url`: compare with all of them
    for (id, v) in &same_kind {
        let matches = ["url", "health_url", "public_url"].iter().any(|k| {
            v.get(*k)
                .and_then(Value::as_str)
                .is_some_and(|u| !hint_url.is_empty() && urls_same_service(u, hint_url))
        });
        if matches {
            return (*id).clone();
        }
    }
    // no address: the one instance of this kind there is, if there is just one
    if hint_url.is_empty() && same_kind.len() == 1 {
        return same_kind[0].0.clone();
    }
    // one configured instance of this kind: the catalogue means that one
    let configured: Vec<&String> = same_kind
        .iter()
        .filter(|(_, v)| {
            v.get("source")
                .and_then(Value::as_str)
                .is_some_and(|s| s.starts_with("fqs.json"))
        })
        .map(|(id, _)| *id)
        .collect();
    if configured.len() == 1 {
        return configured[0].clone();
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

pub(crate) fn configured_frontends() -> Vec<(String, Value)> {
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

/// Run a configured restart. Returns JSON result; errors as Err(message).
pub fn restart_frontend(frontend_id: &str) -> Result<Value, String> {
    let configured = configured_frontends().into_iter().find(|(id, _)| id == frontend_id).map(|(_, v)| v);
    let kind = match &configured {
        Some(c) => normalize_frontend_kind(c.get("kind").and_then(Value::as_str).unwrap_or(frontend_id)),
        None => normalize_frontend_kind(frontend_id.split(':').next().unwrap_or(frontend_id)),
    };
    let restart = configured
        .as_ref()
        .and_then(|c| c.get("restart"))
        .cloned()
        .or_else(|| default_restart_block(&kind))
        .ok_or_else(|| {
            format!(
                "frontend '{frontend_id}' cannot be restarted from here: no restart block in {} \
                 and no restart trigger in {} (install-stack.pl sets one up)",
                fqs_config_path().display(),
                restart_trigger_dir().display()
            )
        })?;
    let restart = restart.as_object().ok_or_else(|| format!("frontend '{frontend_id}': restart must be an object"))?;
    run_restart_block(restart, true)
}

/// Folder with restart triggers. FQS runs as an unprivileged user (and with
/// NoNewPrivileges), so it may not run `systemctl restart` itself. install-stack.pl
/// installs, as root, one systemd path unit per unit FQS may restart
/// (`fqs-restart-<unit>.path`, watching `<dir>/<unit>`); FQS writes that file and systemd
/// restarts the unit. Which units can be restarted is decided by root, not by a request.
pub(crate) fn restart_trigger_dir() -> PathBuf {
    std::env::var("FQS_RESTART_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/fqs/restart"))
}

fn valid_unit_name(unit: &str) -> bool {
    !unit.is_empty()
        && unit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '@')
}

/// The trigger file for `unit`, when root set one up.
pub(crate) fn restart_trigger(unit: &str) -> Option<PathBuf> {
    let unit = unit.trim();
    let base = unit.strip_suffix(".service").unwrap_or(unit);
    if !valid_unit_name(base) || base.starts_with('.') {
        return None;
    }
    let p = restart_trigger_dir().join(base);
    p.is_file().then_some(p)
}

/// Restart for a frontend that is not in fqs.json (found on this machine or named by the
/// catalogue): only when root set up a restart trigger for its unit.
pub(crate) fn default_restart_block(kind: &str) -> Option<Value> {
    let unit = match kind {
        "kontext" => "kontext",
        _ => return None,
    };
    restart_trigger(unit).map(|_| json!({ "method": "systemctl", "unit": unit, "via": "restart trigger" }))
}

/// `systemctl show` works without privileges: when the unit last became active.
fn unit_started_at(unit: &str) -> Option<String> {
    let out = Command::new("systemctl")
        .args(["show", "-p", "ActiveEnterTimestampMonotonic", "-p", "ActiveState", unit])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let get = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix('=')))
            .unwrap_or("")
            .trim()
            .to_string()
    };
    Some(format!("{}|{}", get("ActiveEnterTimestampMonotonic"), get("ActiveState")))
}

/// Ask systemd (through the trigger file) to restart `unit`; with `wait`, until the unit
/// is active again (at most 30 s).
fn trigger_restart(unit: &str, trigger: &Path, wait: bool) -> Value {
    let before = unit_started_at(unit);
    let stamp = format!("{}\n", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0));
    if let Err(e) = fs::write(trigger, stamp) {
        return json!({
            "ok": false, "method": "trigger", "unit": unit, "trigger": trigger.display().to_string(),
            "error": format!("cannot write the restart trigger {}: {e}", trigger.display()),
        });
    }
    if !wait {
        return json!({ "ok": true, "method": "trigger", "unit": unit,
            "trigger": trigger.display().to_string(), "confirmed": false });
    }
    let Some(before) = before else {
        return json!({ "ok": true, "method": "trigger", "unit": unit,
            "trigger": trigger.display().to_string(), "confirmed": false,
            "note": "requested; systemctl show is not available to confirm it" });
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        if let Some(now) = unit_started_at(unit) {
            if now != before && now.ends_with("|active") {
                return json!({ "ok": true, "method": "trigger", "unit": unit,
                    "trigger": trigger.display().to_string(), "confirmed": true });
            }
        }
    }
    json!({
        "ok": false, "method": "trigger", "unit": unit, "trigger": trigger.display().to_string(),
        "error": format!(
            "restart of {unit} was requested, but it did not come back within 30 s: \
             check `systemctl status fqs-restart-{unit}.path fqs-restart@{unit}.service {unit}`"
        ),
    })
}

/// Restart this FQS process via the optional `fqs.restart` block in fqs.json.
pub fn restart_fqs() -> Result<Value, String> {
    let restart = fqs_self_config()
        .get("restart")
        .and_then(|v| v.as_object())
        .cloned()
        .or_else(|| {
            restart_trigger("fqs").map(|_| {
                let mut m = Map::new();
                m.insert("method".into(), json!("systemctl"));
                m.insert("unit".into(), json!("fqs"));
                m
            })
        })
        .ok_or_else(|| {
            format!(
                "no fqs.restart block in {} — configure e.g. {{\"fqs\":{{\"restart\":{{\"method\":\"systemctl\",\"unit\":\"fqs\"}}}}}}",
                fqs_config_path().display()
            )
        })?;
    // this process is what gets restarted: do not wait for it
    run_restart_block(&restart, false)
}

/// Inventory for this FQS binary: version, update check, restartability.
pub fn probe_fqs_self(server_name: Option<&str>) -> Value {
    let version = env!("CARGO_PKG_VERSION").to_string();
    let cfg = fqs_self_config();
    let restartable = cfg.get("restart").and_then(|v| v.as_object()).is_some() || restart_trigger("fqs").is_some();
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
            "Configure fqs.restart in fqs.json, or run install-stack.pl (it sets up a restart trigger), to enable Restart from the admin UI."
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

fn run_restart_block(restart: &serde_json::Map<String, Value>, wait: bool) -> Result<Value, String> {
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
            // set up by root: the way an unprivileged FQS can restart the unit
            if action != "reload" {
                if let Some(trigger) = restart_trigger(unit) {
                    return Ok(trigger_restart(unit, &trigger, wait));
                }
            }
            let out = Command::new("systemctl")
                .args([action, unit])
                .output()
                .map_err(|e| e.to_string())?;
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let denied = !out.status.success()
                && (stderr.contains("authentication") || stderr.contains("Access denied") || stderr.contains("not allowed"));
            Ok(json!({
                "ok": out.status.success(),
                "method": "systemctl",
                "unit": unit,
                "action": action,
                "stdout": String::from_utf8_lossy(&out.stdout),
                "stderr": stderr,
                "exit_code": out.status.code(),
                "hint": if denied {
                    format!("FQS may not restart {unit} itself; install-stack.pl sets up a restart trigger ({}/{})",
                        restart_trigger_dir().display(), unit.strip_suffix(".service").unwrap_or(unit))
                } else { String::new() },
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
pub use crate::frontends::CatalogCorpus as CoverageCorpus;
use crate::frontends::{corpus_can_serve_fcs, corpus_looks_pando_servable, corpus_servable_through_fqs};

/// Compare the FQS catalogue with what each frontend has (through its frontend module),
/// and list the corpora with no FCS decision yet.
pub fn compute_frontend_coverage(corpora: &[CoverageCorpus<'_>]) -> Value {
    let mut reports: Vec<Value> = Vec::new();
    let configured = configured_frontends();
    for module in crate::frontends::modules() {
        let mut found = false;
        for (id, cfg) in &configured {
            let kind = normalize_frontend_kind(cfg.get("kind").and_then(Value::as_str).unwrap_or(""));
            if kind != module.kind() && !id.eq_ignore_ascii_case(module.kind()) {
                continue;
            }
            found = true;
            reports.push(module.coverage(id, cfg, corpora));
        }
        // none in fqs.json: one found on this machine (or named in the catalogue)
        if !found {
            if let Some(cfg) = module.discover(corpora) {
                reports.push(module.coverage(module.kind(), &cfg, corpora));
            }
        }
    }
    let kontext_reports: Vec<Value> = reports
        .iter()
        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("kontext"))
        .cloned()
        .collect();

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

    let modules: Vec<Value> = crate::frontends::modules()
        .iter()
        .map(|m| json!({ "kind": m.kind(), "label": m.label() }))
        .collect();
    json!({
        "ok": true,
        "modules": modules,
        "frontends": reports,
        // KonText only, for older admin UI builds
        "kontext": kontext_reports,
        "fcs": {
            "missing": missing_fcs.clone(),
            // Alias kept for older admin UI builds.
            "undecided": missing_fcs,
        },
        "help": {
            "kontext_corplist": "KonText: optionally set frontends[].corplist, pando_corpora, registry, manatee_data, manatee_vert, encodevert, public_url and fqs_url in fqs.json. Otherwise FQS looks where KonText, kontext-pando and Manatee keep them (/opt/kontext/conf, config.xml, /var/lib/manatee).",
            "fcs": "Add to FCS sets settings.fcs.enabled=true; Exclude sets false.",
        },
    })
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
        return corpus_servable_through_fqs(c) || corpus_looks_pando_servable(c);
    }
    corpus_servable_through_fqs(c)
}


pub(crate) fn fcs_enabled_flag(settings: &Value, capabilities: &Value) -> Option<bool> {
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



/// The files and folders the frontend modules write, for the frontends in fqs.json or
/// found on this machine: `{"paths": [{"frontend", "path", "dir", "exists"}]}`.
pub fn frontend_write_paths() -> Value {
    let configured = configured_frontends();
    let mut out = Vec::new();
    for module in crate::frontends::modules() {
        let mut cfgs: Vec<(String, Value)> = configured
            .iter()
            .filter(|(id, cfg)| {
                normalize_frontend_kind(cfg.get("kind").and_then(Value::as_str).unwrap_or("")) == module.kind()
                    || id.eq_ignore_ascii_case(module.kind())
            })
            .cloned()
            .collect();
        if cfgs.is_empty() {
            if let Some(c) = module.discover(&[]) {
                cfgs.push((module.kind().to_string(), c));
            }
        }
        for (id, cfg) in cfgs {
            for (path, dir) in module.write_paths(&cfg) {
                out.push(json!({ "frontend": id, "path": path.display().to_string(), "dir": dir, "exists": path.exists() }));
            }
        }
    }
    json!({ "paths": out })
}

/// Publish a catalogue corpus to a frontend through its frontend module.
pub fn publish_to_frontend(
    frontend_id: &str,
    req: &crate::frontends::PublishRequest<'_>,
) -> Result<Value, String> {
    let cfg = configured_frontends()
        .into_iter()
        .find(|(id, _)| id == frontend_id)
        .map(|(_, v)| v);
    let kind = match &cfg {
        Some(c) => normalize_frontend_kind(c.get("kind").and_then(Value::as_str).unwrap_or(frontend_id)),
        None => normalize_frontend_kind(frontend_id),
    };
    let module = crate::frontends::module_for(&kind)
        .or_else(|| crate::frontends::module_for(frontend_id))
        .ok_or_else(|| format!("FQS has no frontend module for '{frontend_id}' (kind '{kind}')"))?;
    if cfg.is_none() && !frontend_id.eq_ignore_ascii_case(module.kind()) {
        return Err(format!("frontend '{frontend_id}' is not in fqs.json"));
    }
    module.publish(frontend_id, cfg.as_ref(), req)
}

#[cfg(test)]
mod tests {

    #[test]
    fn kontext_corpus_with_only_a_corpname_joins_the_kontext_card() {
        let with_url = hints_from_catalog_row("ntrex", None, None,
            &json!({"kontext": {"corpname": "ntrex", "public_url": "https://lindat.cz/services/test-kontext"}}),
            "pando", false, None, &json!({}));
        let no_url = hints_from_catalog_row("migrantstories", None, None,
            &json!({"kontext": {"corpname": "migrantstories"}}), "pando", false, None, &json!({}));
        assert_eq!(no_url.len(), 1);
        assert!(no_url[0].url.is_none());
        // the URL-less one comes first in the catalogue, and still joins the instance
        let mut all = no_url.clone();
        all.extend(with_url);
        let report = probe_frontends(&all);
        let text = report.to_string();
        let kontext = report["kinds"].as_array().unwrap().iter().find(|k| k["id"] == "kontext").unwrap();
        let instances = kontext["instances"].as_array().unwrap();
        assert_eq!(instances.len(), 1, "{text}");
        let corpora: Vec<&str> = instances[0]["corpora"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert_eq!(corpora, vec!["migrantstories", "ntrex"]);
        // nothing at all: no hint
        assert!(hints_from_catalog_row("x", None, None, &json!({"kontext": {}}), "pando", false, None, &json!({})).is_empty());
    }

    #[test]
    fn restart_triggers_are_only_what_root_set_up() {
        let dir = std::env::temp_dir().join(format!("fqs-restart-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("FQS_RESTART_DIR", &dir) };
        // nothing set up: no restart for a discovered KonText
        assert!(restart_trigger("kontext").is_none());
        assert!(default_restart_block("kontext").is_none());
        fs::write(dir.join("kontext"), "").unwrap();
        assert_eq!(restart_trigger("kontext.service"), Some(dir.join("kontext")));
        let block = default_restart_block("kontext").unwrap();
        assert_eq!(block["unit"], "kontext");
        assert!(default_restart_block("teitok").is_none());
        // names that are not unit names never reach the file system
        assert!(restart_trigger("../kontext").is_none());
        assert!(restart_trigger(".hidden").is_none());
        // without waiting: the trigger file gets a time stamp
        let r = trigger_restart("kontext", &dir.join("kontext"), false);
        assert_eq!(r["ok"], true);
        assert!(!fs::read_to_string(dir.join("kontext")).unwrap().trim().is_empty());
        // a restart of a catalogue KonText uses the trigger
        let r = restart_frontend("kontext:lindat.cz").unwrap();
        assert_eq!(r["method"], "trigger");
        unsafe { std::env::remove_var("FQS_RESTART_DIR") };
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn catalog_hint_merges_into_configured_instance_by_public_url() {
        let mut by_id = Map::new();
        by_id.insert(
            "kontext".into(),
            json!({"id":"kontext","kind":"kontext","source":"fqs.json",
                   "url":"http://127.0.0.1:8080","public_url":"https://lindat.cz/services/test-kontext",
                   "restart":{"method":"systemctl","unit":"kontext"}}),
        );
        let hint = FrontendHint {
            id: "kontext:lindat.cz".into(),
            kind: "kontext".into(),
            label: "KonText".into(),
            url: Some("https://lindat.cz/services/test-kontext".into()),
            corpus_id: "ntrex".into(),
            corpus_alias: None,
            centralized: true,
        };
        assert_eq!(resolve_frontend_merge_id(&by_id, &hint), "kontext");
        // no public_url, but the only configured KonText: still that one
        by_id.get_mut("kontext").unwrap().as_object_mut().unwrap().remove("public_url");
        assert_eq!(resolve_frontend_merge_id(&by_id, &hint), "kontext");
        // two configured ones and no url match: a separate catalogue instance
        by_id.insert(
            "kontext2".into(),
            json!({"id":"kontext2","kind":"kontext","source":"fqs.json","url":"http://127.0.0.1:8081"}),
        );
        assert_eq!(resolve_frontend_merge_id(&by_id, &hint), "kontext:lindat.cz");
    }
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

    #[test]
    fn fcs_hint_follows_effective_enabled_flag() {
        let kinds = |settings: Value, caps: Value| {
            hints_from_catalog_row("c", None, None, &settings, "pando", false, None, &caps)
                .into_iter()
                .map(|h| h.kind)
                .collect::<Vec<_>>()
        };
        assert!(!kinds(json!({"fcs": {"enabled": false}}), json!({})).contains(&"fcs".to_string()));
        assert!(kinds(json!({}), json!({"fcs": {"enabled": true}})).contains(&"fcs".to_string()));
        // excluded in the admin overrides the registration's capabilities
        assert!(!kinds(json!({"fcs": {"enabled": false}}), json!({"fcs": {"enabled": true}})).contains(&"fcs".to_string()));
    }

}
