use std::fs;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
use std::collections::HashMap;

use anyhow::{Context, Result};
use axum::extract::{Query as AxumQuery, State};
use axum::http::header;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum::{extract::Request, response::Response};
use clap::{Args, Parser, Subcommand};
use rusqlite::{Connection, Error as SqliteError, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::time::sleep;

#[derive(Parser, Debug)]
#[command(
    name = "fqs",
    about = "FlexiCorp Query Server prototype CLI",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Initialize the SQLite catalog (safe to run repeatedly)
    Init(DbPathArg),
    /// List corpus entries from a manifest
    Corpora(CorporaArgs),
    /// Run a prototype query against a corpus entry
    Query(QueryArgs),
    /// Run HTTP server mode for query/catalog testing
    Serve(ServeArgs),
    /// Check DB + HTTP server health (without starting server)
    Status(StatusArgs),
    /// Reindex queue/history scaffolding (control-plane)
    Reindex(ReindexArgs),
}

#[derive(Args, Debug)]
struct CorporaArgs {
    #[command(subcommand)]
    action: CorporaAction,
}

#[derive(Args, Debug)]
struct ReindexArgs {
    #[command(subcommand)]
    action: ReindexAction,
}

#[derive(Subcommand, Debug)]
enum ReindexAction {
    /// Enqueue a reindex job (scaffolding only; worker dispatch follows in next phase)
    Enqueue(ReindexEnqueueArgs),
    /// List queued/running jobs
    Queue(ReindexQueueArgs),
    /// Show reindex history (includes completion timestamps)
    History(ReindexHistoryArgs),
    /// Mark a job started (worker scaffolding hook)
    MarkStarted(ReindexMarkStartedArgs),
    /// Mark a job finished (worker scaffolding hook)
    MarkFinished(ReindexMarkFinishedArgs),
    /// Dispatch queued jobs to healthy workers once (scaffolding scheduler tick)
    DispatchOnce(ReindexDispatchOnceArgs),
    /// Worker heartbeat (CLI/testing hook)
    WorkerHeartbeat(ReindexWorkerHeartbeatArgs),
}

#[derive(Args, Debug)]
struct ReindexEnqueueArgs {
    /// Corpus id from FQS catalog
    #[arg(long)]
    corpus: String,
    /// Comma-separated backend targets (e.g. pando,cqp,clickhouse)
    #[arg(long)]
    backends: Option<String>,
    /// Scheduling priority (higher number = sooner)
    #[arg(long, default_value_t = 0)]
    priority: i64,
    /// Origin tag for diagnostics (e.g. teitok, cli, api)
    #[arg(long, default_value = "cli")]
    origin: String,
    /// Request role (admin required for enqueue)
    #[arg(long, default_value = "admin")]
    request_role: String,
    /// Optional note/message
    #[arg(long)]
    note: Option<String>,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct ReindexQueueArgs {
    /// Optional status filter: queued|running|completed|failed|cancelled
    #[arg(long)]
    status: Option<String>,
    /// Optional corpus id filter
    #[arg(long)]
    corpus: Option<String>,
    /// Max rows
    #[arg(long, default_value_t = 100)]
    limit: usize,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct ReindexHistoryArgs {
    /// Optional corpus id filter
    #[arg(long)]
    corpus: Option<String>,
    /// Max rows
    #[arg(long, default_value_t = 200)]
    limit: usize,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct ReindexMarkStartedArgs {
    #[arg(long)]
    job_id: String,
    #[arg(long)]
    worker_id: Option<String>,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct ReindexMarkFinishedArgs {
    #[arg(long)]
    job_id: String,
    #[arg(long, default_value_t = false)]
    ok: bool,
    #[arg(long)]
    message: Option<String>,
    #[arg(long)]
    error: Option<String>,
    /// Optional result JSON string
    #[arg(long)]
    result_json: Option<String>,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct ReindexDispatchOnceArgs {
    /// Default max concurrent jobs per worker when worker heartbeat does not set it
    #[arg(long, default_value_t = 1)]
    default_worker_max_concurrent: i64,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct ReindexWorkerHeartbeatArgs {
    #[arg(long)]
    worker_id: String,
    #[arg(long, default_value_t = 1)]
    max_concurrent: i64,
    /// Optional host label for diagnostics
    #[arg(long)]
    host: Option<String>,
    /// Optional capabilities CSV (e.g. pando,cqp,clickhouse)
    #[arg(long)]
    capabilities: Option<String>,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Subcommand, Debug)]
enum CorporaAction {
    /// List all known corpora
    List(ListCorporaArgs),
    /// Show one corpus by id
    Show(ShowByIdArgs),
    /// Insert one corpus (fails if id exists unless --force)
    Add(AddCorpusArgs),
    /// Insert or update corpus entries from JSON object/array (reports new vs updated ids)
    UpsertJson(UpsertJsonArgs),
    /// Export corpus entries as JSON object array
    ExportJson(ExportJsonArgs),
    /// Validate corpus registry entries and optionally run query probes
    Validate(ValidateArgs),
    /// Mark one corpus as superseded (hidden by default list)
    Supersede(ShowByIdArgs),
    /// Remove one corpus row from the catalogue (requires --force)
    Delete(DeleteCorpusArgs),
}

#[derive(Args, Debug)]
struct ShowByIdArgs {
    /// Corpus id to inspect
    #[arg(long)]
    id: String,

    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct DeleteCorpusArgs {
    /// Corpus id to remove
    #[arg(long)]
    id: String,
    /// Required to confirm destructive delete
    #[arg(long, default_value_t = false)]
    force: bool,

    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct QueryArgs {
    /// Corpus id to query
    #[arg(long)]
    corpus: String,
    /// Query string (for now stored in response only)
    #[arg(long = "q")]
    query_text: String,
    /// Optional language hint
    #[arg(long, default_value = "auto")]
    language: String,
    /// Optional start offset
    #[arg(long, default_value_t = 0)]
    start: u32,
    /// Optional page size
    #[arg(long, default_value_t = 25)]
    size: u32,
    /// Override catalogue backend for this request (pando | cqp)
    #[arg(long)]
    backend: Option<String>,

    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct ServeArgs {
    /// Bind host/IP for HTTP server
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Bind port for HTTP server
    #[arg(long, default_value_t = 8787)]
    port: u16,
    /// FCS database name shown in SRU explain
    #[arg(long, default_value = "fqs-endpoint")]
    fcs_database: String,
    /// Test mode: report policy blocks but still execute queries
    #[arg(long, default_value_t = false)]
    test: bool,
    /// Plaintext request log path (default: OS standard fqs log path)
    #[arg(long)]
    log_file: Option<PathBuf>,
    /// Rotate request log after this many bytes
    #[arg(long, default_value_t = 10 * 1024 * 1024)]
    log_max_bytes: u64,
    /// Number of rotated files to keep (fqs.log.1 ... fqs.log.N)
    #[arg(long, default_value_t = 7)]
    log_keep_files: usize,
    /// Consider sessions stale after N minutes of inactivity
    #[arg(long, default_value_t = 120)]
    session_ttl_minutes: i64,
    /// Human-readable label for this instance (shown in /health; e.g. "LINDAT live corpus query server")
    #[arg(long = "server-name", env = "FQS_SERVER_NAME")]
    server_name: Option<String>,
    /// Restart mode: terminate matching existing `fqs serve --host/--port` process before bind
    #[arg(long, default_value_t = false)]
    restart: bool,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct StatusArgs {
    /// Probe URL base (example: http://127.0.0.1:8787)
    #[arg(long)]
    url: Option<String>,
    /// Probe host when --url is omitted
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Probe port when --url is omitted
    #[arg(long, default_value_t = 8787)]
    port: u16,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Debug, Deserialize)]
struct HttpQueryRequest {
    corpus: String,
    query: String,
    language: Option<String>,
    start: Option<u32>,
    size: Option<u32>,
    window: Option<u32>,
    context_scope: Option<String>,
    context_format: Option<String>,
    flexicorp_fragment_kwic_cpos_span: Option<bool>,
    /// Override FQS backend for this request (pando | cqp) — TEITOK/flexicorp should set from project config
    backend: Option<String>,
    request_role: Option<String>,
    /// Optional caller-generated session id for activity tracking
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HttpCorporaQuery {
    environment: Option<String>,
    include_noncurrent: Option<bool>,
    request_role: Option<String>,
    /// Filter by browse tag (case-insensitive)
    tag: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HttpReindexJobsQuery {
    status: Option<String>,
    corpus: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct HttpReindexEnqueueRequest {
    corpus: String,
    backends: Option<Vec<String>>,
    priority: Option<i64>,
    request_role: Option<String>,
    origin: Option<String>,
    note: Option<String>,
    options: Option<HashMap<String, String>>,
    backend_options: Option<HashMap<String, HashMap<String, String>>>,
}

#[derive(Debug, Deserialize)]
struct HttpReindexWorkerHeartbeatRequest {
    worker_id: String,
    max_concurrent: Option<i64>,
    host: Option<String>,
    capabilities: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct HttpReindexMarkStartedRequest {
    job_id: String,
    worker_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HttpReindexMarkFinishedRequest {
    job_id: String,
    ok: Option<bool>,
    message: Option<String>,
    error: Option<String>,
    result: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct FcsQuery {
    operation: Option<String>,
    query: Option<String>,
    #[serde(rename = "x-corpus")]
    x_corpus: Option<String>,
    #[serde(rename = "x-fcs-context")]
    x_fcs_context: Option<String>,
    request_role: Option<String>,
    #[serde(rename = "x-fcs-endpoint-description")]
    x_fcs_endpoint_description: Option<bool>,
    #[serde(rename = "startRecord")]
    start_record: Option<u32>,
    #[serde(rename = "maximumRecords")]
    maximum_records: Option<u32>,
}

#[derive(Clone)]
struct HttpAppState {
    db_path: PathBuf,
    test_mode: bool,
    host: String,
    port: u16,
    fcs_database: String,
    server_name: Option<String>,
    request_log_path: PathBuf,
    request_log_max_bytes: u64,
    request_log_keep_files: usize,
}

#[derive(Args, Debug)]
struct DbPathArg {
    /// Path to SQLite catalog database
    #[arg(long)]
    db: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct AddCorpusArgs {
    /// Stable corpus identifier (unique)
    #[arg(long)]
    id: String,
    /// Human label
    #[arg(long)]
    label: String,
    /// Absolute or project-relative path
    #[arg(long)]
    project_root: PathBuf,
    /// Canonical corpus/project URL (TEITOK page base)
    #[arg(long)]
    project_url: Option<String>,
    /// FQS execution hint: pando | cqp | auto (auto resolves from settings.query_backend or index paths)
    #[arg(long, default_value = "auto")]
    preferred_backend: String,
    /// Deployment lane: dev/stable/live/etc.
    #[arg(long, default_value = "live")]
    environment: String,
    /// Lifecycle state: draft/staging/published
    #[arg(long, default_value = "published")]
    visibility: String,
    /// Listing policy: public/auth/corpus_admin/server_admin
    #[arg(long, default_value = "public")]
    listing_visibility: String,
    /// Optional family key (for grouped corpora sets)
    #[arg(long)]
    family_key: Option<String>,
    /// Optional family display label
    #[arg(long)]
    family_label: Option<String>,
    /// Optional version tag (e.g. live, stable — deployment lane)
    #[arg(long)]
    version_tag: Option<String>,
    /// Content / publication version (semver, date, etc.) — one catalogue row per corpus + version
    #[arg(long)]
    corpus_version: Option<String>,
    /// Preferred UI surface (teitok, flexicorp, fqs, kontext, …) — routing is done outside FQS
    #[arg(long)]
    interface_preference: Option<String>,
    /// Browse/filter tags (repeat for multiple; Kontext-style facets)
    #[arg(long = "tag", value_name = "TAG")]
    tags: Vec<String>,
    /// Mark this entry as superseded/non-current
    #[arg(long, default_value_t = false)]
    superseded: bool,

    /// Overwrite an existing corpus with the same id (otherwise add fails)
    #[arg(long, default_value_t = false)]
    force: bool,

    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
#[command(group(
    clap::ArgGroup::new("json_input")
        .required(true)
        .args(["json", "json_file", "stdin"])
))]
struct UpsertJsonArgs {
    /// Inline JSON object or array
    #[arg(long)]
    json: Option<String>,
    /// Read JSON object or array from file
    #[arg(long = "json-file")]
    json_file: Option<PathBuf>,
    /// Read JSON object or array from stdin
    #[arg(long, default_value_t = false)]
    stdin: bool,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct ListCorporaArgs {
    /// Filter by environment label (exact match; any string stored per corpus, e.g. dev/live/stable)
    #[arg(long)]
    environment: Option<String>,
    /// Keep only corpora that have this browse tag (case-insensitive)
    #[arg(long)]
    tag: Option<String>,
    /// Include superseded corpora
    #[arg(long, default_value_t = false)]
    include_noncurrent: bool,
    /// Group output by family key
    #[arg(long, default_value_t = false)]
    group_by_family: bool,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct ExportJsonArgs {
    /// Filter by environment label (exact match)
    #[arg(long)]
    environment: Option<String>,
    /// Include superseded corpora
    #[arg(long, default_value_t = false)]
    include_noncurrent: bool,
    /// Write JSON to file instead of stdout
    #[arg(long)]
    output: Option<PathBuf>,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct ValidateArgs {
    /// Validate only one corpus id
    #[arg(long)]
    id: Option<String>,
    /// Filter by environment label (exact match)
    #[arg(long)]
    environment: Option<String>,
    /// Include superseded corpora
    #[arg(long, default_value_t = false)]
    include_noncurrent: bool,
    /// Run deeper backend probes (query-level when configured)
    #[arg(long, default_value_t = false)]
    full: bool,
    /// In full mode, fail corpus validation when query probe is unavailable
    #[arg(long, default_value_t = false)]
    strict_full: bool,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CorpusEntry {
    id: String,
    label: String,
    project_root: PathBuf,
    project_url: Option<String>,
    preferred_backend: String,
    #[serde(default = "default_visibility")]
    visibility: String,
    #[serde(default = "default_listing_visibility")]
    listing_visibility: String,
    /// Deployment / host bucket for filtering (free-form; e.g. dev, live, stable).
    #[serde(default = "default_environment")]
    environment: String,
    family_key: Option<String>,
    family_label: Option<String>,
    version_tag: Option<String>,
    /// Content / publication version (distinct from version_tag deployment lane)
    corpus_version: Option<String>,
    /// Where the user should open the corpus (TEITOK/flexicorp picks the engine)
    interface_preference: Option<String>,
    #[serde(default = "default_source_kind")]
    source_kind: String,
    #[serde(default = "default_supports_xml")]
    supports_xml: bool,
    #[serde(default = "default_http_policy_mode")]
    http_policy_mode: String,
    #[serde(default = "default_http_allowed_operations")]
    http_allowed_operations: Vec<String>,
    #[serde(default = "default_interfaces")]
    interfaces: Vec<String>,
    /// Kontext-style browse tags (facets for filtering lists)
    #[serde(default = "default_labels")]
    labels: Vec<String>,
    #[serde(default = "default_empty_object")]
    capabilities: Value,
    #[serde(default = "default_empty_object")]
    settings: Value,
    first_corpus_update_at: Option<String>,
    last_corpus_update_at: Option<String>,
    corpus_size: Option<i64>,
    corpus_size_updated_at: Option<String>,
    last_validated_at: Option<String>,
    last_validation_ok: Option<bool>,
    last_validation_message: Option<String>,
    #[serde(default = "default_is_current")]
    is_current: bool,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReindexJobEntry {
    job_id: String,
    corpus_id: String,
    status: String,
    priority: i64,
    requested_backends: Vec<String>,
    requested_by_role: Option<String>,
    origin: Option<String>,
    message: Option<String>,
    last_error: Option<String>,
    worker_id: Option<String>,
    requested_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
    updated_at: String,
    request: Value,
    result: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReindexHistoryEntry {
    id: i64,
    corpus_id: String,
    job_id: Option<String>,
    event: String,
    at: String,
    details: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReindexWorkerEntry {
    worker_id: String,
    status: String,
    max_concurrent: i64,
    host: Option<String>,
    capabilities: Vec<String>,
    running_jobs: i64,
    last_heartbeat_at: String,
    created_at: String,
    updated_at: String,
}

fn default_environment() -> String {
    "live".to_string()
}

fn default_visibility() -> String {
    "published".to_string()
}

fn default_listing_visibility() -> String {
    "public".to_string()
}

fn default_is_current() -> bool {
    true
}

fn default_source_kind() -> String {
    "generic".to_string()
}

fn default_supports_xml() -> bool {
    false
}

fn default_http_policy_mode() -> String {
    "public_query".to_string()
}

fn default_http_allowed_operations() -> Vec<String> {
    vec!["query".to_string(), "catalog".to_string()]
}

fn default_interfaces() -> Vec<String> {
    Vec::new()
}

fn default_labels() -> Vec<String> {
    Vec::new()
}

/// Dedupe case-insensitively; keep first spelling.
fn normalize_browse_labels(tags: &[String]) -> Vec<String> {
    use std::collections::HashSet;
    let mut seen = HashSet::<String>::new();
    let mut out = Vec::new();
    for t in tags {
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        let key = t.to_ascii_lowercase();
        if seen.insert(key) {
            out.push(t.to_string());
        }
    }
    out
}

fn default_empty_object() -> Value {
    json!({})
}

fn runtime_health_path_for_base(base_path: &str) -> String {
    if base_path == "/" {
        "/health".to_string()
    } else if base_path.ends_with("/health") {
        base_path.to_string()
    } else {
        format!("{}/health", base_path.trim_end_matches('/'))
    }
}

fn runtime_entry_is_healthy(url: &str) -> bool {
    if let Some((host, port, base_path)) = parse_http_url_target(url) {
        let health_path = runtime_health_path_for_base(&base_path);
        if let Ok((ok, _, _)) = probe_http_health_details(&host, port, &health_path) {
            return ok;
        }
    }
    false
}

fn discover_db_path_from_runtime_files() -> Option<PathBuf> {
    let candidates = vec![
        PathBuf::from("/usr/local/var/fqs/fqs-http.json"),
        PathBuf::from("/var/lib/fqs/fqs-http.json"),
    ];
    for path in candidates {
        let s = match fs::read_to_string(&path) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let v: Value = match serde_json::from_str(&s) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let Some(db_path) = v
            .get("db_path")
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty()) else {
            continue;
        };
        let url = v
            .get("url")
            .and_then(|x| x.as_str())
            .map(str::trim)
            .unwrap_or("");
        if url.is_empty() || runtime_entry_is_healthy(url) {
            return Some(PathBuf::from(db_path));
        }
    }
    None
}

fn resolve_db_path(arg: &DbPathArg) -> PathBuf {
    if let Some(path) = &arg.db {
        return path.clone();
    }
    if let Ok(path) = std::env::var("FQS_DB_PATH") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    if let Some(path) = discover_db_path_from_runtime_files() {
        return path;
    }
    default_db_path()
}

/// Sidecar JSON written on successful `fqs serve` bind so clients (TEITOK PHP, shell) can discover
/// the HTTP base URL without hard-coding port 8787. Lives next to the catalog DB.
fn fqs_http_runtime_path(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("fqs-http.json")
}

/// Host to advertise in `fqs-http.json` for same-machine consumers (e.g. PHP-FPM → loopback).
fn loopback_url_host(bind_host: &str) -> String {
    match bind_host.trim() {
        "" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1".to_string(),
        h => h.to_string(),
    }
}

fn read_fqs_http_runtime_url(db_path: &Path) -> Option<String> {
    let path = fqs_http_runtime_path(db_path);
    let s = fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&s).ok()?;
    v.get("url")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn write_fqs_http_runtime_file(db_path: &Path, bind_host: &str, port: u16) -> Result<()> {
    let host = loopback_url_host(bind_host);
    let url = format!("http://{}:{}", host, port);
    let path = fqs_http_runtime_path(db_path);
    let ts = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "".to_string());
    let v = json!({
        "url": url,
        "updated_at": ts,
        "db_path": db_path.to_string_lossy(),
    });
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_string_pretty(&v)?)?;
    Ok(())
}

fn default_db_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.trim().is_empty() {
                return PathBuf::from(appdata).join("fqs").join("fqs.db");
            }
        }
        return PathBuf::from(r"C:\ProgramData\fqs\fqs.db");
    }

    #[cfg(target_os = "macos")]
    {
        return PathBuf::from("/usr/local/var/fqs/fqs.db");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return PathBuf::from("/var/lib/fqs/fqs.db");
    }

    #[cfg(not(any(unix, target_os = "windows")))]
    PathBuf::from("fqs.db")
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init(db) => {
            let db_path = resolve_db_path(&db);
            let _ = open_db(&db_path)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "db_path": db_path,
                    "initialized": true
                }))?
            );
        }
        Command::Corpora(args) => handle_corpora(args)?,
        Command::Query(args) => handle_query(args)?,
        Command::Serve(args) => run_http_server(args).await?,
        Command::Status(args) => handle_status(args)?,
        Command::Reindex(args) => handle_reindex(args)?,
    }
    Ok(())
}

fn handle_corpora(args: CorporaArgs) -> Result<()> {
    match args.action {
        CorporaAction::List(list) => {
            let conn = open_db(&resolve_db_path(&list.db))?;
            let corpora = list_corpora(
                &conn,
                list.environment.as_deref(),
                list.include_noncurrent,
                list.tag.as_deref(),
            )?;
            if list.group_by_family {
                let mut grouped = std::collections::BTreeMap::<String, Vec<CorpusEntry>>::new();
                let mut ungrouped: Vec<CorpusEntry> = Vec::new();
                for corpus in corpora {
                    if let Some(key) = corpus.family_key.clone() {
                        grouped.entry(key).or_default().push(corpus);
                    } else {
                        ungrouped.push(corpus);
                    }
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "grouped": grouped,
                        "ungrouped": ungrouped
                    }))?
                );
            } else {
                println!("{}", serde_json::to_string_pretty(&corpora)?);
            }
        }
        CorporaAction::Show(show) => {
            let conn = open_db(&resolve_db_path(&show.db))?;
            let corpus = get_corpus(&conn, &show.id)?;
            println!("{}", serde_json::to_string_pretty(&corpus)?);
        }
        CorporaAction::Add(add) => {
            let conn = open_db(&resolve_db_path(&add.db))?;
            let existed_before = corpus_exists(&conn, &add.id)?;
            if !add.force && existed_before {
                anyhow::bail!(
                    "Corpus id '{}' already exists. To change fields, use `fqs corpora upsert-json` with JSON from `corpora show`, or pass `--force` to replace this entry from CLI flags.",
                    add.id
                );
            }
            let entry = CorpusEntry {
                id: add.id,
                label: add.label,
                project_root: add.project_root,
                project_url: add.project_url,
                preferred_backend: add.preferred_backend,
                visibility: add.visibility,
                listing_visibility: add.listing_visibility,
                environment: add.environment,
                family_key: add.family_key,
                family_label: add.family_label,
                version_tag: add.version_tag,
                corpus_version: add.corpus_version,
                interface_preference: add.interface_preference,
                source_kind: "generic".to_string(),
                supports_xml: false,
                http_policy_mode: default_http_policy_mode(),
                http_allowed_operations: default_http_allowed_operations(),
                interfaces: vec![],
                labels: normalize_browse_labels(&add.tags),
                capabilities: json!({}),
                settings: json!({}),
                first_corpus_update_at: None,
                last_corpus_update_at: None,
                corpus_size: None,
                corpus_size_updated_at: None,
                last_validated_at: None,
                last_validation_ok: None,
                last_validation_message: None,
                is_current: !add.superseded,
                created_at: None,
                updated_at: None,
            };
            upsert_corpus(&conn, &entry)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "operation": if add.force && existed_before { "corpora_add_replace" } else { "corpora_add" },
                    "replaced": add.force && existed_before,
                    "corpus": entry
                }))?
            );
        }
        CorporaAction::Supersede(args) => {
            let conn = open_db(&resolve_db_path(&args.db))?;
            mark_corpus_superseded(&conn, &args.id)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "operation": "corpora_supersede",
                    "id": args.id
                }))?
            );
        }
        CorporaAction::Delete(args) => {
            if !args.force {
                anyhow::bail!(
                    "Refusing to delete corpus '{}' without --force (destructive).",
                    args.id
                );
            }
            let conn = open_db(&resolve_db_path(&args.db))?;
            let n = delete_corpus(&conn, &args.id)?;
            if n == 0 {
                anyhow::bail!("Corpus '{}' not found in database", args.id);
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "operation": "corpora_delete",
                    "id": args.id,
                    "deleted": n
                }))?
            );
        }
        CorporaAction::UpsertJson(args) => {
            let conn = open_db(&resolve_db_path(&args.db))?;
            let payload = read_json_input(&args)?;
            let entries = parse_entries_from_json(&payload)?;
            let mut inserted_ids = Vec::<String>::new();
            let mut updated_ids = Vec::<String>::new();
            for entry in &entries {
                let existed = corpus_exists(&conn, &entry.id)?;
                upsert_corpus(&conn, entry)?;
                if existed {
                    updated_ids.push(entry.id.clone());
                } else {
                    inserted_ids.push(entry.id.clone());
                }
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "operation": "corpora_upsert_json",
                    "count": entries.len(),
                    "inserted": inserted_ids,
                    "updated": updated_ids,
                    "ids": entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>()
                }))?
            );
        }
        CorporaAction::ExportJson(args) => {
            let conn = open_db(&resolve_db_path(&args.db))?;
            let corpora = list_corpora(&conn, args.environment.as_deref(), args.include_noncurrent, None)?;
            let json_text = serde_json::to_string_pretty(&corpora)?;
            if let Some(path) = args.output {
                fs::write(&path, json_text)
                    .with_context(|| format!("Failed to write export file '{}'", path.display()))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "operation": "corpora_export_json",
                        "output": path
                    }))?
                );
            } else {
                println!("{json_text}");
            }
        }
        CorporaAction::Validate(args) => {
            let conn = open_db(&resolve_db_path(&args.db))?;
            let corpora = if let Some(id) = args.id.as_deref() {
                vec![get_corpus(&conn, id)?]
            } else {
                list_corpora(&conn, args.environment.as_deref(), args.include_noncurrent, None)?
            };

            let mut results = Vec::new();
            for corpus in corpora {
                let result = validate_corpus(&corpus, args.full, args.strict_full);
                update_validation_result(&conn, &corpus.id, &result)?;
                results.push(result);
            }

            let failures = results.iter().filter(|r| !r.ok).count();
            let summary = json!({
                "ok": failures == 0,
                "operation": "corpora_validate",
                "full": args.full,
                "strict_full": args.strict_full,
                "validated": results.len(),
                "failures": failures,
                "results": results
            });
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
    }
    Ok(())
}

