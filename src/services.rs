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
            notes: "Per-project PHP UI — each corpus has its own site, not one shared frontend.",
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

    out
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
    lower.contains("/teitok/") || lower.contains("teitok.")
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
        );
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].kind, "teitok");
        assert!(!hints[0].centralized);
    }

    #[test]
    fn kontext_settings_yield_corpus_list_hint() {
        let hints = hints_from_catalog_row(
            "ud_pando",
            None,
            None,
            &json!({"kontext": {"url": "http://127.0.0.1:8080/kontext", "corpname": "ud"}}),
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
        );
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].kind, "cqpweb");
        assert!(hints[0].centralized);
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