fn handle_reindex(args: ReindexArgs) -> Result<()> {
    match args.action {
        ReindexAction::Enqueue(a) => {
            let conn = open_db(&resolve_db_path(&a.db))?;
            let role = normalize_role(Some(&a.request_role));
            if role != "admin" {
                anyhow::bail!("Reindex enqueue requires admin role (got '{}')", role);
            }
            let _ = get_corpus(&conn, &a.corpus)
                .with_context(|| format!("Corpus '{}' not found in FQS catalog", a.corpus))?;
            let backends = parse_backend_csv(a.backends.as_deref());
            let req = json!({
                "corpus": a.corpus,
                "reindex_backends": backends,
                "priority": a.priority,
                "origin": a.origin,
                "request_role": role,
                "note": a.note,
            });
            let created = enqueue_reindex_job(
                &conn,
                &a.corpus,
                &backends,
                a.priority,
                Some(role.as_str()),
                Some(a.origin.as_str()),
                a.note.as_deref(),
                &req,
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "operation": "reindex_enqueue",
                    "job": created
                }))?
            );
        }
        ReindexAction::Queue(a) => {
            let conn = open_db(&resolve_db_path(&a.db))?;
            let rows = list_reindex_jobs(
                &conn,
                a.status.as_deref(),
                a.corpus.as_deref(),
                clamp_limit(a.limit, 1, 1000),
            )?;
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        ReindexAction::History(a) => {
            let conn = open_db(&resolve_db_path(&a.db))?;
            let rows = list_reindex_history(&conn, a.corpus.as_deref(), clamp_limit(a.limit, 1, 5000))?;
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        ReindexAction::MarkStarted(a) => {
            let conn = open_db(&resolve_db_path(&a.db))?;
            let updated = mark_reindex_job_started(&conn, &a.job_id, a.worker_id.as_deref())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "operation": "reindex_mark_started",
                    "job": updated
                }))?
            );
        }
        ReindexAction::MarkFinished(a) => {
            let conn = open_db(&resolve_db_path(&a.db))?;
            let result_val = a
                .result_json
                .as_deref()
                .map(|s| serde_json::from_str::<Value>(s))
                .transpose()
                .context("Invalid --result-json payload (must be valid JSON)")?;
            let updated = mark_reindex_job_finished(
                &conn,
                &a.job_id,
                a.ok,
                a.message.as_deref(),
                a.error.as_deref(),
                result_val.as_ref(),
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "operation": "reindex_mark_finished",
                    "job": updated
                }))?
            );
        }
        ReindexAction::DispatchOnce(a) => {
            let db_path = resolve_db_path(&a.db);
            let assigned = dispatch_reindex_once_path(&db_path, a.default_worker_max_concurrent)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "operation": "reindex_dispatch_once",
                    "assigned": assigned
                }))?
            );
        }
        ReindexAction::WorkerHeartbeat(a) => {
            let conn = open_db(&resolve_db_path(&a.db))?;
            let caps = parse_backend_csv(a.capabilities.as_deref());
            let worker = upsert_reindex_worker_heartbeat(
                &conn,
                &a.worker_id,
                a.max_concurrent.max(1),
                a.host.as_deref(),
                &caps,
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "operation": "reindex_worker_heartbeat",
                    "worker": worker
                }))?
            );
        }
    }
    Ok(())
}

/// When `settings.available_backends` is a non-empty JSON array, only those engine names may run.
/// Missing key or wrong type: no restriction (legacy rows). Empty array: catalogue marks no engines.
fn backend_allowed_by_settings(corpus: &CorpusEntry, backend: &str) -> bool {
    match corpus.settings.get("available_backends") {
        None => true,
        Some(Value::Array(arr)) if arr.is_empty() => false,
        Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str().map(str::trim) == Some(backend)),
        _ => true,
    }
}

/// Catalogue `preferred_backend` hint: `auto` resolves from `settings.query_backend` or index paths.
/// One FQS row describes one corpus (version); TEITOK/flexicorp typically passes `backend` on each query.
fn resolve_effective_backend(corpus: &CorpusEntry) -> Result<String> {
    match corpus.preferred_backend.as_str() {
        "auto" => {
            if let Some(s) = corpus
                .settings
                .get("query_backend")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if backend_allowed_by_settings(corpus, s) {
                    return Ok(s.to_string());
                }
            }
            if resolve_pando_index_dir(corpus).is_ok() && backend_allowed_by_settings(corpus, "pando") {
                return Ok("pando".to_string());
            }
            let (_, reg) = resolve_cqp_registry(corpus);
            if reg.is_some() && backend_allowed_by_settings(corpus, "cqp") {
                return Ok("cqp".to_string());
            }
            anyhow::bail!(
                "preferred_backend is 'auto' but could not resolve (check Pando/CQP paths, settings.query_backend, and settings.available_backends)"
            );
        }
        other => {
            if !backend_allowed_by_settings(corpus, other) {
                anyhow::bail!(
                    "preferred_backend '{}' is not listed in settings.available_backends for this corpus",
                    other
                );
            }
            Ok(other.to_string())
        }
    }
}

fn handle_query(args: QueryArgs) -> Result<()> {
    let conn = open_db(&resolve_db_path(&args.db))?;
    let corpus = get_corpus(&conn, &args.corpus)?;
    let response = execute_query(
        &corpus,
        &args.corpus,
        &args.query_text,
        &args.language,
        args.start,
        args.size,
        args.backend.as_deref(),
        None,
    )?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

fn parse_http_url_target(url: &str) -> Option<(String, u16, String)> {
    let u = url.trim();
    if !u.starts_with("http://") {
        return None;
    }
    let rest = &u["http://".len()..];
    let (host_port, path) = if let Some(idx) = rest.find('/') {
        (&rest[..idx], &rest[idx..])
    } else {
        (rest, "/")
    };
    if host_port.trim().is_empty() {
        return None;
    }
    let (host, port) = if let Some((h, p)) = host_port.rsplit_once(':') {
        if let Ok(pp) = p.parse::<u16>() {
            (h.to_string(), pp)
        } else {
            (host_port.to_string(), 80)
        }
    } else {
        (host_port.to_string(), 80)
    };
    let p = if path.is_empty() { "/" } else { path };
    Some((host, port, p.to_string()))
}

fn probe_http_health_details(
    host: &str,
    port: u16,
    health_path: &str,
) -> Result<(bool, String, Option<Value>)> {
    let addr = format!("{host}:{port}");
    let sock = addr
        .to_socket_addrs()?
        .next()
        .with_context(|| format!("Could not resolve address '{addr}'"))?;
    let mut stream = TcpStream::connect_timeout(&sock, Duration::from_secs(2))
        .with_context(|| format!("Could not connect to {addr}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok();

    let path = if health_path.trim().is_empty() {
        "/health"
    } else {
        health_path
    };
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\n\r\n",
        path, host
    );
    stream.write_all(req.as_bytes())?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw)?;
    let mut lines = raw.lines();
    let first = lines.next().unwrap_or("").to_string();
    let ok = first.contains(" 200 ");
    let body = if let Some((_, b)) = raw.split_once("\r\n\r\n") {
        b
    } else if let Some((_, b)) = raw.split_once("\n\n") {
        b
    } else {
        ""
    };
    let health_json = serde_json::from_str::<Value>(body.trim()).ok();
    Ok((ok, first, health_json))
}

fn handle_status(args: StatusArgs) -> Result<()> {
    let db_path = resolve_db_path(&args.db);
    let _ = open_db(&db_path)?;

    let (host, port, base_path, effective_url) = if let Some(url) = args.url.as_deref() {
        if let Some((h, p, path)) = parse_http_url_target(url) {
            let effective = format!("http://{}:{}{}", h, p, path);
            (h, p, path, effective)
        } else {
            anyhow::bail!("--url must be an http:// URL (example: http://127.0.0.1:8787)")
        }
    } else if let Some(runtime_url) = read_fqs_http_runtime_url(&db_path) {
        if let Some((h, p, path)) = parse_http_url_target(&runtime_url) {
            let effective = format!("http://{}:{}{}", h, p, path);
            (h, p, path, effective)
        } else {
            anyhow::bail!(
                "Invalid \"url\" in {} (expected http://host:port/...)",
                fqs_http_runtime_path(&db_path).display()
            )
        }
    } else {
        (
            args.host.clone(),
            args.port,
            "/".to_string(),
            format!("http://{}:{}", args.host, args.port),
        )
    };
    let health_path = if base_path == "/" {
        "/health".to_string()
    } else if base_path.ends_with("/health") {
        base_path
    } else {
        format!("{}/health", base_path.trim_end_matches('/'))
    };
    let health = probe_http_health_details(&host, port, &health_path);
    let (http_ok, status_line, err, health_json) = match health {
        Ok((ok, line, details)) => (ok, line, String::new(), details),
        Err(e) => (false, String::new(), e.to_string(), None),
    };
    let server_version = health_json
        .as_ref()
        .and_then(|v| v.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let server_name = health_json
        .as_ref()
        .and_then(|v| v.get("server_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let server_db_path = health_json
        .as_ref()
        .and_then(|v| v.get("db_path"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // Top-level `ok` matches HTTP reachability (same notion TEITOK uses for FQS query routing).
    // The status command still exits 0 after printing JSON so scripts can parse output; use `ok` for health.
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": http_ok,
            "operation": "status",
            "db_path": db_path,
            "cli_version": env!("CARGO_PKG_VERSION"),
            "http": {
                "url": effective_url,
                "runtime_file": fqs_http_runtime_path(&db_path),
                "health_path": health_path,
                "ok": http_ok,
                "status_line": status_line,
                "error": err,
                "server_version": server_version,
                "server_name": server_name,
                "server_db_path": server_db_path
            }
        }))?
    );
    Ok(())
}

fn cleanup_runtime_tables(conn: &Connection, session_ttl_minutes: i64) -> Result<()> {
    let mins = if session_ttl_minutes < 1 {
        1
    } else {
        session_ttl_minutes
    };
    conn.execute(
        "DELETE FROM active_sessions WHERE last_seen_at < datetime('now', '-' || ?1 || ' minutes')",
        params![mins],
    )?;
    Ok(())
}

fn run_housekeeping_once(db_path: &PathBuf, session_ttl_minutes: i64) {
    if let Ok(conn) = open_db(db_path) {
        let _ = cleanup_runtime_tables(&conn, session_ttl_minutes);
    }
}

fn default_request_log_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.trim().is_empty() {
                return PathBuf::from(appdata)
                    .join("fqs")
                    .join("logs")
                    .join("fqs.log");
            }
        }
        return PathBuf::from(r"C:\ProgramData\fqs\logs\fqs.log");
    }
    #[cfg(target_os = "macos")]
    {
        return PathBuf::from("/usr/local/var/log/fqs/fqs.log");
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return PathBuf::from("/var/log/fqs/fqs.log");
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    PathBuf::from("fqs.log")
}

fn prepare_request_log_path(db_path: &PathBuf, configured: Option<PathBuf>) -> (PathBuf, Option<String>) {
    let preferred = configured.unwrap_or_else(default_request_log_path);
    let mut warning = None::<String>;
    let mut chosen = preferred.clone();
    let ensure = |p: &PathBuf| -> Result<()> {
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        Ok(())
    };
    if ensure(&chosen).is_err() {
        let fallback = db_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("fqs-http.log");
        if ensure(&fallback).is_ok() {
            warning = Some(format!(
                "Request log path '{}' not writable; using fallback '{}'",
                chosen.display(),
                fallback.display()
            ));
            chosen = fallback;
        } else {
            warning = Some(format!(
                "Request log path '{}' not writable; request log writes may fail",
                chosen.display()
            ));
        }
    }
    (chosen, warning)
}

fn rotate_request_log_if_needed(path: &PathBuf, max_bytes: u64, keep_files: usize) -> Result<()> {
    if max_bytes == 0 || keep_files == 0 {
        return Ok(());
    }
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    if meta.len() < max_bytes {
        return Ok(());
    }
    let oldest = PathBuf::from(format!("{}.{}", path.display(), keep_files));
    let _ = fs::remove_file(&oldest);
    for i in (1..keep_files).rev() {
        let src = PathBuf::from(format!("{}.{}", path.display(), i));
        let dst = PathBuf::from(format!("{}.{}", path.display(), i + 1));
        if src.exists() {
            let _ = fs::rename(src, dst);
        }
    }
    let first = PathBuf::from(format!("{}.1", path.display()));
    if path.exists() {
        let _ = fs::rename(path, first);
    }
    Ok(())
}

fn log_http_request_row(
    state: &HttpAppState,
    method: &str,
    path: &str,
    status: u16,
    elapsed_ms: u128,
    client_ip: &str,
    user_agent: &str,
) {
    let line = format!(
        "method={} path=\"{}\" status={} elapsed_ms={} client_ip={} ua=\"{}\"",
        method,
        path.replace('"', "\\\""),
        status,
        elapsed_ms,
        if client_ip.trim().is_empty() { "-" } else { client_ip },
        user_agent.replace('"', "\\\"")
    );
    append_request_log_line(state, &line);
}

fn append_request_log_line(state: &HttpAppState, line: &str) {
    let _ = rotate_request_log_if_needed(
        &state.request_log_path,
        state.request_log_max_bytes,
        state.request_log_keep_files,
    );
    let ts = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
    let full = format!("{} {}\n", ts, line);
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&state.request_log_path)
    {
        let _ = f.write_all(full.as_bytes());
    }
}

fn log_query_request_row(
    state: &HttpAppState,
    status: u16,
    elapsed_ms: u128,
    req: &HttpQueryRequest,
    role: &str,
    backend_effective: Option<&str>,
    would_block: bool,
    reasons: &[String],
    error: Option<&str>,
) {
    let reason_txt = if reasons.is_empty() {
        "-".to_string()
    } else {
        reasons.join(";").replace('"', "\\\"")
    };
    let line = format!(
        "method=POST path=\"/query\" status={} elapsed_ms={} client_ip=- ua=\"-\" corpus=\"{}\" role=\"{}\" backend_req=\"{}\" backend_effective=\"{}\" language=\"{}\" start={} size={} session_id=\"{}\" blocked={} reasons=\"{}\" query=\"{}\" error=\"{}\"",
        status,
        elapsed_ms,
        req.corpus.replace('"', "\\\""),
        role.replace('"', "\\\""),
        req.backend.as_deref().unwrap_or("auto").replace('"', "\\\""),
        backend_effective.unwrap_or("-").replace('"', "\\\""),
        req.language.as_deref().unwrap_or("auto").replace('"', "\\\""),
        req.start.unwrap_or(0),
        req.size.unwrap_or(25),
        req.session_id.as_deref().unwrap_or("-").replace('"', "\\\""),
        if would_block { "true" } else { "false" },
        reason_txt,
        req.query.replace('"', "\\\""),
        error.unwrap_or("-").replace('"', "\\\"")
    );
    append_request_log_line(state, &line);
}

fn touch_active_session(
    db_path: &PathBuf,
    session_id: &str,
    role: Option<&str>,
    corpus_id: Option<&str>,
    backend: Option<&str>,
) {
    let sid = session_id.trim();
    if sid.is_empty() {
        return;
    }
    if let Ok(conn) = open_db(db_path) {
        let _ = conn.execute(
            r#"
INSERT INTO active_sessions (session_id, role, corpus_id, backend, created_at, last_seen_at)
VALUES (?1, ?2, ?3, ?4, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
ON CONFLICT(session_id) DO UPDATE SET
  role=COALESCE(excluded.role, active_sessions.role),
  corpus_id=COALESCE(excluded.corpus_id, active_sessions.corpus_id),
  backend=COALESCE(excluded.backend, active_sessions.backend),
  last_seen_at=CURRENT_TIMESTAMP
"#,
            params![sid, role, corpus_id, backend],
        );
    }
}

async fn http_log_middleware(
    State(state): State<HttpAppState>,
    req: Request,
    next: Next,
) -> Response {
    let client_ip = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            req.headers()
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "-".to_string());
    let user_agent = req
        .headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let started = Instant::now();
    let response = next.run(req).await;
    let status = response.status().as_u16();
    let is_polling_jobs = method == "GET" && path == "/reindex/jobs" && (200..300).contains(&status);
    if path == "/health" || path == "/query" || is_polling_jobs {
        return response;
    }
    let elapsed = started.elapsed().as_millis();
    log_http_request_row(
        &state,
        &method,
        &path,
        status,
        elapsed,
        &client_ip,
        &user_agent,
    );
    response
}

/// Canonical query-language id for the given backend, or an error if the dialect does not match
/// the executor (e.g. `manatee-cql` with `pando`). Translation between dialects belongs to callers
/// or to flexicorp-pando / Manatee, not FQS.
fn query_language_effective_for_backend(backend: &str, requested_language: &str) -> Result<String> {
    let t = requested_language.trim();
    let lower = t.to_ascii_lowercase();
    match backend {
        "pando" => match lower.as_str() {
            "" | "auto" => Ok("pando-cql".to_string()),
            "pando-cql" => Ok("pando-cql".to_string()),
            _ => anyhow::bail!(
                "Query language '{}' is not supported for backend 'pando' (use 'pando-cql' or 'auto')",
                if t.is_empty() { "(empty)" } else { t }
            ),
        },
        "cqp" => match lower.as_str() {
            "" | "auto" => Ok("cwb-cql".to_string()),
            "cwb-cql" => Ok("cwb-cql".to_string()),
            _ => anyhow::bail!(
                "Query language '{}' is not supported for backend 'cqp' (use 'cwb-cql' or 'auto')",
                if t.is_empty() { "(empty)" } else { t }
            ),
        },
        _ => anyhow::bail!("Unknown backend for query language validation: {}", backend),
    }
}

fn extract_effective_query_operation(payload: &Value) -> Option<String> {
    let candidates = [
        "/operation",
        "/result/operation",
        "/done/operation",
        "/done/result/operation",
        "/result/result/operation",
        "/raw/operation",
    ];
    for ptr in candidates {
        if let Some(op) = payload.pointer(ptr).and_then(|v| v.as_str()) {
            let s = op.trim().to_lowercase();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

fn execute_query(
    corpus: &CorpusEntry,
    corpus_id: &str,
    query_text: &str,
    language: &str,
    start: u32,
    size: u32,
    backend_override: Option<&str>,
    query_options: Option<&HttpQueryRequest>,
) -> Result<Value> {
    let started = Instant::now();
    let requested_language = language.to_string();
    let backend = if let Some(b) = backend_override {
        b.trim().to_string()
    } else {
        resolve_effective_backend(corpus)?
    };
    if !backend_allowed_by_settings(corpus, &backend) {
        anyhow::bail!(
            "Backend '{}' is not allowed for this corpus (see settings.available_backends)",
            backend
        );
    }
    if backend != "pando" && backend != "cqp" {
        anyhow::bail!(
            "Resolved backend '{}' is not implemented in fqs query execution. Use backend=pando|cqp, or set preferred_backend / settings.query_backend.",
            backend
        );
    }
    let (effective_language, exec_kind, exec_binary, exec_target, payload, exit_code) =
        match backend.as_str() {
            "pando" => {
                let effective = query_language_effective_for_backend("pando", &requested_language)?;
                let pando_query = normalize_pando_query(query_text);
                let exec = run_pando_query(corpus, &pando_query, start, size)?;
                (
                    effective,
                    "flexicorp-pando-cli".to_string(),
                    exec.binary,
                    exec.index_dir,
                    exec.payload,
                    exec.exit_code,
                )
            }
            "cqp" => {
                let effective = query_language_effective_for_backend("cqp", &requested_language)?;
                let prefer_flexicorp = corpus.supports_xml
                    || corpus
                        .settings
                        .get("use_flexicorp_cqp")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                    || corpus.source_kind.contains("teitok");
                let exec = if prefer_flexicorp {
                    run_flexicorp_cqp_query(corpus, query_text, start, size, query_options)?
                } else {
                    run_cqp_query(corpus, query_text, start, size)?
                };
                (
                    effective,
                    exec.kind,
                    exec.binary,
                    exec.target,
                    exec.payload,
                    exec.exit_code,
                )
            }
            _ => unreachable!("backend was checked to be pando or cqp"),
        };
    let elapsed_ms = started.elapsed().as_millis();

    let operation_effective = extract_effective_query_operation(&payload)
        .unwrap_or_else(|| "query".to_string());
    let response = json!({
        "ok": true,
        "prototype": false,
        "operation": "query",
        "operation_effective": operation_effective,
        "query": {
            "corpus": corpus_id,
            "language_requested": requested_language,
            "language_effective": effective_language,
            "text": query_text,
            "start": start,
            "size": size,
            "window": query_options.and_then(|q| q.window),
            "context_scope": query_options.and_then(|q| q.context_scope.clone()),
            "context_format": query_options.and_then(|q| q.context_format.clone()),
            "flexicorp_fragment_kwic_cpos_span": query_options.and_then(|q| q.flexicorp_fragment_kwic_cpos_span)
        },
        "corpus": corpus,
        "backend_catalog": corpus.preferred_backend,
        "backend_resolved": backend,
        "backend_override": backend_override,
        "executor": {
            "kind": exec_kind,
            "binary": exec_binary,
            "target": exec_target
        },
        "meta": {
            "elapsed_ms": elapsed_ms,
            "exit_code": exit_code
        },
        "raw": payload
    });

    Ok(response)
}

async fn run_http_server(args: ServeArgs) -> Result<()> {
    if args.restart {
        restart_matching_serve_processes(&args.host, args.port);
    }
    let db_path = resolve_db_path(&args.db);
    let _ = open_db(&db_path)?;
    run_housekeeping_once(&db_path, args.session_ttl_minutes);
    let (request_log_path, request_log_warning) = prepare_request_log_path(&db_path, args.log_file.clone());

    let state = HttpAppState {
        db_path: db_path.clone(),
        test_mode: args.test,
        host: args.host.clone(),
        port: args.port,
        fcs_database: args.fcs_database.clone(),
        server_name: args.server_name.clone(),
        request_log_path: request_log_path.clone(),
        request_log_max_bytes: args.log_max_bytes,
        request_log_keep_files: args.log_keep_files,
    };
    let hk_db_path = db_path.clone();
    let hk_session_ttl = args.session_ttl_minutes;
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(60)).await;
            run_housekeeping_once(&hk_db_path, hk_session_ttl);
        }
    });
    let app = Router::new()
        .route("/", get(http_root_with_state))
        .route("/health", get(http_health))
        .route("/corpora", get(http_list_corpora))
        .route("/labels", get(http_browse_labels))
        .route("/fcs", get(http_fcs))
        .route("/reindex/jobs", get(http_reindex_jobs).post(http_reindex_enqueue))
        .route("/reindex/history", get(http_reindex_history))
        .route("/reindex/workers/heartbeat", post(http_reindex_worker_heartbeat))
        .route("/reindex/jobs/mark-started", post(http_reindex_mark_started))
        .route("/reindex/jobs/mark-finished", post(http_reindex_mark_finished))
        .route("/query", post(http_query))
        .layer(middleware::from_fn_with_state(state.clone(), http_log_middleware))
        .with_state(state);

    let worker_id = format!("fqs-serve-{}", std::process::id());
    let worker_caps = vec![
        "auto".to_string(),
        "manatee".to_string(),
        "pando".to_string(),
        "cqp".to_string(),
        "clickql".to_string(),
        "clickhouse".to_string(),
        "blacklab".to_string(),
        "pmltq".to_string(),
    ];
    let dispatch_db_path = db_path.clone();
    let execute_db_path = db_path.clone();
    let dispatch_worker_id = worker_id.clone();
    let dispatch_worker_caps = worker_caps.clone();
    let execute_worker_id = worker_id.clone();
    let active_jobs: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));
    tokio::spawn(async move {
        loop {
            if let Ok(conn) = open_db(&dispatch_db_path) {
                if let Err(err) = upsert_reindex_worker_heartbeat(
                    &conn,
                    &dispatch_worker_id,
                    1,
                    None,
                    &dispatch_worker_caps,
                ) {
                    eprintln!(
                        "[fqs][reindex] heartbeat failed for worker {}: {}",
                        dispatch_worker_id, err
                    );
                }
                let active_snapshot = if let Ok(set) = active_jobs.lock() {
                    set.clone()
                } else {
                    std::collections::HashSet::new()
                };
                match reconcile_orphaned_running_jobs(&conn, &dispatch_worker_id, &active_snapshot) {
                    Ok(done) if !done.is_empty() => {
                        eprintln!(
                            "[fqs][reindex] reconciled {} orphaned running jobs: {}",
                            done.len(),
                            done.join(", ")
                        );
                    }
                    Ok(_) => {}
                    Err(err) => {
                        eprintln!("[fqs][reindex] reconcile tick failed: {}", err);
                    }
                }
            } else {
                eprintln!(
                    "[fqs][reindex] could not open db for heartbeat/dispatch: {}",
                    dispatch_db_path.display()
                );
            }
            let assigned = match dispatch_reindex_once_path(&dispatch_db_path, 1) {
                Ok(rows) => rows,
                Err(err) => {
                    eprintln!("[fqs][reindex] dispatch tick failed: {}", err);
                    Vec::new()
                }
            };
            for job in assigned {
                let jid = job.job_id.clone();
                let mut should_start = false;
                if let Ok(mut set) = active_jobs.lock() {
                    if !set.contains(&jid) {
                        set.insert(jid.clone());
                        should_start = true;
                    }
                }
                if !should_start {
                    continue;
                }
                let dbp = execute_db_path.clone();
                let wid = execute_worker_id.clone();
                let active_jobs_done = Arc::clone(&active_jobs);
                tokio::task::spawn_blocking(move || {
                    let _ = execute_reindex_job_for_worker(&dbp, &jid, &wid);
                    if let Ok(mut set) = active_jobs_done.lock() {
                        set.remove(&jid);
                    }
                });
            }
            sleep(Duration::from_secs(3)).await;
        }
    });

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind HTTP server at {addr}"))?;
    let http_runtime_warning = match write_fqs_http_runtime_file(&db_path, &args.host, args.port) {
        Ok(()) => None::<String>,
        Err(e) => Some(format!(
            "could not write {}: {}",
            fqs_http_runtime_path(&db_path).display(),
            e
        )),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": true,
            "operation": "serve",
            "address": addr,
            "db_path": db_path,
            "http_runtime_file": fqs_http_runtime_path(&db_path),
            "fcs_database": args.fcs_database,
            "server_name": args.server_name,
            "restart": args.restart,
            "test_mode": args.test,
            "log_file": request_log_path,
            "log_max_bytes": args.log_max_bytes,
            "log_keep_files": args.log_keep_files,
            "session_ttl_minutes": args.session_ttl_minutes,
            "log_warning": request_log_warning,
            "http_runtime_warning": http_runtime_warning
        }))?
    );
    axum::serve(listener, app).await.context("HTTP server failed")?;
    Ok(())
}

fn parse_pgrep_pids(stdout: &str) -> Vec<i32> {
    stdout
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .collect()
}

fn parse_pid_lines(stdout: &str) -> Vec<i32> {
    stdout
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0)
        .collect()
}

fn pid_command_line(pid: i32) -> String {
    if pid <= 0 {
        return String::new();
    }
    match ProcessCommand::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg("command=")
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => String::new(),
    }
}

fn is_fqs_serve_command(cmdline: &str) -> bool {
    let lc = cmdline.to_lowercase();
    lc.contains("fqs") && lc.contains(" serve")
}

fn collect_listener_pids_for_port(port: u16) -> Vec<i32> {
    // Primary probe: lsof (works on macOS and most Linux images that include it).
    if let Ok(out) = ProcessCommand::new("lsof")
        .arg("-nP")
        .arg(format!("-iTCP:{port}"))
        .arg("-sTCP:LISTEN")
        .arg("-t")
        .output()
    {
        if out.status.success() {
            let pids = parse_pid_lines(&String::from_utf8_lossy(&out.stdout));
            if !pids.is_empty() {
                return pids;
            }
        }
    }
    // Fallback: netstat + lsof may be unavailable in minimal containers.
    // Keep this conservative and return empty on parse issues.
    Vec::new()
}

fn collect_matching_serve_pids(host: &str, port: u16) -> Vec<i32> {
    let patterns = vec![
        format!("fqs serve --host {} --port {}", host.trim(), port),
        format!("fqs serve --port {} --host {}", port, host.trim()),
        format!("fqs serve --port {}", port),
        "fqs serve".to_string(),
    ];
    let mine = std::process::id() as i32;
    let mut out = std::collections::HashSet::new();
    for pattern in patterns {
        let pgrep_out = match ProcessCommand::new("pgrep").arg("-f").arg(&pattern).output() {
            Ok(o) => o,
            Err(err) => {
                eprintln!(
                    "[fqs][serve] --restart: could not run pgrep for '{}': {}",
                    pattern, err
                );
                continue;
            }
        };
        if !pgrep_out.status.success() {
            continue;
        }
        for pid in parse_pgrep_pids(&String::from_utf8_lossy(&pgrep_out.stdout)) {
            if pid > 0 && pid != mine {
                out.insert(pid);
            }
        }
    }
    out.into_iter().collect()
}

fn restart_matching_serve_processes(host: &str, port: u16) {
    let mut pids_set: std::collections::HashSet<i32> =
        collect_matching_serve_pids(host, port).into_iter().collect();
    // Also include explicit listener pid(s) on target port when command is fqs serve.
    for pid in collect_listener_pids_for_port(port) {
        let cmd = pid_command_line(pid);
        if is_fqs_serve_command(&cmd) {
            pids_set.insert(pid);
        }
    }
    let pids: Vec<i32> = pids_set.into_iter().collect();
    if pids.is_empty() {
        return;
    }
    for pid in &pids {
        let _ = ProcessCommand::new("kill").arg(pid.to_string()).status();
    }
    std::thread::sleep(Duration::from_millis(500));
    let survivors = collect_matching_serve_pids(host, port);
    for pid in survivors {
        let _ = ProcessCommand::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status();
    }
}

fn truncate_for_job_log(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect::<String>() + "…"
}

fn is_pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    let pid_s = pid.to_string();
    // Unix-friendly fast probe.
    if let Ok(out) = ProcessCommand::new("kill").arg("-0").arg(&pid_s).output() {
        return out.status.success();
    }
    // Fallback probe.
    if let Ok(out) = ProcessCommand::new("ps").arg("-p").arg(&pid_s).output() {
        return out.status.success();
    }
    false
}

fn reindex_job_process_pid(job: &ReindexJobEntry) -> Option<i64> {
    if let Some(pid) = job
        .result
        .get("process")
        .and_then(|p| p.get("pid"))
        .and_then(|v| v.as_i64())
    {
        if pid > 0 {
            return Some(pid);
        }
    }
    job.result.get("child_pid").and_then(|v| v.as_i64()).filter(|pid| *pid > 0)
}

fn is_reindex_worker_recent(conn: &Connection, worker_id: &str) -> Result<bool> {
    let worker_id = worker_id.trim();
    if worker_id.is_empty() {
        return Ok(false);
    }
    let val: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM reindex_workers WHERE worker_id=?1 AND status='online' AND last_heartbeat_at >= datetime('now', '-120 seconds') LIMIT 1",
            params![worker_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| sqlite_write_err("read reindex_workers recent heartbeat", e))?;
    Ok(val.is_some())
}

fn is_reindex_job_stale(conn: &Connection, job_id: &str, seconds: i64) -> Result<bool> {
    let sec = seconds.max(1);
    let threshold = format!("-{} seconds", sec);
    let stale: Option<i64> = conn
        .query_row(
            "SELECT CASE WHEN COALESCE(updated_at, started_at, requested_at) <= datetime('now', ?2) THEN 1 ELSE 0 END FROM reindex_jobs WHERE job_id=?1",
            params![job_id, threshold],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| sqlite_write_err("read reindex_jobs staleness", e))?;
    Ok(stale.unwrap_or(0) == 1)
}

fn set_reindex_job_process_started(
    conn: &Connection,
    job_id: &str,
    worker_id: &str,
    child_pid: i64,
    command_preview: &str,
) -> Result<()> {
    let existing = get_reindex_job(conn, job_id)?;
    if existing.status != "running" {
        return Ok(());
    }
    let mut result = existing.result;
    result["process"] = json!({
        "pid": child_pid,
        "alive": true,
        "worker_id": worker_id,
        "started_at": OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|_| "".to_string()),
        "command": truncate_for_job_log(command_preview, 1200),
    });
    conn.execute(
        "UPDATE reindex_jobs SET result_json=?2, updated_at=CURRENT_TIMESTAMP WHERE job_id=?1 AND status='running'",
        params![
            job_id,
            serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string()),
        ],
    )
    .map_err(|e| sqlite_write_err("update reindex_jobs process metadata", e))?;
    Ok(())
}

fn reconcile_orphaned_running_jobs(
    conn: &Connection,
    worker_id: &str,
    active_job_ids: &std::collections::HashSet<String>,
) -> Result<Vec<String>> {
    let running = list_reindex_jobs(conn, Some("running"), None, 5000)?;
    let mut reconciled: Vec<String> = Vec::new();
    for job in running {
        if active_job_ids.contains(&job.job_id) {
            continue;
        }
        let pid = reindex_job_process_pid(&job);
        let pid_alive = pid.map(is_pid_alive).unwrap_or(false);
        let owner = job.worker_id.clone().unwrap_or_default();
        let owner_recent = is_reindex_worker_recent(conn, &owner)?;
        // Grace window avoids racing right after "started".
        let stale = is_reindex_job_stale(conn, &job.job_id, 20)?;
        if !stale {
            continue;
        }
        let orphaned = if pid.is_some() {
            !pid_alive
        } else {
            !owner_recent
        };
        if !orphaned {
            continue;
        }
        let mut result = job.result.clone();
        result["reconciled"] = json!({
            "by_worker": worker_id,
            "at": OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|_| "".to_string()),
            "reason": "running_job_process_missing",
            "owner_worker_id": owner,
            "owner_recent": owner_recent,
            "pid": pid,
            "pid_alive": pid_alive,
        });
        let err = format!(
            "running reindex job lost worker process (worker_id={}, pid={})",
            job.worker_id.as_deref().unwrap_or(""),
            pid.map(|v| v.to_string()).unwrap_or_else(|| "none".to_string())
        );
        let _ = mark_reindex_job_finished(
            conn,
            &job.job_id,
            false,
            Some("failed"),
            Some(&err),
            Some(&result),
        )?;
        reconciled.push(job.job_id.clone());
    }
    Ok(reconciled)
}

fn extract_requested_backends(job: &ReindexJobEntry) -> Vec<String> {
    if !job.requested_backends.is_empty() {
        return job
            .requested_backends
            .iter()
            .map(|x| x.trim().to_lowercase())
            .filter(|x| !x.is_empty())
            .collect();
    }
    if let Some(arr) = job.request.get("reindex_backends").and_then(|v| v.as_array()) {
        let out: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|x| x.trim().to_lowercase())
            .filter(|x| !x.is_empty())
            .collect();
        if !out.is_empty() {
            return out;
        }
    }
    vec!["auto".to_string()]
}

fn pick_reindex_cli_backend(corpus: &CorpusEntry, requested_backends: &[String]) -> String {
    for b in requested_backends {
        let bb = b.trim().to_lowercase();
        if bb.is_empty() || bb == "auto" {
            continue;
        }
        return if bb == "clickhouse" {
            "clickql".to_string()
        } else {
            bb
        };
    }
    if let Ok(eff) = resolve_effective_backend(corpus) {
        return eff;
    }
    "flexi".to_string()
}

fn backend_reindex_dialect(backend: &str) -> (Option<&'static str>, Option<&'static str>) {
    match backend {
        "manatee" => (Some("manatee-cql"), Some("manatee")),
        "cqp" => (Some("cwb-cql"), Some("cwb")),
        "pando" => (Some("pando-cql"), Some("pando")),
        "clickql" | "clickhouse" => (Some("clickcql"), Some("clickhouse")),
        "blacklab" => (Some("bcql"), Some("blacklab")),
        _ => (None, None),
    }
}

fn extract_reindex_options_map(job: &ReindexJobEntry, key: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(obj) = job.request.get(key).and_then(|v| v.as_object()) else {
        return out;
    };
    for (k, v) in obj {
        let kk = k.trim();
        if kk.is_empty() {
            continue;
        }
        let vv = match v {
            Value::String(s) => s.trim().to_string(),
            Value::Bool(b) => {
                if *b {
                    "yes".to_string()
                } else {
                    "no".to_string()
                }
            }
            Value::Number(n) => n.to_string(),
            _ => continue,
        };
        if vv.is_empty() {
            continue;
        }
        out.insert(kk.to_string(), vv);
    }
    out
}

fn extract_backend_reindex_options(job: &ReindexJobEntry, backend: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(root) = job.request.get("backend_options").and_then(|v| v.as_object()) else {
        return out;
    };
    // Apply wildcard defaults first, backend-specific values override them.
    for scope in ["*", backend] {
        let Some(obj) = root.get(scope).and_then(|v| v.as_object()) else {
            continue;
        };
        for (k, v) in obj {
            let kk = k.trim();
            if kk.is_empty() {
                continue;
            }
            let vv = match v {
                Value::String(s) => s.trim().to_string(),
                Value::Bool(b) => {
                    if *b {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    }
                }
                Value::Number(n) => n.to_string(),
                _ => continue,
            };
            if vv.is_empty() {
                continue;
            }
            out.insert(kk.to_string(), vv);
        }
    }
    out
}

#[derive(Debug, Clone)]
struct RuntimeProgress {
    percent: Option<i64>,
    phase: Option<String>,
    message: String,
    stream: String,
}

fn try_parse_percent(text: &str) -> Option<i64> {
    for tok in text.split_whitespace() {
        if !tok.contains('%') {
            continue;
        }
        let n = tok.trim_matches(|c: char| {
            c == '%'
                || c == '('
                || c == ')'
                || c == ','
                || c == '.'
                || c == ';'
                || c == ':'
                || c == '['
                || c == ']'
        });
        if n.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(v) = n.parse::<i64>() {
                return Some(v.clamp(0, 100));
            }
        }
    }
    None
}

fn try_parse_phase(text: &str) -> Option<String> {
    if let Some(s) = text.find('(') {
        if let Some(e_rel) = text[s + 1..].find(')') {
            let phase = text[s + 1..s + 1 + e_rel].trim();
            if !phase.is_empty() {
                return Some(phase.to_string());
            }
        }
    }
    let lower = text.to_ascii_lowercase();
    for p in [
        "queued",
        "running",
        "encoding",
        "finalizing",
        "mkstats",
        "staging",
        "copying",
        "completed",
        "failed",
    ] {
        if lower.contains(p) {
            return Some(p.to_string());
        }
    }
    None
}

fn parse_runtime_progress_line(line: &str, stream: &str) -> Option<RuntimeProgress> {
    let msg = line.trim();
    if msg.is_empty() {
        return None;
    }
    let percent = try_parse_percent(msg);
    let phase = try_parse_phase(msg);
    let looks_progressy = percent.is_some()
        || phase.is_some()
        || msg.contains("mkstats")
        || msg.contains("Compiling")
        || msg.contains("compile");
    if !looks_progressy {
        return None;
    }
    Some(RuntimeProgress {
        percent,
        phase,
        message: truncate_for_job_log(msg, 600),
        stream: stream.to_string(),
    })
}

fn update_reindex_job_progress(
    conn: &Connection,
    job_id: &str,
    progress: &RuntimeProgress,
) -> Result<()> {
    let existing = get_reindex_job(conn, job_id)?;
    if existing.status != "running" {
        return Ok(());
    }
    let mut result = existing.result;
    let mut prog = json!({
        "message": progress.message,
        "stream": progress.stream,
        "updated_at": OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|_| "".to_string()),
    });
    if let Some(v) = progress.percent {
        prog["percent"] = json!(v);
    }
    if let Some(ph) = progress.phase.as_deref() {
        prog["phase"] = json!(ph);
    }
    result["progress"] = prog;
    result["last_log_line"] = json!(progress.message.clone());
    conn.execute(
        "UPDATE reindex_jobs SET message=?2, result_json=?3, updated_at=CURRENT_TIMESTAMP WHERE job_id=?1 AND status='running'",
        params![
            job_id,
            progress.message,
            serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string())
        ],
    )
    .map_err(|e| sqlite_write_err("update reindex_jobs progress", e))?;
    Ok(())
}

fn execute_reindex_job_for_worker(db_path: &Path, job_id: &str, worker_id: &str) -> Result<()> {
    let conn = open_db(&db_path.to_path_buf())?;
    let job = get_reindex_job(&conn, job_id)?;
    if job.status != "running" {
        return Ok(());
    }
    if let Some(w) = job.worker_id.as_deref() {
        if !w.trim().is_empty() && w != worker_id {
            return Ok(());
        }
    }
    let corpus = get_corpus(&conn, &job.corpus_id)?;
    let project_root = resolve_teitok_project_root(&corpus);
    let requested_backends = extract_requested_backends(&job);
    let requested_csv = requested_backends.join(",");
    let backend = pick_reindex_cli_backend(&corpus, &requested_backends);
    let (query_language, corpus_format) = backend_reindex_dialect(&backend);
    let mut reindex_options = extract_reindex_options_map(&job, "options");
    for (k, v) in extract_backend_reindex_options(&job, &backend) {
        reindex_options.insert(k, v);
    }

    let python_bin = corpus
        .settings
        .get("python_bin")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("PYTHON_BIN").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| "python3".to_string());
    let flexicorp_module = corpus
        .settings
        .get("flexicorp_module")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("flexicorp")
        .to_string();

    let mut cmd = ProcessCommand::new(&python_bin);
    cmd.arg("-m")
        .arg(&flexicorp_module)
        .arg("reindex")
        .arg("--api")
        .arg("--backend")
        .arg(&backend)
        .arg("--folder")
        .arg(&project_root)
        .arg("--teitok")
        .arg("yes")
        .arg("--verbose")
        .arg("--staging")
        .arg("--reindex-backends")
        .arg(&requested_csv);
    if let Some(ql) = query_language {
        cmd.arg("--query-language").arg(ql);
    }
    if let Some(cf) = corpus_format {
        cmd.arg("--corpus-format").arg(cf);
    }
    let mut options_pairs: Vec<(String, String)> = reindex_options.into_iter().collect();
    options_pairs.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in &options_pairs {
        cmd.arg("--options").arg(format!("{k}={v}"));
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let options_preview = if options_pairs.is_empty() {
        String::new()
    } else {
        options_pairs
            .iter()
            .map(|(k, v)| format!(" --options {k}={v}"))
            .collect::<String>()
    };
    let command_preview = format!(
        "{python_bin} -m {flexicorp_module} reindex --api --backend {backend} --folder {project_root} --teitok yes --verbose --staging --reindex-backends {requested_csv}{options_preview}"
    );
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to execute reindex job '{}' via flexicorp CLI", job_id))?;
    let child_pid = child.id() as i64;
    let _ = set_reindex_job_process_started(&conn, job_id, worker_id, child_pid, &command_preview);

    let child_stdout = child.stdout.take().context("missing child stdout pipe")?;
    let child_stderr = child.stderr.take().context("missing child stderr pipe")?;
    let (tx, rx) = mpsc::channel::<(String, String)>();
    let tx_out = tx.clone();
    thread::spawn(move || {
        let reader = BufReader::new(child_stdout);
        for line in reader.lines().map_while(Result::ok) {
            let _ = tx_out.send(("stdout".to_string(), line));
        }
    });
    let tx_err = tx.clone();
    thread::spawn(move || {
        let reader = BufReader::new(child_stderr);
        for line in reader.lines().map_while(Result::ok) {
            let _ = tx_err.send(("stderr".to_string(), line));
        }
    });
    drop(tx);

    let progress_conn = open_db(&db_path.to_path_buf())?;
    let mut stdout_lines: Vec<String> = Vec::new();
    let mut stderr_lines: Vec<String> = Vec::new();
    let mut last_progress_percent: Option<i64> = None;
    let mut last_progress_phase: String = String::new();
    let mut last_progress_write = Instant::now()
        .checked_sub(Duration::from_secs(2))
        .unwrap_or_else(Instant::now);
    let mut channel_closed = false;
    let mut child_status: Option<std::process::ExitStatus> = None;
    while !channel_closed {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok((stream, line)) => {
                if stream == "stderr" {
                    stderr_lines.push(line.clone());
                } else {
                    stdout_lines.push(line.clone());
                }
                if let Some(progress) = parse_runtime_progress_line(&line, &stream) {
                    let percent_changed = progress.percent != last_progress_percent;
                    let phase_now = progress.phase.clone().unwrap_or_default();
                    let phase_changed = phase_now != last_progress_phase;
                    let timed = last_progress_write.elapsed() >= Duration::from_millis(800);
                    if percent_changed || phase_changed || timed {
                        let _ = update_reindex_job_progress(&progress_conn, job_id, &progress);
                        last_progress_percent = progress.percent;
                        last_progress_phase = phase_now;
                        last_progress_write = Instant::now();
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                channel_closed = true;
            }
        }
        if channel_closed {
            break;
        }
        if let Some(status) = child.try_wait()? {
            child_status = Some(status);
            // child exited; drain remaining queued lines quickly before leaving loop
            while let Ok((stream, line)) = rx.try_recv() {
                if stream == "stderr" {
                    stderr_lines.push(line.clone());
                } else {
                    stdout_lines.push(line.clone());
                }
                if let Some(progress) = parse_runtime_progress_line(&line, &stream) {
                    let _ = update_reindex_job_progress(&progress_conn, job_id, &progress);
                }
            }
            channel_closed = true;
        }
    }
    let status = match child_status {
        Some(s) => s,
        None => child.wait()?,
    };
    let exit_code = status.code().unwrap_or(-1);
    let stdout = stdout_lines.join("\n");
    let stderr = stderr_lines.join("\n");
    let stdout_t = truncate_for_job_log(&stdout, 16000);
    let stderr_t = truncate_for_job_log(&stderr, 16000);
    let mut result = json!({
        "executor": "fqs-serve-worker",
        "worker_id": worker_id,
        "command": command_preview,
        "process": {
            "pid": child_pid,
            "alive": false,
            "exited_at": OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|_| "".to_string()),
        },
        "exit_code": exit_code,
        "backend": backend,
        "reindex_backends": requested_backends,
        "options": options_pairs.iter().map(|(k,v)| format!("{k}={v}")).collect::<Vec<_>>(),
        "project_root": project_root,
        "stdout": stdout_t,
        "stderr": stderr_t,
        "progress": {
            "phase": if status.success() { "completed" } else { "failed" },
            "percent": if status.success() { 100 } else { last_progress_percent.unwrap_or(0) },
            "message": if status.success() { "completed" } else { "failed" }
        }
    });
    if let Ok(parsed) = serde_json::from_str::<Value>(&stdout) {
        result["raw"] = parsed;
    }

    let conn2 = open_db(&db_path.to_path_buf())?;
    if status.success() {
        let _ = mark_reindex_job_finished(
            &conn2,
            job_id,
            true,
            Some("completed"),
            None,
            Some(&result),
        )?;
    } else {
        let err_text = if !stderr.trim().is_empty() {
            truncate_for_job_log(stderr.trim(), 12000)
        } else if !stdout.trim().is_empty() {
            truncate_for_job_log(stdout.trim(), 12000)
        } else {
            format!("reindex process exited with status {}", exit_code)
        };
        let _ = mark_reindex_job_finished(
            &conn2,
            job_id,
            false,
            Some("failed"),
            Some(&err_text),
            Some(&result),
        )?;
    }
    Ok(())
}

async fn http_health(State(state): State<HttpAppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "service": "fqs",
        "mode": "http",
        "version": env!("CARGO_PKG_VERSION"),
        "server_name": state.server_name,
        "db_path": state.db_path.to_string_lossy(),
    }))
}

async fn http_root_with_state(State(state): State<HttpAppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "service": "fqs",
        "version": env!("CARGO_PKG_VERSION"),
        "server_name": state.server_name,
        "routes": [
            {"method":"GET", "path":"/", "description":"Route index"},
            {"method":"GET", "path":"/health", "description":"Health check"},
            {"method":"GET", "path":"/corpora", "description":"List corpora (query params: request_role, environment, include_noncurrent, tag)"},
            {"method":"GET", "path":"/labels", "description":"Distinct browse labels for catalog filtering"},
            {"method":"GET", "path":"/fcs", "description":"FCS/SRU-style endpoint"},
            {"method":"GET", "path":"/reindex/jobs", "description":"List reindex queue (status/corpus/limit)"},
            {"method":"POST", "path":"/reindex/jobs", "description":"Enqueue reindex job (admin role)"},
            {"method":"GET", "path":"/reindex/history", "description":"Reindex history log (corpus/limit)"},
            {"method":"POST", "path":"/reindex/workers/heartbeat", "description":"Worker heartbeat + capacity"},
            {"method":"POST", "path":"/reindex/jobs/mark-started", "description":"Worker callback: mark started"},
            {"method":"POST", "path":"/reindex/jobs/mark-finished", "description":"Worker callback: mark finished"},
            {"method":"POST", "path":"/query", "description":"Run query (JSON body: corpus, query, language?, start?, size?, request_role?)"}
        ]
    }))
}

async fn http_list_corpora(
    State(state): State<HttpAppState>,
    AxumQuery(params): AxumQuery<HttpCorporaQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let conn = open_db(&state.db_path).map_err(to_http_err)?;
    let role = normalize_role(params.request_role.as_deref());
    let include_noncurrent = params.include_noncurrent.unwrap_or(false);
    let corpora = list_corpora(
        &conn,
        params.environment.as_deref(),
        include_noncurrent,
        params.tag.as_deref(),
    )
    .map_err(to_http_err)?;
    let filtered = corpora
        .into_iter()
        .filter(|c| is_http_access_allowed(c, &role) && is_http_operation_allowed(c, "catalog"))
        .collect::<Vec<_>>();
    Ok(Json(json!({"ok": true, "role": role, "corpora": filtered})))
}

async fn http_browse_labels(
    State(state): State<HttpAppState>,
    AxumQuery(params): AxumQuery<HttpCorporaQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let conn = open_db(&state.db_path).map_err(to_http_err)?;
    let role = normalize_role(params.request_role.as_deref());
    let include_noncurrent = params.include_noncurrent.unwrap_or(false);
    let corpora = list_corpora(
        &conn,
        params.environment.as_deref(),
        include_noncurrent,
        None,
    )
    .map_err(to_http_err)?;
    let filtered = corpora
        .into_iter()
        .filter(|c| is_http_access_allowed(c, &role) && is_http_operation_allowed(c, "catalog"))
        .collect::<Vec<_>>();
    let mut labels: Vec<String> = filtered
        .iter()
        .flat_map(|c| c.labels.iter().cloned())
        .collect();
    labels.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));
    labels.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    Ok(Json(json!({"ok": true, "role": role, "labels": labels})))
}

async fn http_reindex_jobs(
    State(state): State<HttpAppState>,
    AxumQuery(params): AxumQuery<HttpReindexJobsQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let conn = open_db(&state.db_path).map_err(to_http_err)?;
    let limit = clamp_limit(params.limit.unwrap_or(100), 1, 1000);
    let requested_status = params
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let rows = if requested_status.is_none()
        || requested_status.is_some_and(|s| s.eq_ignore_ascii_case("active"))
    {
        // Default HTTP view is active queue only (running + queued),
        // so completed/failed items do not clutter "active" dashboards.
        let mut out =
            list_reindex_jobs(&conn, Some("running"), params.corpus.as_deref(), limit)
                .map_err(to_http_err)?;
        if out.len() < limit {
            let remaining = limit - out.len();
            let mut queued = list_reindex_jobs(
                &conn,
                Some("queued"),
                params.corpus.as_deref(),
                remaining,
            )
            .map_err(to_http_err)?;
            out.append(&mut queued);
        }
        out
    } else if requested_status.is_some_and(|s| s.eq_ignore_ascii_case("all")) {
        list_reindex_jobs(&conn, None, params.corpus.as_deref(), limit).map_err(to_http_err)?
    } else {
        list_reindex_jobs(&conn, requested_status, params.corpus.as_deref(), limit)
            .map_err(to_http_err)?
    };
    let effective_status = match requested_status {
        None => "active",
        Some("all") | Some("ALL") => "all",
        Some(s) => s,
    };
    Ok(Json(json!({"ok": true, "status_filter": effective_status, "jobs": rows})))
}

async fn http_reindex_history(
    State(state): State<HttpAppState>,
    AxumQuery(params): AxumQuery<HttpReindexJobsQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let conn = open_db(&state.db_path).map_err(to_http_err)?;
    let rows = list_reindex_history(
        &conn,
        params.corpus.as_deref(),
        clamp_limit(params.limit.unwrap_or(200), 1, 5000),
    )
    .map_err(to_http_err)?;
    Ok(Json(json!({"ok": true, "history": rows})))
}

async fn http_reindex_enqueue(
    State(state): State<HttpAppState>,
    Json(req): Json<HttpReindexEnqueueRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let role = normalize_role(req.request_role.as_deref());
    if role != "admin" {
        return Err((StatusCode::FORBIDDEN, "Reindex enqueue requires admin role".to_string()));
    }
    let conn = open_db(&state.db_path).map_err(to_http_err)?;
    let _ = get_corpus(&conn, &req.corpus).map_err(to_http_err)?;
    let mut backends = req.backends.unwrap_or_default();
    backends.retain(|x| !x.trim().is_empty());
    if backends.is_empty() {
        backends.push("auto".to_string());
    }
    let payload = json!({
        "corpus": req.corpus,
        "reindex_backends": backends,
        "priority": req.priority.unwrap_or(0),
        "request_role": role,
        "origin": req.origin.clone().unwrap_or_else(|| "http".to_string()),
        "note": req.note,
        "options": req.options,
        "backend_options": req.backend_options,
    });
    let created = enqueue_reindex_job(
        &conn,
        &req.corpus,
        &backends,
        req.priority.unwrap_or(0),
        Some(role.as_str()),
        req.origin.as_deref().or(Some("http")),
        req.note.as_deref(),
        &payload,
    )
    .map_err(to_http_err)?;
    Ok(Json(json!({"ok": true, "job": created})))
}

async fn http_reindex_worker_heartbeat(
    State(state): State<HttpAppState>,
    Json(req): Json<HttpReindexWorkerHeartbeatRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if req.worker_id.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "worker_id is required".to_string()));
    }
    let conn = open_db(&state.db_path).map_err(to_http_err)?;
    let caps = req.capabilities.unwrap_or_default();
    let max_c = req.max_concurrent.unwrap_or(1).max(1);
    let worker = upsert_reindex_worker_heartbeat(&conn, &req.worker_id, max_c, req.host.as_deref(), &caps)
        .map_err(to_http_err)?;
    Ok(Json(json!({"ok": true, "worker": worker})))
}

async fn http_reindex_mark_started(
    State(state): State<HttpAppState>,
    Json(req): Json<HttpReindexMarkStartedRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if req.job_id.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "job_id is required".to_string()));
    }
    let conn = open_db(&state.db_path).map_err(to_http_err)?;
    let updated = mark_reindex_job_started(&conn, &req.job_id, req.worker_id.as_deref()).map_err(to_http_err)?;
    Ok(Json(json!({"ok": true, "job": updated})))
}

async fn http_reindex_mark_finished(
    State(state): State<HttpAppState>,
    Json(req): Json<HttpReindexMarkFinishedRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if req.job_id.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "job_id is required".to_string()));
    }
    let conn = open_db(&state.db_path).map_err(to_http_err)?;
    let updated = mark_reindex_job_finished(
        &conn,
        &req.job_id,
        req.ok.unwrap_or(false),
        req.message.as_deref(),
        req.error.as_deref(),
        req.result.as_ref(),
    )
    .map_err(to_http_err)?;
    Ok(Json(json!({"ok": true, "job": updated})))
}

async fn http_query(
    State(state): State<HttpAppState>,
    Json(req): Json<HttpQueryRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let started = Instant::now();
    let conn = open_db(&state.db_path).map_err(to_http_err)?;
    let corpus = get_corpus(&conn, &req.corpus).map_err(to_http_err)?;
    let role = normalize_role(req.request_role.as_deref());
    if let Some(sid) = req.session_id.as_deref() {
        touch_active_session(
            &state.db_path,
            sid,
            Some(&role),
            Some(&req.corpus),
            req.backend.as_deref(),
        );
    }
    let mut policy_reasons: Vec<String> = Vec::new();
    if !is_http_access_allowed(&corpus, &role) {
        policy_reasons.push(format!("http access disabled by mode '{}'", corpus.http_policy_mode));
    }
    if !is_http_operation_allowed(&corpus, "query") {
        policy_reasons.push("query operation not allowed for this corpus".to_string());
    }
    if role == "visitor" && looks_like_aggregation(&req.query) {
        policy_reasons.push("aggregation-like query blocked for visitor role".to_string());
    }
    let would_block = !policy_reasons.is_empty();
    if would_block && !state.test_mode {
        let msg = format!("Policy blocked query: {}", policy_reasons.join("; "));
        log_query_request_row(
            &state,
            StatusCode::FORBIDDEN.as_u16(),
            started.elapsed().as_millis(),
            &req,
            &role,
            req.backend.as_deref(),
            true,
            &policy_reasons,
            Some(&msg),
        );
        return Err((StatusCode::FORBIDDEN, msg));
    }

    let language = req
        .language
        .as_deref()
        .unwrap_or("auto")
        .to_string();
    let start = req.start.unwrap_or(0);
    let size = req.size.unwrap_or(25);
    let mut response = match execute_query(
        &corpus,
        &req.corpus,
        &req.query,
        &language,
        start,
        size,
        req.backend.as_deref(),
        Some(&req),
    ) {
        Ok(r) => r,
        Err(e) => {
            let err_txt = e.to_string();
            let http = to_http_err(e);
            log_query_request_row(
                &state,
                http.0.as_u16(),
                started.elapsed().as_millis(),
                &req,
                &role,
                req.backend.as_deref(),
                would_block,
                &policy_reasons,
                Some(&err_txt),
            );
            return Err(http);
        }
    };
    if let Some(obj) = response.as_object_mut() {
        obj.insert(
            "policy".to_string(),
            json!({
                "test_mode": state.test_mode,
                "role": role,
                "would_block": would_block,
                "reasons": policy_reasons
            }),
        );
    }
    let backend_effective = response
        .get("backend_resolved")
        .and_then(|v| v.as_str())
        .or(req.backend.as_deref());
    log_query_request_row(
        &state,
        StatusCode::OK.as_u16(),
        started.elapsed().as_millis(),
        &req,
        &role,
        backend_effective,
        would_block,
        &policy_reasons,
        None,
    );
    Ok(Json(response))
}

async fn http_fcs(
    State(state): State<HttpAppState>,
    AxumQuery(params): AxumQuery<FcsQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let conn = open_db(&state.db_path).map_err(to_http_err)?;
    let role = normalize_role(params.request_role.as_deref());
    let operation = resolve_fcs_operation(&params);

    let xml = match operation.as_str() {
        "explain" => {
            let corpora = list_corpora(&conn, None, false, None).map_err(to_http_err)?;
            let visible = corpora
                .into_iter()
                .filter(|c| is_http_access_allowed(c, &role) || state.test_mode)
                .filter(|c| is_fcs_enabled(c))
                .collect::<Vec<_>>();
            build_fcs_explain_xml(
                &visible,
                &state.host,
                state.port,
                &state.fcs_database,
                params.x_fcs_endpoint_description.unwrap_or(false),
            )
        }
        "searchretrieve" => {
            let query = params
                .query
                .clone()
                .ok_or_else(|| (StatusCode::BAD_REQUEST, "FCS searchRetrieve requires query".to_string()))?;
            let start = params.start_record.unwrap_or(1).saturating_sub(1);
            let max = params.maximum_records.unwrap_or(10);
            let context = params
                .x_fcs_context
                .clone()
                .or_else(|| params.x_corpus.clone());

            if let Some(corpus_id) = context {
                let corpus = get_corpus(&conn, &corpus_id).map_err(to_http_err)?;
                if !is_fcs_enabled(&corpus) {
                    return Err((StatusCode::FORBIDDEN, format!("Corpus '{}' is not FCS-enabled", corpus_id)));
                }
                let mut policy_reasons = Vec::<String>::new();
                if !is_http_access_allowed(&corpus, &role) {
                    policy_reasons.push("http access blocked".to_string());
                }
                if !is_http_operation_allowed(&corpus, "query") {
                    policy_reasons.push("query op not allowed".to_string());
                }
                if !policy_reasons.is_empty() && !state.test_mode {
                    return Err((
                        StatusCode::FORBIDDEN,
                        format!("Policy blocked FCS searchRetrieve: {}", policy_reasons.join("; ")),
                    ));
                }

                let response = execute_query(&corpus, &corpus_id, &query, "auto", start, max, None, None)
                    .map_err(to_http_err)?;
                build_fcs_search_xml(&corpus, &query, &response)
            } else {
                // Compatibility: no context means search all eligible corpora and merge summaries.
                let corpora = list_corpora(&conn, None, false, None).map_err(to_http_err)?;
                let eligible = corpora
                    .into_iter()
                    .filter(|c| is_fcs_enabled(c))
                    .filter(|c| is_http_access_allowed(c, &role) || state.test_mode)
                    .filter(|c| is_http_operation_allowed(c, "query") || state.test_mode)
                    .collect::<Vec<_>>();
                let mut per = Vec::<(CorpusEntry, Value)>::new();
                for c in eligible {
                    if let Ok(resp) = execute_query(&c, &c.id, &query, "auto", start, max, None, None) {
                        per.push((c, resp));
                    }
                }
                build_fcs_search_xml_multi(&query, &per)
            }
        }
        "scan" => build_fcs_scan_not_implemented_xml(),
        _ => build_fcs_diagnostic_xml(&format!("Unsupported FCS operation '{}'", operation)),
    };

    Ok(([(header::CONTENT_TYPE, "application/xml; charset=utf-8")], xml))
}

fn to_http_err(err: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, err.to_string())
}

fn normalize_role(role: Option<&str>) -> String {
    match role.unwrap_or("visitor").trim().to_lowercase().as_str() {
        "admin" | "server_admin" | "corpus_admin" => "admin".to_string(),
        _ => "visitor".to_string(),
    }
}

fn parse_backend_csv(raw: Option<&str>) -> Vec<String> {
    let Some(txt) = raw else {
        return vec!["auto".to_string()];
    };
    let mut out: Vec<String> = txt
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if out.is_empty() {
        out.push("auto".to_string());
    }
    out
}

fn clamp_limit(v: usize, min_v: usize, max_v: usize) -> usize {
    v.max(min_v).min(max_v)
}

fn normalize_reindex_status(status: Option<&str>) -> Option<String> {
    let s = status?.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    Some(s)
}

fn is_http_access_allowed(corpus: &CorpusEntry, role: &str) -> bool {
    match corpus.http_policy_mode.as_str() {
        "disabled" => false,
        "auth_required" => role == "admin",
        "public_query" => true,
        _ => true,
    }
}

fn is_http_operation_allowed(corpus: &CorpusEntry, op: &str) -> bool {
    corpus.http_allowed_operations.iter().any(|x| x == op)
}

fn looks_like_aggregation(query: &str) -> bool {
    let q = query.to_lowercase();
    q.contains("group by")
        || q.contains("tabulate")
        || q.contains("having")
        || q.contains("colloc")
        || q.contains("keyness")
        || q.contains("frequenc")
}

fn resolve_fcs_operation(params: &FcsQuery) -> String {
    if let Some(op) = params.operation.as_deref() {
        let norm = op.trim().to_lowercase();
        if !norm.is_empty() {
            return norm;
        }
    }
    if params.query.as_deref().map(str::trim).filter(|q| !q.is_empty()).is_some() {
        return "searchretrieve".to_string();
    }
    "explain".to_string()
}

fn normalize_pando_query(query: &str) -> String {
    query.trim_end().trim_end_matches(';').trim_end().to_string()
}

fn is_fcs_enabled(corpus: &CorpusEntry) -> bool {
    corpus
        .capabilities
        .get("fcs")
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn fcs_resource_pid(corpus: &CorpusEntry) -> String {
    corpus
        .capabilities
        .get("fcs")
        .and_then(|v| v.get("resource_pid"))
        .and_then(|v| v.as_str())
        .unwrap_or(&corpus.id)
        .to_string()
}

fn fcs_languages(corpus: &CorpusEntry) -> Vec<String> {
    let mut langs: Vec<String> = Vec::new();
    let push_lang = |langs: &mut Vec<String>, value: &str| {
        let t = value.trim();
        if t.is_empty() {
            return;
        }
        if !langs.iter().any(|x| x == t) {
            langs.push(t.to_string());
        }
    };

    if let Some(arr) = corpus
        .capabilities
        .get("fcs")
        .and_then(|v| v.get("languages"))
        .and_then(|v| v.as_array())
    {
        for v in arr {
            if let Some(s) = v.as_str() {
                push_lang(&mut langs, s);
            }
        }
    }
    if langs.is_empty() {
        if let Some(s) = corpus
            .capabilities
            .get("fcs")
            .and_then(|v| v.get("language"))
            .and_then(|v| v.as_str())
        {
            push_lang(&mut langs, s);
        }
    }
    if langs.is_empty() {
        if let Some(arr) = corpus.settings.get("languages").and_then(|v| v.as_array()) {
            for v in arr {
                if let Some(s) = v.as_str() {
                    push_lang(&mut langs, s);
                }
            }
        }
    }
    if langs.is_empty() {
        if let Some(s) = corpus.settings.get("language").and_then(|v| v.as_str()) {
            push_lang(&mut langs, s);
        }
    }
    if langs.is_empty() {
        langs.push("und".to_string());
    }
    langs
}

fn build_fcs_explain_xml(
    corpora: &[CorpusEntry],
    host: &str,
    port: u16,
    database: &str,
    include_endpoint_description: bool,
) -> String {
    let host_xml = xml_escape(host);
    let db_xml = xml_escape(database);
    let title_en = "FlexiCorp corpora";
    let title_local = "FlexiCorp Corpora";
    let desc_en = format!("Search in {} corpora via FCS.", corpora.len());
    let desc_local = "Search in FlexiCorp corpora.";
    let extra = if include_endpoint_description {
        build_fcs_endpoint_description_xml(corpora)
    } else {
        String::new()
    };

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<sruResponse:explainResponse xmlns:sruResponse=\"http://docs.oasis-open.org/ns/search-ws/sruResponse\">\
<sruResponse:version>2.0</sruResponse:version>\
<sruResponse:record>\
<sruResponse:recordSchema>http://explain.z3950.org/dtd/2.0/</sruResponse:recordSchema>\
<sruResponse:recordXMLEscaping>xml</sruResponse:recordXMLEscaping>\
<sruResponse:recordData>\
<zr:explain xmlns:zr=\"http://explain.z3950.org/dtd/2.0/\">\
<zr:serverInfo protocol=\"SRU\" version=\"2.0\" transport=\"http\">\
<zr:host>{host_xml}</zr:host>\
<zr:port>{port}</zr:port>\
<zr:database>{db_xml}</zr:database>\
</zr:serverInfo>\
<zr:databaseInfo>\
<zr:title lang=\"en\" primary=\"true\">{title_en}</zr:title>\
<zr:title lang=\"local\">{title_local}</zr:title>\
<zr:description lang=\"en\" primary=\"true\">{desc_en}</zr:description>\
<zr:description lang=\"local\">{desc_local}</zr:description>\
<zr:author lang=\"en\" primary=\"true\">FlexiCorp</zr:author>\
</zr:databaseInfo>\
<zr:indexInfo>\
<zr:set identifier=\"http://clarin.eu/fcs/resource\" name=\"fcs\">\
<zr:title lang=\"en\" primary=\"true\">CLARIN Content Search</zr:title>\
</zr:set>\
<zr:index search=\"true\" scan=\"false\" sort=\"false\">\
<zr:title lang=\"en\" primary=\"true\">Words</zr:title>\
<zr:map primary=\"true\"><zr:name set=\"fcs\">words</zr:name></zr:map>\
</zr:index>\
</zr:indexInfo>\
<zr:schemaInfo>\
<zr:schema identifier=\"http://clarin.eu/fcs/resource\" name=\"fcs\">\
<zr:title lang=\"en\" primary=\"true\">CLARIN Content Search</zr:title>\
</zr:schema>\
</zr:schemaInfo>\
<zr:configInfo>\
<zr:default type=\"numberOfRecords\">250</zr:default>\
<zr:setting type=\"maximumRecords\">1000</zr:setting>\
</zr:configInfo>\
</zr:explain>\
</sruResponse:recordData>\
</sruResponse:record>\
<sruResponse:echoedExplainRequest>\
<sruResponse:version>2.0</sruResponse:version>\
</sruResponse:echoedExplainRequest>\
{extra}\
</sruResponse:explainResponse>"
    )
}

fn build_fcs_endpoint_description_xml(corpora: &[CorpusEntry]) -> String {
    let mut resources_xml = String::new();
    for corpus in corpora {
        let pid = xml_escape(&fcs_resource_pid(corpus));
        let title = xml_escape(&corpus.label);
        let desc = corpus
            .capabilities
            .get("fcs")
            .and_then(|v| v.get("description"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let desc_xml = xml_escape(desc);
        let landing = corpus.project_url.clone().unwrap_or_default();
        let landing_xml = xml_escape(&landing);
        let dataviews = corpus
            .capabilities
            .get("fcs")
            .and_then(|v| v.get("supports_dataviews"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "hits".to_string());
        let layers = corpus
            .capabilities
            .get("fcs")
            .and_then(|v| v.get("supported_layers"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "word".to_string());
        let languages_xml = fcs_languages(corpus)
            .into_iter()
            .map(|lang| format!("<ed:Language>{}</ed:Language>", xml_escape(&lang)))
            .collect::<Vec<_>>()
            .join("");

        resources_xml.push_str(&format!(
            "<ed:Resource pid=\"{pid}\">\
<ed:Title xml:lang=\"en\">{title}</ed:Title>\
<ed:Description xml:lang=\"en\">{desc_xml}</ed:Description>\
<ed:LandingPageURI>{landing_xml}</ed:LandingPageURI>\
<ed:Languages>{languages_xml}</ed:Languages>\
<ed:AvailableDataViews ref=\"{dataviews}\"/>\
<ed:AvailableLayers ref=\"{layers}\"/>\
</ed:Resource>"
        ));
    }

    format!(
        "<sruResponse:extraResponseData>\
<ed:EndpointDescription xmlns:ed=\"http://clarin.eu/fcs/endpoint-description\" version=\"2\">\
<ed:Capabilities>\
<ed:Capability>http://clarin.eu/fcs/capability/basic-search</ed:Capability>\
</ed:Capabilities>\
<ed:SupportedDataViews>\
<ed:SupportedDataView id=\"hits\" delivery-policy=\"send-by-default\">application/x-clarin-fcs-hits+xml</ed:SupportedDataView>\
</ed:SupportedDataViews>\
<ed:SupportedLayers>\
<ed:SupportedLayer id=\"word\" qualifier=\"word\" result-id=\"http://clarin.dk/ns/fcs/layer/word\">text</ed:SupportedLayer>\
</ed:SupportedLayers>\
<ed:Resources>{resources_xml}</ed:Resources>\
</ed:EndpointDescription>\
</sruResponse:extraResponseData>"
    )
}

fn build_fcs_search_xml(corpus: &CorpusEntry, query: &str, response: &Value) -> String {
    let total = response
        .get("raw")
        .and_then(|r| r.get("done"))
        .and_then(|d| d.get("result"))
        .and_then(|r| r.get("total"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let pid = xml_escape(&fcs_resource_pid(corpus));
    let q = xml_escape(query);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<sru:searchRetrieveResponse xmlns:sru=\"http://docs.oasis-open.org/ns/search-ws/sruResponse\" \
xmlns:fcs=\"http://clarin.eu/fcs/resource\">\
<sru:version>2.0</sru:version>\
<sru:numberOfRecords>{total}</sru:numberOfRecords>\
<sru:echoedSearchRetrieveRequest><sru:query>{q}</sru:query></sru:echoedSearchRetrieveRequest>\
<sru:records>\
<sru:record>\
<sru:recordSchema>http://clarin.eu/fcs/resource</sru:recordSchema>\
<sru:recordData><fcs:Resource pid=\"{pid}\"><fcs:DataView type=\"hits\"/></fcs:Resource></sru:recordData>\
</sru:record>\
</sru:records>\
</sru:searchRetrieveResponse>"
    )
}

fn build_fcs_search_xml_multi(query: &str, results: &[(CorpusEntry, Value)]) -> String {
    let mut records_xml = String::new();
    let mut total_sum: i64 = 0;
    for (corpus, response) in results {
        let total = response
            .get("raw")
            .and_then(|r| r.get("done"))
            .and_then(|d| d.get("result"))
            .and_then(|r| r.get("total"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        total_sum += total;
        let pid = xml_escape(&fcs_resource_pid(corpus));
        records_xml.push_str(&format!(
            "<sru:record><sru:recordSchema>http://clarin.eu/fcs/resource</sru:recordSchema>\
<sru:recordData><fcs:Resource pid=\"{pid}\"><fcs:DataView type=\"hits\"/></fcs:Resource></sru:recordData></sru:record>"
        ));
    }
    let q = xml_escape(query);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<sru:searchRetrieveResponse xmlns:sru=\"http://docs.oasis-open.org/ns/search-ws/sruResponse\" \
xmlns:fcs=\"http://clarin.eu/fcs/resource\">\
<sru:version>2.0</sru:version>\
<sru:numberOfRecords>{total_sum}</sru:numberOfRecords>\
<sru:echoedSearchRetrieveRequest><sru:query>{q}</sru:query></sru:echoedSearchRetrieveRequest>\
<sru:records>{records_xml}</sru:records>\
</sru:searchRetrieveResponse>"
    )
}

fn build_fcs_scan_not_implemented_xml() -> String {
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<sru:scanResponse xmlns:sru=\"http://docs.oasis-open.org/ns/search-ws/sruResponse\">\
<sru:version>2.0</sru:version>\
<sru:diagnostics><sru:diagnostic>scan is not implemented yet</sru:diagnostic></sru:diagnostics>\
</sru:scanResponse>"
        .to_string()
}

fn build_fcs_diagnostic_xml(msg: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<sru:diagnostics xmlns:sru=\"http://docs.oasis-open.org/ns/search-ws/sruResponse\">\
<sru:diagnostic>{}</sru:diagnostic>\
</sru:diagnostics>",
        xml_escape(msg)
    )
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn open_db(path: &PathBuf) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create parent directory '{}' for SQLite database",
                    parent.display()
                )
            })?;
        }
    }
    let conn = Connection::open(path).with_context(|| {
        format!(
            "Failed to open SQLite database '{}'",
            path.as_path().display()
        )
    })?;
    init_schema(&conn)?;
    Ok(conn)
}

/// Maps SQLite write failures so common permission issues surface a clear hint.
fn sqlite_write_err(op: &'static str, e: SqliteError) -> anyhow::Error {
    let s = e.to_string();
    if s.to_lowercase().contains("readonly") {
        anyhow::anyhow!(
            "{op}: {s}\n\
            Hint: SQLite is read-only for this process. The database file and its parent directory must be writable (SQLite needs to create -journal/-shm/-wal files there). Set `FQS_DB_PATH` to a writable path, use `--db`, or fix ownership (e.g. `chown`/`chmod` so the user that runs `fqs`—often `www-data` under Apache—can write the catalog directory). See `fqs` README «Database»."
        )
    } else {
        anyhow::anyhow!("{op}: {s}")
    }
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS corpora (
  id TEXT PRIMARY KEY,
  label TEXT NOT NULL,
  project_root TEXT NOT NULL,
  project_url TEXT,
  preferred_backend TEXT NOT NULL,
  environment TEXT NOT NULL DEFAULT 'live',
  visibility TEXT NOT NULL DEFAULT 'published',
  listing_visibility TEXT NOT NULL DEFAULT 'public',
  family_key TEXT,
  family_label TEXT,
  version_tag TEXT,
  source_kind TEXT NOT NULL DEFAULT 'generic',
  supports_xml INTEGER NOT NULL DEFAULT 0 CHECK(supports_xml IN (0,1)),
  http_policy_mode TEXT NOT NULL DEFAULT 'public_query',
  http_allowed_operations_json TEXT NOT NULL DEFAULT '["query","catalog"]',
  interfaces_json TEXT NOT NULL DEFAULT '[]',
  capabilities_json TEXT NOT NULL DEFAULT '{}',
  settings_json TEXT NOT NULL DEFAULT '{}',
  first_corpus_update_at TEXT,
  last_corpus_update_at TEXT,
  corpus_size INTEGER,
  corpus_size_updated_at TEXT,
  last_validated_at TEXT,
  last_validation_ok INTEGER CHECK(last_validation_ok IN (0,1)),
  last_validation_message TEXT,
  is_current INTEGER NOT NULL DEFAULT 1 CHECK(is_current IN (0,1)),
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_corpora_environment ON corpora(environment);
CREATE INDEX IF NOT EXISTS idx_corpora_is_current ON corpora(is_current);
CREATE INDEX IF NOT EXISTS idx_corpora_family_key ON corpora(family_key);

CREATE TABLE IF NOT EXISTS active_sessions (
  session_id TEXT PRIMARY KEY,
  role TEXT,
  corpus_id TEXT,
  backend TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_seen_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_active_sessions_last_seen_at ON active_sessions(last_seen_at);

CREATE TABLE IF NOT EXISTS reindex_jobs (
  job_id TEXT PRIMARY KEY,
  corpus_id TEXT NOT NULL,
  status TEXT NOT NULL,
  priority INTEGER NOT NULL DEFAULT 0,
  requested_backends_json TEXT NOT NULL DEFAULT '[]',
  requested_by_role TEXT,
  origin TEXT,
  message TEXT,
  last_error TEXT,
  worker_id TEXT,
  request_json TEXT NOT NULL DEFAULT '{}',
  result_json TEXT NOT NULL DEFAULT '{}',
  requested_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  started_at TEXT,
  finished_at TEXT,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_reindex_jobs_status_priority ON reindex_jobs(status, priority DESC, requested_at ASC);
CREATE INDEX IF NOT EXISTS idx_reindex_jobs_corpus ON reindex_jobs(corpus_id);

CREATE TABLE IF NOT EXISTS reindex_history (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  corpus_id TEXT NOT NULL,
  job_id TEXT,
  event TEXT NOT NULL,
  details_json TEXT NOT NULL DEFAULT '{}',
  at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_reindex_history_corpus_at ON reindex_history(corpus_id, at DESC);
CREATE INDEX IF NOT EXISTS idx_reindex_history_job_at ON reindex_history(job_id, at DESC);

CREATE TABLE IF NOT EXISTS reindex_workers (
  worker_id TEXT PRIMARY KEY,
  status TEXT NOT NULL DEFAULT 'online',
  max_concurrent INTEGER NOT NULL DEFAULT 1,
  host TEXT,
  capabilities_json TEXT NOT NULL DEFAULT '[]',
  last_heartbeat_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_reindex_workers_heartbeat ON reindex_workers(last_heartbeat_at DESC);
"#,
    )
    .context("Failed to initialize schema")?;
    ensure_column(
        conn,
        "corpora",
        "project_url",
        "ALTER TABLE corpora ADD COLUMN project_url TEXT",
    )?;
    ensure_column(
        conn,
        "corpora",
        "http_policy_mode",
        "ALTER TABLE corpora ADD COLUMN http_policy_mode TEXT NOT NULL DEFAULT 'public_query'",
    )?;
    ensure_column(
        conn,
        "corpora",
        "http_allowed_operations_json",
        "ALTER TABLE corpora ADD COLUMN http_allowed_operations_json TEXT NOT NULL DEFAULT '[\"query\",\"catalog\"]'",
    )?;
    ensure_column(
        conn,
        "corpora",
        "source_kind",
        "ALTER TABLE corpora ADD COLUMN source_kind TEXT NOT NULL DEFAULT 'generic'",
    )?;
    ensure_column(
        conn,
        "corpora",
        "supports_xml",
        "ALTER TABLE corpora ADD COLUMN supports_xml INTEGER NOT NULL DEFAULT 0 CHECK(supports_xml IN (0,1))",
    )?;
    ensure_column(
        conn,
        "corpora",
        "interfaces_json",
        "ALTER TABLE corpora ADD COLUMN interfaces_json TEXT NOT NULL DEFAULT '[]'",
    )?;
    ensure_column(
        conn,
        "corpora",
        "labels_json",
        "ALTER TABLE corpora ADD COLUMN labels_json TEXT NOT NULL DEFAULT '[]'",
    )?;
    ensure_column(
        conn,
        "corpora",
        "capabilities_json",
        "ALTER TABLE corpora ADD COLUMN capabilities_json TEXT NOT NULL DEFAULT '{}'",
    )?;
    ensure_column(
        conn,
        "corpora",
        "settings_json",
        "ALTER TABLE corpora ADD COLUMN settings_json TEXT NOT NULL DEFAULT '{}'",
    )?;
    ensure_column(
        conn,
        "corpora",
        "first_corpus_update_at",
        "ALTER TABLE corpora ADD COLUMN first_corpus_update_at TEXT",
    )?;
    ensure_column(
        conn,
        "corpora",
        "last_corpus_update_at",
        "ALTER TABLE corpora ADD COLUMN last_corpus_update_at TEXT",
    )?;
    ensure_column(
        conn,
        "corpora",
        "corpus_size",
        "ALTER TABLE corpora ADD COLUMN corpus_size INTEGER",
    )?;
    ensure_column(
        conn,
        "corpora",
        "corpus_size_updated_at",
        "ALTER TABLE corpora ADD COLUMN corpus_size_updated_at TEXT",
    )?;
    ensure_column(
        conn,
        "corpora",
        "last_validated_at",
        "ALTER TABLE corpora ADD COLUMN last_validated_at TEXT",
    )?;
    ensure_column(
        conn,
        "corpora",
        "last_validation_ok",
        "ALTER TABLE corpora ADD COLUMN last_validation_ok INTEGER CHECK(last_validation_ok IN (0,1))",
    )?;
    ensure_column(
        conn,
        "corpora",
        "last_validation_message",
        "ALTER TABLE corpora ADD COLUMN last_validation_message TEXT",
    )?;
    ensure_column(
        conn,
        "corpora",
        "corpus_version",
        "ALTER TABLE corpora ADD COLUMN corpus_version TEXT",
    )?;
    ensure_column(
        conn,
        "corpora",
        "interface_preference",
        "ALTER TABLE corpora ADD COLUMN interface_preference TEXT",
    )?;
    Ok(())
}

fn ensure_column(conn: &Connection, table: &str, column: &str, alter_sql: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(());
        }
    }
    conn.execute(alter_sql, [])
        .with_context(|| format!("Failed to apply migration for column '{column}'"))?;
    Ok(())
}

fn row_to_corpus(row: &rusqlite::Row<'_>) -> rusqlite::Result<CorpusEntry> {
    let interfaces_json: String = row.get("interfaces_json")?;
    let allowed_ops_json: String = row.get("http_allowed_operations_json")?;
    let capabilities_json: String = row.get("capabilities_json")?;
    let settings_json: String = row.get("settings_json")?;
    let interfaces = serde_json::from_str::<Vec<String>>(&interfaces_json)
        .unwrap_or_else(|_| Vec::new());
    let labels_json: String = row.get("labels_json")?;
    let labels = serde_json::from_str::<Vec<String>>(&labels_json).unwrap_or_else(|_| Vec::new());
    let http_allowed_operations =
        serde_json::from_str::<Vec<String>>(&allowed_ops_json).unwrap_or_else(|_| {
            default_http_allowed_operations()
        });
    let capabilities =
        serde_json::from_str::<Value>(&capabilities_json).unwrap_or_else(|_| json!({}));
    let settings = serde_json::from_str::<Value>(&settings_json).unwrap_or_else(|_| json!({}));
    Ok(CorpusEntry {
        id: row.get("id")?,
        label: row.get("label")?,
        project_root: PathBuf::from(row.get::<_, String>("project_root")?),
        project_url: row.get("project_url")?,
        preferred_backend: row.get("preferred_backend")?,
        environment: row.get("environment")?,
        visibility: row.get("visibility")?,
        listing_visibility: row.get("listing_visibility")?,
        family_key: row.get("family_key")?,
        family_label: row.get("family_label")?,
        version_tag: row.get("version_tag")?,
        corpus_version: row.get("corpus_version")?,
        interface_preference: row.get("interface_preference")?,
        source_kind: row.get("source_kind")?,
        supports_xml: row.get::<_, i64>("supports_xml")? == 1,
        http_policy_mode: row.get("http_policy_mode")?,
        http_allowed_operations,
        interfaces,
        labels,
        capabilities,
        settings,
        first_corpus_update_at: row.get("first_corpus_update_at")?,
        last_corpus_update_at: row.get("last_corpus_update_at")?,
        corpus_size: row.get("corpus_size")?,
        corpus_size_updated_at: row.get("corpus_size_updated_at")?,
        last_validated_at: row.get("last_validated_at")?,
        last_validation_ok: row.get::<_, Option<i64>>("last_validation_ok")?.map(|v| v == 1),
        last_validation_message: row.get("last_validation_message")?,
        is_current: row.get::<_, i64>("is_current")? == 1,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn row_to_reindex_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReindexJobEntry> {
    let requested_backends_json: String = row.get("requested_backends_json")?;
    let request_json: String = row.get("request_json")?;
    let result_json: String = row.get("result_json")?;
    let requested_backends =
        serde_json::from_str::<Vec<String>>(&requested_backends_json).unwrap_or_default();
    let request = serde_json::from_str::<Value>(&request_json).unwrap_or_else(|_| json!({}));
    let result = serde_json::from_str::<Value>(&result_json).unwrap_or_else(|_| json!({}));
    Ok(ReindexJobEntry {
        job_id: row.get("job_id")?,
        corpus_id: row.get("corpus_id")?,
        status: row.get("status")?,
        priority: row.get("priority")?,
        requested_backends,
        requested_by_role: row.get("requested_by_role")?,
        origin: row.get("origin")?,
        message: row.get("message")?,
        last_error: row.get("last_error")?,
        worker_id: row.get("worker_id")?,
        requested_at: row.get("requested_at")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
        updated_at: row.get("updated_at")?,
        request,
        result,
    })
}

fn row_to_reindex_history(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReindexHistoryEntry> {
    let details_json: String = row.get("details_json")?;
    let details = serde_json::from_str::<Value>(&details_json).unwrap_or_else(|_| json!({}));
    Ok(ReindexHistoryEntry {
        id: row.get("id")?,
        corpus_id: row.get("corpus_id")?,
        job_id: row.get("job_id")?,
        event: row.get("event")?,
        at: row.get("at")?,
        details,
    })
}

fn row_to_reindex_worker(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReindexWorkerEntry> {
    let caps_json: String = row.get("capabilities_json")?;
    let capabilities = serde_json::from_str::<Vec<String>>(&caps_json).unwrap_or_default();
    let worker_id: String = row.get("worker_id")?;
    let running_jobs: i64 = row.get("running_jobs")?;
    Ok(ReindexWorkerEntry {
        worker_id,
        status: row.get("status")?,
        max_concurrent: row.get("max_concurrent")?,
        host: row.get("host")?,
        capabilities,
        running_jobs,
        last_heartbeat_at: row.get("last_heartbeat_at")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn make_reindex_job_id(corpus_id: &str) -> String {
    let ts = OffsetDateTime::now_utc().unix_timestamp_nanos();
    let cid: String = corpus_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("rj-{}-{}", ts, cid)
}

fn append_reindex_history_event(
    conn: &Connection,
    corpus_id: &str,
    job_id: Option<&str>,
    event: &str,
    details: &Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO reindex_history (corpus_id, job_id, event, details_json, at) VALUES (?1, ?2, ?3, ?4, CURRENT_TIMESTAMP)",
        params![
            corpus_id,
            job_id,
            event,
            serde_json::to_string(details).unwrap_or_else(|_| "{}".to_string())
        ],
    )
    .map_err(|e| sqlite_write_err("insert reindex_history", e))?;
    Ok(())
}

fn upsert_reindex_worker_heartbeat(
    conn: &Connection,
    worker_id: &str,
    max_concurrent: i64,
    host: Option<&str>,
    capabilities: &[String],
) -> Result<ReindexWorkerEntry> {
    conn.execute(
        r#"
INSERT INTO reindex_workers
(worker_id, status, max_concurrent, host, capabilities_json, last_heartbeat_at, created_at, updated_at)
VALUES (?1, 'online', ?2, ?3, ?4, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
ON CONFLICT(worker_id) DO UPDATE SET
  status='online',
  max_concurrent=excluded.max_concurrent,
  host=COALESCE(excluded.host, reindex_workers.host),
  capabilities_json=excluded.capabilities_json,
  last_heartbeat_at=CURRENT_TIMESTAMP,
  updated_at=CURRENT_TIMESTAMP
"#,
        params![
            worker_id,
            max_concurrent.max(1),
            host,
            serde_json::to_string(capabilities).unwrap_or_else(|_| "[]".to_string())
        ],
    )
    .map_err(|e| sqlite_write_err("upsert reindex_workers", e))?;
    get_reindex_worker(conn, worker_id)
}

fn get_reindex_worker(conn: &Connection, worker_id: &str) -> Result<ReindexWorkerEntry> {
    conn.query_row(
        r#"
SELECT w.worker_id, w.status, w.max_concurrent, w.host, w.capabilities_json,
       w.last_heartbeat_at, w.created_at, w.updated_at,
       COALESCE(r.running_jobs, 0) AS running_jobs
FROM reindex_workers w
LEFT JOIN (
  SELECT worker_id, COUNT(1) AS running_jobs
  FROM reindex_jobs
  WHERE status = 'running'
  GROUP BY worker_id
) r ON r.worker_id = w.worker_id
WHERE w.worker_id = ?1
"#,
        params![worker_id],
        row_to_reindex_worker,
    )
    .with_context(|| format!("Reindex worker '{}' not found", worker_id))
}

fn list_reindex_workers(conn: &Connection) -> Result<Vec<ReindexWorkerEntry>> {
    let mut stmt = conn.prepare(
        r#"
SELECT w.worker_id, w.status, w.max_concurrent, w.host, w.capabilities_json,
       w.last_heartbeat_at, w.created_at, w.updated_at,
       COALESCE(r.running_jobs, 0) AS running_jobs
FROM reindex_workers w
LEFT JOIN (
  SELECT worker_id, COUNT(1) AS running_jobs
  FROM reindex_jobs
  WHERE status = 'running'
  GROUP BY worker_id
) r ON r.worker_id = w.worker_id
WHERE w.status = 'online' AND w.last_heartbeat_at >= datetime('now', '-120 seconds')
ORDER BY w.last_heartbeat_at DESC, w.worker_id ASC
"#,
    )?;
    let rows = stmt
        .query_map([], row_to_reindex_worker)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn pick_next_queued_job_for_worker(
    conn: &Connection,
    worker: &ReindexWorkerEntry,
) -> Result<Option<ReindexJobEntry>> {
    let mut stmt = conn.prepare(
        r#"
SELECT job_id, corpus_id, status, priority, requested_backends_json, requested_by_role, origin, message, last_error, worker_id, request_json, result_json, requested_at, started_at, finished_at, updated_at
FROM reindex_jobs
WHERE status = 'queued'
ORDER BY priority DESC, requested_at ASC
LIMIT 50
"#,
    )?;
    let jobs = stmt
        .query_map([], row_to_reindex_job)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let caps: std::collections::HashSet<String> =
        worker.capabilities.iter().map(|x| x.trim().to_lowercase()).collect();
    for job in jobs {
        if caps.is_empty() || caps.contains("auto") {
            return Ok(Some(job));
        }
        let needed: Vec<String> = if job.requested_backends.is_empty() {
            vec!["auto".to_string()]
        } else {
            job.requested_backends
                .iter()
                .map(|x| x.trim().to_lowercase())
                .filter(|x| !x.is_empty())
                .collect()
        };
        let ok = needed
            .iter()
            .all(|b| b == "auto" || caps.contains(b) || (b == "clickql" && caps.contains("clickhouse")));
        if ok {
            return Ok(Some(job));
        }
    }
    Ok(None)
}

fn dispatch_reindex_once(conn: &Connection, default_worker_max_concurrent: i64) -> Result<Vec<ReindexJobEntry>> {
    let workers = list_reindex_workers(conn)?;
    let mut assigned: Vec<ReindexJobEntry> = Vec::new();
    for mut w in workers {
        if w.max_concurrent <= 0 {
            w.max_concurrent = default_worker_max_concurrent.max(1);
        }
        let available_slots = (w.max_concurrent - w.running_jobs).max(0);
        if available_slots <= 0 {
            continue;
        }
        for _ in 0..available_slots {
            let maybe_job = pick_next_queued_job_for_worker(conn, &w)?;
            let Some(job) = maybe_job else {
                break;
            };
            let started = mark_reindex_job_started(conn, &job.job_id, Some(&w.worker_id))?;
            append_reindex_history_event(
                conn,
                &started.corpus_id,
                Some(&started.job_id),
                "dispatched",
                &json!({
                    "worker_id": w.worker_id,
                    "max_concurrent": w.max_concurrent
                }),
            )?;
            assigned.push(started);
            w.running_jobs += 1;
        }
    }
    Ok(assigned)
}

fn dispatch_reindex_once_path(db_path: &Path, default_worker_max_concurrent: i64) -> Result<Vec<ReindexJobEntry>> {
    let conn = open_db(&db_path.to_path_buf())?;
    dispatch_reindex_once(&conn, default_worker_max_concurrent)
}

fn enqueue_reindex_job(
    conn: &Connection,
    corpus_id: &str,
    requested_backends: &[String],
    priority: i64,
    requested_by_role: Option<&str>,
    origin: Option<&str>,
    message: Option<&str>,
    request: &Value,
) -> Result<ReindexJobEntry> {
    let job_id = make_reindex_job_id(corpus_id);
    conn.execute(
        r#"
INSERT INTO reindex_jobs
(job_id, corpus_id, status, priority, requested_backends_json, requested_by_role, origin, message, request_json, result_json, requested_at, updated_at)
VALUES (?1, ?2, 'queued', ?3, ?4, ?5, ?6, ?7, ?8, '{}', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
"#,
        params![
            job_id,
            corpus_id,
            priority,
            serde_json::to_string(requested_backends).unwrap_or_else(|_| "[]".to_string()),
            requested_by_role,
            origin,
            message,
            serde_json::to_string(request).unwrap_or_else(|_| "{}".to_string())
        ],
    )
    .map_err(|e| sqlite_write_err("insert reindex_jobs", e))?;
    append_reindex_history_event(
        conn,
        corpus_id,
        Some(&job_id),
        "queued",
        &json!({
            "priority": priority,
            "requested_backends": requested_backends,
            "requested_by_role": requested_by_role,
            "origin": origin,
            "message": message
        }),
    )?;
    get_reindex_job(conn, &job_id)
}

fn get_reindex_job(conn: &Connection, job_id: &str) -> Result<ReindexJobEntry> {
    conn.query_row(
        r#"SELECT job_id, corpus_id, status, priority, requested_backends_json, requested_by_role, origin, message, last_error, worker_id, request_json, result_json, requested_at, started_at, finished_at, updated_at
           FROM reindex_jobs WHERE job_id = ?1"#,
        params![job_id],
        row_to_reindex_job,
    )
    .with_context(|| format!("Reindex job '{}' not found", job_id))
}

fn list_reindex_jobs(
    conn: &Connection,
    status: Option<&str>,
    corpus: Option<&str>,
    limit: usize,
) -> Result<Vec<ReindexJobEntry>> {
    let st = normalize_reindex_status(status);
    let mut sql = String::from(
        "SELECT job_id, corpus_id, status, priority, requested_backends_json, requested_by_role, origin, message, last_error, worker_id, request_json, result_json, requested_at, started_at, finished_at, updated_at FROM reindex_jobs WHERE 1=1",
    );
    if st.is_some() {
        sql.push_str(" AND status = ?1");
        if corpus.is_some() {
            sql.push_str(" AND corpus_id = ?2");
            sql.push_str(" ORDER BY CASE status WHEN 'running' THEN 0 WHEN 'queued' THEN 1 ELSE 2 END, priority DESC, requested_at ASC LIMIT ?3");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![st, corpus, limit as i64], row_to_reindex_job)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            return Ok(rows);
        }
        sql.push_str(" ORDER BY CASE status WHEN 'running' THEN 0 WHEN 'queued' THEN 1 ELSE 2 END, priority DESC, requested_at ASC LIMIT ?2");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![st, limit as i64], row_to_reindex_job)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        return Ok(rows);
    }
    if corpus.is_some() {
        sql.push_str(" AND corpus_id = ?1");
        sql.push_str(" ORDER BY CASE status WHEN 'running' THEN 0 WHEN 'queued' THEN 1 ELSE 2 END, priority DESC, requested_at ASC LIMIT ?2");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![corpus, limit as i64], row_to_reindex_job)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        return Ok(rows);
    }
    sql.push_str(
        " ORDER BY CASE status WHEN 'running' THEN 0 WHEN 'queued' THEN 1 ELSE 2 END, priority DESC, requested_at ASC LIMIT ?1",
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![limit as i64], row_to_reindex_job)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn list_reindex_history(
    conn: &Connection,
    corpus: Option<&str>,
    limit: usize,
) -> Result<Vec<ReindexHistoryEntry>> {
    let sql = if corpus.is_some() {
        "SELECT id, corpus_id, job_id, event, details_json, at FROM reindex_history WHERE corpus_id = ?1 ORDER BY at DESC, id DESC LIMIT ?2"
    } else {
        "SELECT id, corpus_id, job_id, event, details_json, at FROM reindex_history ORDER BY at DESC, id DESC LIMIT ?1"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = if let Some(c) = corpus {
        stmt.query_map(params![c, limit as i64], row_to_reindex_history)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map(params![limit as i64], row_to_reindex_history)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    Ok(rows)
}

fn mark_reindex_job_started(
    conn: &Connection,
    job_id: &str,
    worker_id: Option<&str>,
) -> Result<ReindexJobEntry> {
    let existing = get_reindex_job(conn, job_id)?;
    conn.execute(
        "UPDATE reindex_jobs SET status='running', worker_id=?2, started_at=COALESCE(started_at, CURRENT_TIMESTAMP), updated_at=CURRENT_TIMESTAMP WHERE job_id=?1",
        params![job_id, worker_id],
    )
    .map_err(|e| sqlite_write_err("update reindex_jobs started", e))?;
    append_reindex_history_event(
        conn,
        &existing.corpus_id,
        Some(job_id),
        "started",
        &json!({"worker_id": worker_id}),
    )?;
    get_reindex_job(conn, job_id)
}

fn mark_reindex_job_finished(
    conn: &Connection,
    job_id: &str,
    ok: bool,
    message: Option<&str>,
    error: Option<&str>,
    result: Option<&Value>,
) -> Result<ReindexJobEntry> {
    let existing = get_reindex_job(conn, job_id)?;
    let status = if ok { "completed" } else { "failed" };
    conn.execute(
        "UPDATE reindex_jobs SET status=?2, message=?3, last_error=?4, result_json=?5, finished_at=CURRENT_TIMESTAMP, updated_at=CURRENT_TIMESTAMP WHERE job_id=?1",
        params![
            job_id,
            status,
            message,
            error,
            serde_json::to_string(&result.cloned().unwrap_or_else(|| json!({}))).unwrap_or_else(|_| "{}".to_string())
        ],
    )
    .map_err(|e| sqlite_write_err("update reindex_jobs finished", e))?;
    append_reindex_history_event(
        conn,
        &existing.corpus_id,
        Some(job_id),
        if ok { "completed" } else { "failed" },
        &json!({"message": message, "error": error}),
    )?;
    if ok {
        append_reindex_history_event(
            conn,
            &existing.corpus_id,
            Some(job_id),
            "indexed",
            &json!({"message": message}),
        )?;
    }
    get_reindex_job(conn, job_id)
}

fn list_corpora(
    conn: &Connection,
    environment: Option<&str>,
    include_noncurrent: bool,
    tag: Option<&str>,
) -> Result<Vec<CorpusEntry>> {
    let mut sql = String::from(
        "SELECT id,label,project_root,project_url,preferred_backend,environment,visibility,listing_visibility,family_key,family_label,version_tag,corpus_version,interface_preference,source_kind,supports_xml,http_policy_mode,http_allowed_operations_json,interfaces_json,labels_json,capabilities_json,settings_json,first_corpus_update_at,last_corpus_update_at,corpus_size,corpus_size_updated_at,last_validated_at,last_validation_ok,last_validation_message,is_current,created_at,updated_at FROM corpora WHERE 1=1",
    );
    if environment.is_some() {
        sql.push_str(" AND environment = ?1");
    }
    if !include_noncurrent {
        sql.push_str(" AND is_current = 1");
    }
    sql.push_str(" ORDER BY COALESCE(family_key, id), label, id");

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = if let Some(env) = environment {
        stmt.query_map(params![env], row_to_corpus)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map([], row_to_corpus)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if let Some(tag) = tag {
        let t = tag.trim();
        if !t.is_empty() {
            rows.retain(|c| c.labels.iter().any(|l| l.eq_ignore_ascii_case(t)));
        }
    }
    Ok(rows)
}

fn corpus_exists(conn: &Connection, id: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(1) FROM corpora WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

fn delete_corpus(conn: &Connection, id: &str) -> Result<usize> {
    let n = conn
        .execute("DELETE FROM corpora WHERE id = ?1", params![id])
        .context("Failed to delete corpus row")?;
    Ok(n)
}

fn get_corpus(conn: &Connection, id: &str) -> Result<CorpusEntry> {
    conn.query_row(
        "SELECT id,label,project_root,project_url,preferred_backend,environment,visibility,listing_visibility,family_key,family_label,version_tag,corpus_version,interface_preference,source_kind,supports_xml,http_policy_mode,http_allowed_operations_json,interfaces_json,labels_json,capabilities_json,settings_json,first_corpus_update_at,last_corpus_update_at,corpus_size,corpus_size_updated_at,last_validated_at,last_validation_ok,last_validation_message,is_current,created_at,updated_at FROM corpora WHERE id = ?1",
        params![id],
        row_to_corpus,
    )
    .with_context(|| format!("Corpus '{}' not found in database", id))
}

fn upsert_corpus(conn: &Connection, entry: &CorpusEntry) -> Result<()> {
    conn.execute(
        r#"
INSERT INTO corpora
(id,label,project_root,project_url,preferred_backend,environment,visibility,listing_visibility,family_key,family_label,version_tag,corpus_version,interface_preference,source_kind,supports_xml,http_policy_mode,http_allowed_operations_json,interfaces_json,labels_json,capabilities_json,settings_json,first_corpus_update_at,last_corpus_update_at,corpus_size,corpus_size_updated_at,last_validated_at,last_validation_ok,last_validation_message,is_current)
VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,COALESCE(?22, CURRENT_TIMESTAMP),COALESCE(?23, CURRENT_TIMESTAMP),?24,?25,?26,?27,?28,?29)
ON CONFLICT(id) DO UPDATE SET
  label=excluded.label,
  project_root=excluded.project_root,
  project_url=excluded.project_url,
  preferred_backend=excluded.preferred_backend,
  environment=excluded.environment,
  visibility=excluded.visibility,
  listing_visibility=excluded.listing_visibility,
  family_key=excluded.family_key,
  family_label=excluded.family_label,
  version_tag=excluded.version_tag,
  corpus_version=excluded.corpus_version,
  interface_preference=excluded.interface_preference,
  source_kind=excluded.source_kind,
  supports_xml=excluded.supports_xml,
  http_policy_mode=excluded.http_policy_mode,
  http_allowed_operations_json=excluded.http_allowed_operations_json,
  interfaces_json=excluded.interfaces_json,
  labels_json=excluded.labels_json,
  capabilities_json=excluded.capabilities_json,
  settings_json=excluded.settings_json,
  first_corpus_update_at=COALESCE(first_corpus_update_at, excluded.first_corpus_update_at, CURRENT_TIMESTAMP),
  last_corpus_update_at=COALESCE(excluded.last_corpus_update_at, last_corpus_update_at, CURRENT_TIMESTAMP),
  corpus_size=COALESCE(excluded.corpus_size, corpus_size),
  corpus_size_updated_at=COALESCE(excluded.corpus_size_updated_at, corpus_size_updated_at),
  last_validated_at=COALESCE(excluded.last_validated_at, last_validated_at),
  last_validation_ok=COALESCE(excluded.last_validation_ok, last_validation_ok),
  last_validation_message=COALESCE(excluded.last_validation_message, last_validation_message),
  is_current=excluded.is_current,
  updated_at=CURRENT_TIMESTAMP
"#,
        params![
            entry.id,
            entry.label,
            entry.project_root.to_string_lossy().to_string(),
            entry.project_url,
            entry.preferred_backend,
            entry.environment,
            entry.visibility,
            entry.listing_visibility,
            entry.family_key,
            entry.family_label,
            entry.version_tag,
            entry.corpus_version,
            entry.interface_preference,
            entry.source_kind,
            if entry.supports_xml { 1 } else { 0 },
            entry.http_policy_mode,
            serde_json::to_string(&entry.http_allowed_operations)?,
            serde_json::to_string(&entry.interfaces)?,
            serde_json::to_string(&entry.labels)?,
            serde_json::to_string(&entry.capabilities)?,
            serde_json::to_string(&entry.settings)?,
            entry.first_corpus_update_at,
            entry.last_corpus_update_at,
            entry.corpus_size,
            entry.corpus_size_updated_at,
            entry.last_validated_at,
            entry.last_validation_ok.map(|v| if v { 1 } else { 0 }),
            entry.last_validation_message,
            if entry.is_current { 1 } else { 0 }
        ],
    )
    .map_err(|e| sqlite_write_err("Failed to upsert corpus", e))?;
    Ok(())
}

fn mark_corpus_superseded(conn: &Connection, id: &str) -> Result<()> {
    let updated = conn.execute(
        "UPDATE corpora SET is_current = 0, updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
        params![id],
    )?;
    if updated == 0 {
        anyhow::bail!("Corpus '{}' not found in database", id);
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct ValidationResult {
    id: String,
    ok: bool,
    checks: Vec<String>,
    query_probe: String,
    corpus_size: Option<i64>,
    message: String,
}

fn validate_corpus(corpus: &CorpusEntry, full: bool, strict_full: bool) -> ValidationResult {
    let mut checks = Vec::new();
    let mut ok = true;

    if corpus.project_root.exists() {
        checks.push(format!("project_root exists: {}", corpus.project_root.display()));
    } else {
        ok = false;
        checks.push(format!("project_root missing: {}", corpus.project_root.display()));
    }

    if corpus.project_url.is_some() {
        checks.push("project_url present".to_string());
    } else {
        checks.push("project_url missing".to_string());
    }

    let mut query_probe = "not_requested".to_string();
    let mut corpus_size = None;
    let mut message = String::new();

    let run_query_probe = full
        && (corpus.interfaces.iter().any(|i| i == "query")
            || corpus.preferred_backend == "pando"
            || corpus.preferred_backend == "auto");

    if run_query_probe {
        match resolve_effective_backend(corpus) {
            Ok(ref b) if b == "cqp" => match run_cqp_probe(corpus) {
                Ok(size) => {
                    query_probe = "ok".to_string();
                    corpus_size = size;
                    message = "cqp query probe succeeded".to_string();
                }
                Err(err) => {
                    query_probe = "failed".to_string();
                    ok = false;
                    message = format!("cqp query probe failed: {err}");
                }
            },
            Ok(ref b) if b == "pando" => match run_pando_probe(corpus) {
                Ok(size) => {
                    query_probe = "ok".to_string();
                    corpus_size = size;
                    message = "pando query probe succeeded".to_string();
                }
                Err(err) => {
                    query_probe = "failed".to_string();
                    ok = false;
                    message = format!("pando query probe failed: {err}");
                }
            },
            Ok(_) => {
                if let Some(cmd) = corpus
                    .settings
                    .get("validation_command")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    match run_external_probe(cmd) {
                        Ok(_) => {
                            query_probe = "ok".to_string();
                            message = "external validation probe succeeded".to_string();
                        }
                        Err(err) => {
                            query_probe = "failed".to_string();
                            ok = false;
                            message = format!("external validation probe failed: {err}");
                        }
                    }
                } else {
                    query_probe = "unconfigured".to_string();
                    message = "no query probe for resolved backend".to_string();
                    if strict_full {
                        ok = false;
                    }
                }
            }
            Err(err) => {
                query_probe = "failed".to_string();
                ok = false;
                message = format!("backend resolution failed: {err}");
            }
        }
    }

    ValidationResult {
        id: corpus.id.clone(),
        ok,
        checks,
        query_probe,
        corpus_size,
        message,
    }
}

fn run_external_probe(cmd: &str) -> Result<()> {
    let output = ProcessCommand::new("sh")
        .arg("-lc")
        .arg(cmd)
        .output()
        .with_context(|| format!("Failed to run validation command: {cmd}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!("exit {}: {} {}", output.status, stdout.trim(), stderr.trim());
    }
    Ok(())
}

fn run_cqp_probe(corpus: &CorpusEntry) -> Result<Option<i64>> {
    let corpus_name = corpus
        .settings
        .get("corpus_name")
        .and_then(|v| v.as_str())
        .or_else(|| corpus.settings.get("cqp_corpus").and_then(|v| v.as_str()))
        .unwrap_or(&corpus.id)
        .to_string();

    let mut cmd = ProcessCommand::new("cqp");
    cmd.current_dir(resolve_cqp_cwd(corpus));
    if let Some(reg_hint) = corpus
        .settings
        .get("registry_hint")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let reg_path = PathBuf::from(reg_hint);
        let reg_dir = if reg_path.is_file() {
            reg_path
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| reg_path.clone())
        } else {
            reg_path
        };
        cmd.arg("-r").arg(reg_dir);
    }

    let (output, _used_id) = run_cqp_script_with_id_fallback(
        &cmd,
        &corpus_name,
        "Matches = [];\nsize Matches;\n",
    )
    .context("Failed to execute cqp probe script")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!("cqp exit {}: {} {}", output.status, stdout.trim(), stderr.trim());
    }

    let out = String::from_utf8_lossy(&output.stdout);
    let size = parse_last_integer(&out);
    Ok(size)
}

/// Runs `flexicorp-pando` with a cheap query and reads `total` from JSON (same idea as CQP `size Matches`).
/// Override the CQL with `settings.pando_probe_query` (default: `[word=".*"]` = one match per token surface form).
fn run_pando_probe(corpus: &CorpusEntry) -> Result<Option<i64>> {
    let raw = corpus
        .settings
        .get("pando_probe_query")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(r#"[word=".*"]"#);
    let query = normalize_pando_query(raw);
    let binaries = resolve_flexicorp_pando_bins(corpus);
    let index_dir = resolve_pando_index_dir(corpus)?;
    let mut output = None;
    let mut used_binary = String::new();
    let mut spawn_errors: Vec<String> = Vec::new();
    for binary in &binaries {
        let mut cmd = ProcessCommand::new(binary);
        cmd.arg("--index-dir")
            .arg(&index_dir)
            .arg("-q")
            .arg(&query)
            .arg("--offset")
            .arg("0")
            .arg("--limit")
            .arg("1")
            .arg("--max-total")
            .arg("0");
        match cmd.output() {
            Ok(out) => {
                output = Some(out);
                used_binary = binary.clone();
                break;
            }
            Err(err) => {
                spawn_errors.push(format!("{binary}: {err}"));
            }
        }
    }
    let output = if let Some(out) = output {
        out
    } else {
        anyhow::bail!(
            "Failed to execute flexicorp-pando probe. Tried: {}",
            spawn_errors.join(" ; ")
        );
    };
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!(
            "flexicorp-pando probe failed via '{}' (exit {}): stdout='{}' stderr='{}'",
            used_binary,
            exit_code,
            stdout.trim(),
            stderr.trim()
        );
    }
    let payload: Value = serde_json::from_str(stdout.trim())
        .with_context(|| "flexicorp-pando probe output is not valid JSON")?;
    if payload.get("success").and_then(|v| v.as_bool()) == Some(false) {
        anyhow::bail!("flexicorp-pando probe reported success=false: {}", stdout.trim());
    }
    Ok(parse_pando_total_from_json(&payload))
}

fn parse_pando_total_from_json(payload: &Value) -> Option<i64> {
    payload
        .pointer("/done/result/total")
        .and_then(json_total)
        .or_else(|| payload.pointer("/done/result/result/total").and_then(json_total))
}

fn json_total(v: &Value) -> Option<i64> {
    if let Some(i) = v.as_i64() {
        return Some(i);
    }
    if let Some(u) = v.as_u64() {
        return i64::try_from(u).ok();
    }
    None
}

fn parse_last_integer(text: &str) -> Option<i64> {
    for line in text.lines().rev() {
        for token in line.split_whitespace().rev() {
            if let Ok(v) = token.parse::<i64>() {
                return Some(v);
            }
        }
    }
    None
}

fn pando_query_looks_aggregation(query_text: &str) -> bool {
    let q = query_text.to_ascii_lowercase();
    ["freq", "count", "dist", "group", "keyness", "coll", "dcoll"]
        .iter()
        .any(|kw| q.contains(kw))
}

fn update_validation_result(conn: &Connection, id: &str, result: &ValidationResult) -> Result<()> {
    conn.execute(
        r#"
UPDATE corpora
SET
  last_validated_at = CURRENT_TIMESTAMP,
  last_validation_ok = ?2,
  last_validation_message = ?3,
  corpus_size = COALESCE(?4, corpus_size),
  corpus_size_updated_at = CASE WHEN ?4 IS NOT NULL THEN CURRENT_TIMESTAMP ELSE corpus_size_updated_at END,
  updated_at = CURRENT_TIMESTAMP
WHERE id = ?1
"#,
        params![
            id,
            if result.ok { 1 } else { 0 },
            result.message,
            result.corpus_size
        ],
    )
    .with_context(|| format!("Failed to persist validation result for '{id}'"))?;
    Ok(())
}

#[derive(Debug)]
struct PandoExecResult {
    binary: String,
    index_dir: String,
    exit_code: i32,
    payload: Value,
}

#[derive(Debug)]
struct CqpExecResult {
    kind: String,
    binary: String,
    target: String,
    exit_code: i32,
    payload: Value,
}

fn run_pando_query(corpus: &CorpusEntry, query_text: &str, start: u32, size: u32) -> Result<PandoExecResult> {
    let binaries = resolve_flexicorp_pando_bins(corpus);
    let index_dir = resolve_pando_index_dir(corpus)?;
    let mut output = None;
    let mut binary = String::new();
    let mut spawn_errors: Vec<String> = Vec::new();
    for candidate in &binaries {
        let max_total = if pando_query_looks_aggregation(query_text) {
            "1000000"
        } else {
            "10000"
        };
        let mut cmd = ProcessCommand::new(candidate);
        cmd.arg("--index-dir")
            .arg(&index_dir)
            .arg("-q")
            .arg(query_text)
            .arg("--offset")
            .arg(start.to_string())
            .arg("--limit")
            .arg(size.to_string())
            .arg("--max-total")
            .arg(max_total);
        match cmd.output() {
            Ok(out) => {
                output = Some(out);
                binary = candidate.clone();
                break;
            }
            Err(err) => {
                spawn_errors.push(format!("{candidate}: {err}"));
            }
        }
    }
    let output = if let Some(out) = output {
        out
    } else {
        anyhow::bail!(
            "Failed to execute flexicorp-pando query binary. Tried: {}",
            spawn_errors.join(" ; ")
        );
    };
    let exit_code = output.status.code().unwrap_or(-1);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "flexicorp-pando query failed via '{}' (exit {}): stdout='{}' stderr='{}'",
            binary,
            exit_code,
            stdout.trim(),
            stderr.trim()
        );
    }
    let payload_text = String::from_utf8_lossy(&output.stdout);
    let payload: Value = serde_json::from_str(&payload_text)
        .with_context(|| "flexicorp-pando output is not valid JSON")?;

    Ok(PandoExecResult {
        binary,
        index_dir: index_dir.display().to_string(),
        exit_code,
        payload,
    })
}

fn run_cqp_query(corpus: &CorpusEntry, query_text: &str, start: u32, size: u32) -> Result<CqpExecResult> {
    let corpus_name = corpus
        .settings
        .get("corpus_name")
        .and_then(|v| v.as_str())
        .or_else(|| corpus.settings.get("cqp_corpus").and_then(|v| v.as_str()))
        .unwrap_or(&corpus.id)
        .to_string();

    let cqp_bin = corpus
        .settings
        .get("cqp_binary")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("CQP_BINARY").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| "cqp".to_string());

    let (registry_dir, registry_arg) = resolve_cqp_registry(corpus);
    let mut cmd = ProcessCommand::new(&cqp_bin);
    cmd.current_dir(resolve_cqp_cwd(corpus));
    if let Some(reg) = &registry_arg {
        cmd.arg("-r").arg(reg);
    }
    let end = start.saturating_add(size.saturating_sub(1));
    let cqp_script = format!(
        "set PrettyPrint off;\nset Context 5 words;\nset Paging off;\nMatches = {query};\nsize Matches;\ncat Matches {start} {end};\n",
        query = query_text,
        start = start,
        end = end
    );
    let (output, used_id) = run_cqp_script_with_id_fallback(&cmd, &corpus_name, &cqp_script)
        .with_context(|| format!("Failed to execute cqp script with corpus '{}'", corpus_name))?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        anyhow::bail!(
            "cqp query failed (exit {}): stdout='{}' stderr='{}'",
            exit_code,
            stdout.trim(),
            stderr.trim()
        );
    }

    let total = parse_cqp_total(&stdout);
    let lines = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.ends_with('>'))
        .map(str::to_string)
        .collect::<Vec<_>>();

    let payload = json!({
        "success": true,
        "done": {
            "backend": "cqp",
            "operation": "query",
            "errors": [],
            "warnings": [],
            "result": {
                "query": query_text,
                "query_lang": "cwb-cql",
                "corpus_id_used": used_id,
                "result_type": "kwic_text",
                "start": start,
                "requested_size": size,
                "total": total,
                "lines": lines
            }
        },
        "stderr": stderr
    });

    Ok(CqpExecResult {
        kind: "cqp-cli".to_string(),
        binary: cqp_bin,
        target: registry_dir,
        exit_code,
        payload,
    })
}

fn resolve_cqp_cwd(corpus: &CorpusEntry) -> PathBuf {
    let teitok = PathBuf::from(resolve_teitok_project_root(corpus));
    if teitok.is_dir() {
        return teitok;
    }
    if corpus.project_root.is_dir() {
        return corpus.project_root.clone();
    }
    PathBuf::from(".")
}

fn run_cqp_script_with_id_fallback(
    base_cmd: &ProcessCommand,
    corpus_id: &str,
    script: &str,
) -> Result<(std::process::Output, String)> {
    let mut ids = vec![corpus_id.to_string()];
    let upper = corpus_id.to_uppercase();
    if upper != corpus_id {
        ids.push(upper);
    }

    let mut last_err = String::new();
    for id in ids {
        let mut cmd = clone_process_command(base_cmd);
        cmd.arg("-D").arg(&id);
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("Failed to spawn cqp process")?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(script.as_bytes())
                .context("Failed writing script to cqp stdin")?;
        }
        let out = child
            .wait_with_output()
            .context("Failed waiting for cqp script output")?;
        if out.status.success() {
            return Ok((out, id));
        }
        last_err = format!(
            "id={} exit={} stderr={}",
            id,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    anyhow::bail!("all corpus-id attempts failed: {last_err}")
}

fn clone_process_command(cmd: &ProcessCommand) -> ProcessCommand {
    let mut c = ProcessCommand::new(cmd.get_program());
    c.args(cmd.get_args());
    if let Some(dir) = cmd.get_current_dir() {
        c.current_dir(dir);
    }
    c
}

fn run_flexicorp_cqp_query(
    corpus: &CorpusEntry,
    query_text: &str,
    start: u32,
    size: u32,
    query_options: Option<&HttpQueryRequest>,
) -> Result<CqpExecResult> {
    let preferred_python = corpus
        .settings
        .get("python_bin")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("PYTHON_BIN").ok().filter(|s| !s.trim().is_empty()));

    let flexicorp_module = corpus
        .settings
        .get("flexicorp_module")
        .and_then(|v| v.as_str())
        .unwrap_or("flexicorp");

    let project_root = resolve_teitok_project_root(corpus);
    let mut candidates = Vec::<String>::new();
    if let Some(p) = preferred_python {
        candidates.push(p);
    }
    candidates.push("python".to_string());
    candidates.push("python3".to_string());

    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut last_err = String::new();

    for python_bin in candidates {
        let mut cmd = ProcessCommand::new(&python_bin);
        cmd.current_dir(&repo_root)
            .arg("-m")
            .arg(flexicorp_module)
            .arg("query")
            .arg("--backend")
            .arg("cqp")
            .arg("--folder")
            .arg(&project_root)
            .arg("--query")
            .arg(query_text)
            .arg("--start")
            .arg(start.to_string())
            .arg("--limit")
            .arg(size.to_string())
            .arg("--extract-fragments")
            .arg("--api");
        if let Some(qo) = query_options {
            if let Some(w) = qo.window {
                if w > 0 {
                    cmd.arg("--window").arg(w.to_string());
                }
            }
            if let Some(scope) = qo.context_scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                cmd.arg("--context-scope").arg(scope);
            }
            if let Some(fmt) = qo.context_format.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                cmd.arg("--context-format").arg(fmt);
            }
            if qo.flexicorp_fragment_kwic_cpos_span.unwrap_or(false) {
                cmd.arg("--flexicorp-fragment-kwic-cpos-span");
            }
        }

        if let Some(reg_hint) = corpus
            .settings
            .get("registry_hint")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            cmd.arg("--registry").arg(reg_hint);
        }
        if let Some(corpus_name) = corpus
            .settings
            .get("corpus_name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            cmd.arg("--corpus").arg(corpus_name);
        }

        let output = cmd
            .output()
            .with_context(|| format!("Failed to execute flexicorp query using '{python_bin}'"))?;
        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if output.status.success() {
            let payload: Value = serde_json::from_str(&stdout)
                .with_context(|| "flexicorp query output is not valid JSON")?;
            return Ok(CqpExecResult {
                kind: "flexicorp-cli-cqp".to_string(),
                binary: format!("{python_bin} -m {flexicorp_module}"),
                target: project_root,
                exit_code,
                payload,
            });
        }
        last_err = format!(
            "{} (exit {}): stdout='{}' stderr='{}'",
            python_bin,
            exit_code,
            stdout.trim(),
            stderr.trim()
        );
    }

    anyhow::bail!("flexicorp CQP query failed with all python candidates: {last_err}")
}

fn resolve_teitok_project_root(corpus: &CorpusEntry) -> String {
    if let Some(v) = corpus
        .settings
        .get("teitok_project_root")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return v.to_string();
    }
    let p = corpus.project_root.clone();
    if p.file_name().map(|x| x == "cqp").unwrap_or(false) {
        return p
            .parent()
            .map(|x| x.display().to_string())
            .unwrap_or_else(|| p.display().to_string());
    }
    // project_root may point at .../pando (index dir) instead of TEITOK root; normalize to parent.
    if p.file_name().map(|x| x == "pando").unwrap_or(false) {
        return p
            .parent()
            .map(|x| x.display().to_string())
            .unwrap_or_else(|| p.display().to_string());
    }
    p.display().to_string()
}

fn resolve_cqp_registry(corpus: &CorpusEntry) -> (String, Option<PathBuf>) {
    if let Some(reg_hint) = corpus
        .settings
        .get("registry_hint")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let reg_path = PathBuf::from(reg_hint);
        if reg_path.is_file() {
            let dir = reg_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| reg_path.clone());
            return (dir.display().to_string(), Some(dir));
        }
        return (reg_path.display().to_string(), Some(reg_path));
    }
    if let Ok(reg_env) = std::env::var("CWB_REGISTRY") {
        let path = PathBuf::from(reg_env);
        return (path.display().to_string(), Some(path));
    }
    ("<default-cqp-registry>".to_string(), None)
}

fn parse_cqp_total(stdout: &str) -> Option<i64> {
    stdout.lines().find_map(|line| {
        let trimmed = line.trim();
        if trimmed.chars().all(|c| c.is_ascii_digit()) {
            trimmed.parse::<i64>().ok()
        } else {
            None
        }
    })
}

fn resolve_flexicorp_pando_bins(corpus: &CorpusEntry) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push_unique = |value: String| {
        if !out.iter().any(|v| v == &value) {
            out.push(value);
        }
    };
    if let Some(v) = corpus
        .settings
        .get("pando_cli")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        push_unique(v.to_string());
    }
    if let Ok(v) = std::env::var("FLEXICORP_PANDO_BIN") {
        let vv = v.trim();
        if !vv.is_empty() {
            push_unique(vv.to_string());
        }
    }
    let repo_default = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("flexicorp_pando")
        .join("build")
        .join("flexicorp-pando");
    if is_likely_executable_file(&repo_default) {
        push_unique(repo_default.display().to_string());
    }
    // Final fallback: rely on PATH.
    push_unique("flexicorp-pando".to_string());
    out
}

fn is_likely_executable_file(path: &Path) -> bool {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return (meta.permissions().mode() & 0o111) != 0;
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn resolve_pando_index_dir(corpus: &CorpusEntry) -> Result<PathBuf> {
    let from_settings = ["index_path", "index_dir", "pando_index"]
        .iter()
        .find_map(|k| corpus.settings.get(*k).and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);

    let mut path = if let Some(p) = from_settings {
        p
    } else {
        let root = corpus.project_root.clone();
        let project_pando = root.join("pando");
        if project_pando.is_dir() { project_pando } else { root }
    };

    if path.is_file() {
        path = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(path);
    }
    if !path.exists() {
        anyhow::bail!("Pando index path does not exist: {}", path.display());
    }
    Ok(path)
}

fn read_json_input(args: &UpsertJsonArgs) -> Result<String> {
    if let Some(s) = &args.json {
        return Ok(s.clone());
    }
    if let Some(path) = &args.json_file {
        return fs::read_to_string(path)
            .with_context(|| format!("Failed to read JSON file '{}'", path.display()));
    }
    if args.stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("Failed to read JSON from stdin")?;
        return Ok(buf);
    }
    anyhow::bail!("No JSON input provided; use --json, --json-file, or --stdin");
}

fn parse_entries_from_json(payload: &str) -> Result<Vec<CorpusEntry>> {
    let value: serde_json::Value = serde_json::from_str(payload).context("Invalid JSON input")?;
    match value {
        serde_json::Value::Array(_) => {
            let entries: Vec<CorpusEntry> = serde_json::from_value(value)
                .context("JSON array must contain valid corpus objects")?;
            Ok(entries)
        }
        serde_json::Value::Object(_) => {
            let entry: CorpusEntry =
                serde_json::from_value(value).context("JSON object is not a valid corpus entry")?;
            Ok(vec![entry])
        }
        _ => anyhow::bail!("JSON input must be an object or an array of objects"),
    }
}
