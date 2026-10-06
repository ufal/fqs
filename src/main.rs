use std::fs;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use std::collections::HashMap;

use anyhow::{Context, Result};
use axum::extract::{Path as AxumPath, Query as AxumQuery, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum::{extract::Request, response::Response};
use clap::{Args, Parser, Subcommand};
use rusqlite::{Connection, Error as SqliteError, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::time::sleep;

mod activity;
mod admin;
mod enrich;
mod fcs;
mod hot_corpus;
mod limits;
mod pando_lib;
mod scan;
mod services;
mod frontends;

use hot_corpus::{
    add_xidx_fragments, pando_query_body, wrap_pando_server_as_fqs_raw, HotCorpusConfig,
    HotCorpusManager, HotGuard,
};
use limits::{set_engine_tier, Caller, Limits};
use activity::{ActivityLog, UserMode};
use pando_lib::PandoLib;

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
    /// Mint a short-lived admin JWT (`aud=fqs-admin`) for `/admin/api/*`
    AdminToken(AdminTokenArgs),
    /// Frontend modules (KonText, …): what they need
    Frontends(FrontendsArgs),
}

#[derive(Args, Debug)]
struct FrontendsArgs {
    #[command(subcommand)]
    action: FrontendsAction,
}

#[derive(Subcommand, Debug)]
enum FrontendsAction {
    /// The files and folders frontend modules write (for the installer: systemd
    /// ReadWritePaths and permissions), as JSON
    Paths,
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
    /// One-shot: detect languages / features / interfaces from TEITOK+index layout
    Enrich(EnrichArgs),
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
    /// Public URL of the FCS endpoint (e.g. https://lindat.cz/services/test-kontext/fcs);
    /// used for default resource PIDs and layer identifiers
    #[arg(long, env = "FQS_FCS_BASE_URL")]
    fcs_base_url: Option<String>,
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
    /// Activity log (JSON lines): queries and/or warm-corpus opens, closes and
    /// snapshots, for later analysis. Off unless given. Rotates like --log-file.
    #[arg(long, env = "FQS_ACTIVITY_LOG")]
    activity_log: Option<PathBuf>,
    /// What the activity log records: queries, warm (comma-separated) or all
    #[arg(long, env = "FQS_ACTIVITY_EVENTS", default_value = "all")]
    activity_events: String,
    /// Users in the activity log: hash (default; salted, stable within one run),
    /// plain, or none
    #[arg(long, env = "FQS_ACTIVITY_USERS", default_value = "hash")]
    activity_log_users: String,
    /// Salt for hashed users (keeps hashes comparable across restarts)
    #[arg(long, env = "FQS_ACTIVITY_SALT", hide_env_values = true)]
    activity_salt: Option<String>,
    /// Seconds between warm_state snapshots in the activity log (0 = none)
    #[arg(long, default_value_t = 300)]
    activity_state_secs: u64,
    /// Consider sessions stale after N minutes of inactivity
    #[arg(long, default_value_t = 120)]
    session_ttl_minutes: i64,
    /// Human-readable label for this instance (shown in /health; e.g. "LINDAT live corpus query server")
    #[arg(long = "server-name", env = "FQS_SERVER_NAME")]
    server_name: Option<String>,
    /// Max warm Pando corpora (count + idle eviction; no RSS budget for Pando)
    #[arg(long, default_value_t = 8)]
    pando_max_warm: usize,
    /// Idle TTL seconds before an unused warm Pando handle is closed
    #[arg(long, default_value_t = 1800)]
    pando_idle_ttl_secs: u64,
    /// Disable libflexicorp_pando hot path (force cold CLI)
    #[arg(long, default_value_t = false)]
    pando_cli_only: bool,
    /// Restart mode: terminate matching existing `fqs serve --host/--port` process before bind
    #[arg(long, default_value_t = false)]
    restart: bool,
    /// Limits by tier (visitor / user / admin): JSON with "tiers", "default_tier",
    /// "heavy_slots", "pando" engine options (see src/limits.rs)
    #[arg(long, env = "FQS_LIMITS")]
    limits: Option<PathBuf>,
    /// Shared secret of the front-ends' HS256 tokens (Authorization: Bearer, claim
    /// "role"); when set, a request's role comes only from a valid token
    #[arg(long, env = "FQS_SECRET", hide_env_values = true)]
    jwt_secret: Option<String>,
    /// Enable admin HTTP surface (`/admin/`, `/admin/api/*`). Requires `--jwt-secret` /
    /// `FQS_SECRET`. Off by default.
    #[arg(long, default_value_t = false)]
    enable_admin_http: bool,
    /// Directory with admin UI static files (`index.html`, …). Default: resolve
    /// next to the binary, then `./admin`, then crate `admin/` (see admin.rs).
    #[arg(long, env = "FQS_ADMIN_DIR")]
    admin_dir: Option<PathBuf>,
    /// Bind admin HTTP separately (e.g. `127.0.0.1:8790`). When set with
    /// `--enable-admin-http`, `/admin` is served only on this address — not on
    /// the public `--host/--port`. Recommended for LINDAT / reverse-proxy setups.
    #[arg(long, env = "FQS_ADMIN_BIND")]
    admin_bind: Option<String>,
    /// Optional public URL prefix for the admin UI when behind a reverse proxy
    /// that strips a path (e.g. hub `/services/test-kontext/fqsadmin/`). Injected
    /// as `<base href="…">` into `index.html` so `./admin.css` / `./app.js` resolve.
    /// Env: `FQS_ADMIN_BASE_HREF`. Should end with `/`.
    #[arg(long, env = "FQS_ADMIN_BASE_HREF")]
    admin_base_href: Option<String>,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct AdminTokenArgs {
    /// Subject / operator name stored in the `user` claim
    #[arg(long, default_value = "ops")]
    user: String,
    /// Token lifetime (max 4h). Accepts `30m`, `2h`, `3600`, or seconds.
    #[arg(long, default_value = "4h")]
    ttl: String,
    /// Shared HS256 secret (default: `FQS_SECRET` / `--jwt-secret`)
    #[arg(long, env = "FQS_SECRET", hide_env_values = true)]
    jwt_secret: Option<String>,
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

#[derive(Debug, Clone, Deserialize)]
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
    /// Pando: each hit also gets `fragment` (its context as TEITOK-style XML,
    /// `<s id><tok id=… attrs head=…>form</tok>…</s>`), token ids and a
    /// `highlight_map` by query token — for corpora without XML files (TEITOK
    /// sends it when the project has no xmlfiles/). Catalog default:
    /// `settings.pando.synthetic_fragments`.
    #[serde(default)]
    fragment: Option<bool>,
    /// Override FQS backend for this request (pando | cqp) — TEITOK/flexicorp should set from project config
    backend: Option<String>,
    request_role: Option<String>,
    /// Optional caller-generated session id for activity tracking; with `name` /
    /// `from` also the engine's hit-set session (pando: stored results reused)
    session_id: Option<String>,
    /// Store this query's result as hit set `name` in the session (pando)
    name: Option<String>,
    /// Page the stored hit set `from` instead of running the query (pando)
    from: Option<String>,
    /// Lower the request's time limit (the tier's is the cap)
    timeout_ms: Option<u64>,
    /// User id for per-user limits (believed only without --jwt-secret)
    user: Option<String>,
    /// pando: a random sample of this many hits (KonText "random sample"), in corpus order
    sample: Option<u64>,
    /// pando: the hits (or the sample) in a random order (KonText "shuffle")
    shuffle: Option<bool>,
    /// pando: seed for sample / shuffle (the same seed: the same sample / order on every page)
    seed: Option<u32>,
    /// Set by FQS (never from the client): the caller's engine tier
    #[serde(skip_deserializing, default)]
    engine_tier: Option<String>,
    /// Set by FQS: engine options for this corpus (global limits + its settings.limits)
    #[serde(skip_deserializing, default)]
    engine_open_options: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HttpCorporaQuery {
    environment: Option<String>,
    include_noncurrent: Option<bool>,
    request_role: Option<String>,
    /// Filter by browse tag (case-insensitive)
    tag: Option<String>,
    /// Frontend bag filter, e.g. `teitok` (TEITOK-listable corpora).
    frontend: Option<String>,
    /// Comma-separated facets (`lang:cs,feature:spoken`). Prefer repeated `facet=` in the raw query.
    facets: Option<String>,
    /// Substring match on id / label / family_label.
    q: Option<String>,
    /// `browse` returns a public DTO (no project_root / settings dump).
    view: Option<String>,
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

#[derive(Clone)]
struct HttpAppState {
    db_path: PathBuf,
    test_mode: bool,
    host: String,
    port: u16,
    fcs_database: String,
    /// public URL of /fcs (default PIDs, layer ids)
    fcs_base_url: Option<String>,
    server_name: Option<String>,
    request_log_path: PathBuf,
    request_log_max_bytes: u64,
    request_log_keep_files: usize,
    /// Warm Pando handles via libflexicorp_pando (None if the library is missing).
    pando_hcm: Option<Arc<HotCorpusManager>>,
    /// In-memory corpus catalog (avoids open_db + SELECT on every query).
    catalog: Arc<CorpusCatalog>,
    /// Roles → tiers, admission of heavy requests, engine limits.
    limits: Arc<Limits>,
    /// `--activity-log` (None = off).
    activity: Option<Arc<ActivityLog>>,
    /// When set, `/admin/` static UI and `/admin/api/*` are mounted.
    admin_dir: Option<PathBuf>,
    /// True when admin listens on `--admin-bind` (not the public port).
    admin_bind_separate: bool,
    /// Public `<base href>` for admin static assets behind a path-stripping proxy.
    admin_base_href: Option<String>,
    /// Report-only snapshot of process settings (CLI / env / fqs.json). No secrets.
    settings_snapshot: Arc<Value>,
}

/// Hot-path corpus lookup: all rows in memory, from SQLite.
///
/// Kept fresh without a restart when another process (the CLI, TEITOK's
/// `fqs corpora upsert-json`) writes the catalog: `get` / `list` look at the
/// database files' size and mtime (`fqs.db`, `fqs.db-wal`) at most once per
/// second and reload when they changed; a `get` for an unknown id reloads
/// once (throttled) before answering "not found". The 60 s housekeeping reload
/// stays as a safety net.
struct CorpusCatalog {
    inner: RwLock<HashMap<String, CorpusEntry>>,
    db_path: PathBuf,
    /// (last check, database file stamps at the last load, last forced reload)
    fresh: Mutex<CatalogFreshness>,
}

struct CatalogFreshness {
    checked: Instant,
    stamp: Option<DbFileStamp>,
    forced: Option<Instant>,
}

type DbFileStamp = (Option<SystemTime>, u64, Option<SystemTime>, u64);

fn db_file_stamp(db_path: &Path) -> DbFileStamp {
    let st = |p: &Path| match std::fs::metadata(p) {
        Ok(m) => (m.modified().ok(), m.len()),
        Err(_) => (None, 0),
    };
    let (mt, len) = st(db_path);
    let mut wal = db_path.as_os_str().to_owned();
    wal.push("-wal");
    let (wmt, wlen) = st(Path::new(&wal));
    (mt, len, wmt, wlen)
}

impl CorpusCatalog {
    fn load_from_db(db_path: &Path) -> Result<Self> {
        let stamp = db_file_stamp(db_path);
        let conn = open_db(&db_path.to_path_buf())?;
        let rows = list_corpora(&conn, None, true, None)?;
        let mut by_id = HashMap::with_capacity(rows.len());
        for entry in rows {
            by_id.insert(entry.id.clone(), entry);
        }
        Ok(Self {
            inner: RwLock::new(by_id),
            db_path: db_path.to_path_buf(),
            fresh: Mutex::new(CatalogFreshness { checked: Instant::now(), stamp: Some(stamp), forced: None }),
        })
    }

    fn reload(&self, db_path: &Path) -> Result<usize> {
        let stamp = db_file_stamp(db_path);
        let conn = open_db(&db_path.to_path_buf())?;
        let rows = list_corpora(&conn, None, true, None)?;
        let n = rows.len();
        let mut by_id = HashMap::with_capacity(n);
        for entry in rows {
            by_id.insert(entry.id.clone(), entry);
        }
        *self.inner.write().expect("catalog write lock") = by_id;
        if let Ok(mut f) = self.fresh.lock() {
            f.stamp = Some(stamp);
            f.checked = Instant::now();
        }
        Ok(n)
    }

    /// Reload when the database files changed (checked at most once per second),
    /// or — `force` — unconditionally, at most once per second.
    fn refresh(&self, force: bool) {
        let need = {
            let Ok(mut f) = self.fresh.lock() else { return };
            if force {
                if f.forced.map(|t| t.elapsed() < Duration::from_secs(1)).unwrap_or(false) {
                    false
                } else {
                    f.forced = Some(Instant::now());
                    true
                }
            } else if f.checked.elapsed() < Duration::from_secs(1) {
                false
            } else {
                f.checked = Instant::now();
                f.stamp.as_ref() != Some(&db_file_stamp(&self.db_path))
            }
        };
        if need {
            if let Err(err) = self.reload(&self.db_path) {
                eprintln!("[fqs] catalog reload failed: {err}");
            }
        }
    }

    fn get(&self, id: &str) -> Result<CorpusEntry> {
        self.refresh(false);
        if let Some(e) = self.inner.read().expect("catalog read lock").get(id).cloned() {
            return Ok(e);
        }
        // just registered by another process (e.g. TEITOK: upsert, then enqueue)?
        self.refresh(true);
        if let Some(e) = self.inner.read().expect("catalog read lock").get(id).cloned() {
            return Ok(e);
        }
        // The reloads above are throttled (once a second), so a row written a moment ago
        // can still be missing — TEITOK registers a corpus and enqueues its reindex right
        // after: read that one row from the database itself.
        let conn = open_db(&self.db_path.to_path_buf())?;
        if corpus_exists(&conn, id)? {
            let e = get_corpus(&conn, id)?;
            self.inner
                .write()
                .expect("catalog write lock")
                .insert(id.to_string(), e.clone());
            return Ok(e);
        }
        anyhow::bail!("Corpus '{id}' not found in catalog")
    }

    fn list(
        &self,
        environment: Option<&str>,
        include_noncurrent: bool,
        tag: Option<&str>,
    ) -> Vec<CorpusEntry> {
        self.refresh(false);
        let g = self.inner.read().expect("catalog read lock");
        let mut rows: Vec<CorpusEntry> = g
            .values()
            .filter(|c| {
                if let Some(env) = environment {
                    if c.environment != env {
                        return false;
                    }
                }
                if !include_noncurrent && !c.is_current {
                    return false;
                }
                true
            })
            .cloned()
            .collect();
        if let Some(tag) = tag {
            let t = tag.trim();
            if !t.is_empty() {
                rows.retain(|c| c.labels.iter().any(|l| l.eq_ignore_ascii_case(t)));
            }
        }
        rows.sort_by(|a, b| {
            let af = a.family_key.as_deref().unwrap_or(&a.id);
            let bf = b.family_key.as_deref().unwrap_or(&b.id);
            af.cmp(bf)
                .then_with(|| a.label.cmp(&b.label))
                .then_with(|| a.id.cmp(&b.id))
        });
        rows
    }

    fn len(&self) -> usize {
        self.inner.read().expect("catalog read lock").len()
    }
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
    /// Replace existing rows completely. Default: fields the JSON leaves out keep their
    /// stored value, and so do choices made in the FQS admin (settings.fcs.enabled,
    /// settings.kontext corpname / public_url / url) unless the JSON sets them.
    #[arg(long = "replace", default_value_t = false)]
    replace: bool,
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
    /// After validation, run one-shot metadata enrich and write the catalogue
    #[arg(long, default_value_t = false)]
    enrich: bool,
    #[command(flatten)]
    db: DbPathArg,
}

#[derive(Args, Debug)]
struct EnrichArgs {
    /// Enrich only one corpus id
    #[arg(long)]
    id: Option<String>,
    /// Filter by environment label (exact match)
    #[arg(long)]
    environment: Option<String>,
    /// Include superseded corpora
    #[arg(long, default_value_t = false)]
    include_noncurrent: bool,
    /// Dry-run: print detections without writing the catalogue
    #[arg(long, default_value_t = false)]
    dry_run: bool,
    /// Replace the feature labels (feature:…) with what is detected now, instead of only
    /// adding new ones (labels detected earlier that no longer apply go; other labels stay)
    #[arg(long, default_value_t = false)]
    reset_features: bool,
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

/// Where the catalog came from (for `fqs status`, `/health` and the serve log).
static DB_SOURCE: OnceLock<String> = OnceLock::new();

fn db_source() -> String {
    DB_SOURCE.get().cloned().unwrap_or_else(|| "unknown".to_string())
}

/// System-wide settings for every `fqs` (service, shell, TEITOK's PHP):
/// `FQS_CONFIG`, else `/etc/fqs/fqs.json`, e.g. `{"db_path": "/var/lib/fqs/fqs.db"}`.
fn fqs_config_path() -> PathBuf {
    std::env::var("FQS_CONFIG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/fqs/fqs.json"))
}

fn db_path_from_config() -> Option<PathBuf> {
    let s = fs::read_to_string(fqs_config_path()).ok()?;
    let v: Value = serde_json::from_str(&s).ok()?;
    v.get("db_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// The catalog, first found of: `--db`, `FQS_DB_PATH`, `db_path` in the config
/// file, the database of a running `fqs serve` (its fqs-http.json), the platform
/// default (Linux: /var/lib/fqs/fqs.db).
fn resolve_db_path_with_source(arg: &DbPathArg) -> (PathBuf, String) {
    if let Some(path) = &arg.db {
        return (path.clone(), "--db".into());
    }
    if let Ok(path) = std::env::var("FQS_DB_PATH") {
        if !path.trim().is_empty() {
            return (PathBuf::from(path), "FQS_DB_PATH".into());
        }
    }
    if let Some(path) = db_path_from_config() {
        return (path, format!("db_path in {}", fqs_config_path().display()));
    }
    if let Some(path) = discover_db_path_from_runtime_files() {
        return (path, "the running fqs serve (fqs-http.json)".into());
    }
    (default_db_path(), "the default location".into())
}

fn resolve_db_path(arg: &DbPathArg) -> PathBuf {
    let (path, source) = resolve_db_path_with_source(arg);
    if !path.exists() {
        // the usual reason for "which catalog was that?": a new, empty one
        eprintln!(
            "[fqs] note: no catalog at {} (from {source}); creating a new, empty one. \
             Use --db, FQS_DB_PATH or \"db_path\" in {} for an existing one.",
            path.display(),
            fqs_config_path().display()
        );
    }
    let _ = DB_SOURCE.set(source);
    path
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
        Command::AdminToken(args) => handle_admin_token(args)?,
        Command::Frontends(args) => match args.action {
            FrontendsAction::Paths => {
                println!("{}", serde_json::to_string_pretty(&services::frontend_write_paths())?);
            }
        },
    }
    Ok(())
}

fn parse_ttl_secs(s: &str) -> Result<u64> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        anyhow::bail!("empty --ttl");
    }
    if let Some(num) = t.strip_suffix('h') {
        let n: u64 = num.parse().context("invalid --ttl hours")?;
        return Ok(n.saturating_mul(3600));
    }
    if let Some(num) = t.strip_suffix('m') {
        let n: u64 = num.parse().context("invalid --ttl minutes")?;
        return Ok(n.saturating_mul(60));
    }
    if let Some(num) = t.strip_suffix('s') {
        let n: u64 = num.parse().context("invalid --ttl seconds")?;
        return Ok(n);
    }
    Ok(t.parse().context("invalid --ttl (use 4h, 30m, or seconds)")?)
}

fn handle_admin_token(args: AdminTokenArgs) -> Result<()> {
    let secret = args
        .jwt_secret
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("FQS_SECRET").ok().filter(|s| !s.trim().is_empty()))
        .ok_or_else(|| anyhow::anyhow!("--jwt-secret / FQS_SECRET required to mint admin tokens"))?;
    let ttl = parse_ttl_secs(&args.ttl)?;
    let token = admin::mint_admin_token(&secret, &args.user, ttl).map_err(anyhow::Error::msg)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let clamped = ttl.min(admin::ADMIN_TOKEN_MAX_TTL_SECS as u64);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": true,
            "token": token,
            "user": args.user,
            "aud": admin::FQS_ADMIN_AUD,
            "ttl_secs": clamped,
            "exp": now + clamped,
            "usage": "Authorization: Bearer <token>  (or paste into /admin/ UI; stored in sessionStorage only)",
        }))?
    );
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
            let mut entries = parse_entries_from_json(&payload)?;
            let raws = raw_entries_from_json(&payload);
            let mut inserted_ids = Vec::<String>::new();
            let mut updated_ids = Vec::<String>::new();
            for (i, entry) in entries.iter_mut().enumerate() {
                let existed = corpus_exists(&conn, &entry.id)?;
                if existed && !args.replace {
                    let old = get_corpus(&conn, &entry.id)?;
                    let raw = raws.get(i).cloned().unwrap_or(Value::Null);
                    *entry = merge_upsert_entry(&old, entry, &raw)?;
                }
                // a new entry, or one without labels yet (registered from TEITOK before this):
                // its languages and features from the project, as `corpora enrich` finds them
                if !existed || entry.labels.is_empty() {
                    let _ = enrich::enrich_corpus_entry(entry);
                }
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
            let mut enrich_reports = Vec::new();
            for mut corpus in corpora {
                let result = validate_corpus(&corpus, args.full, args.strict_full);
                update_validation_result(&conn, &corpus.id, &result)?;
                if args.enrich {
                    let report = enrich::enrich_corpus_entry(&mut corpus);
                    if report.changed {
                        upsert_corpus(&conn, &corpus)?;
                    }
                    enrich_reports.push(enrich::report_json(&report));
                }
                results.push(result);
            }

            let failures = results.iter().filter(|r| !r.ok).count();
            let summary = json!({
                "ok": failures == 0,
                "operation": "corpora_validate",
                "full": args.full,
                "strict_full": args.strict_full,
                "enrich": args.enrich,
                "validated": results.len(),
                "failures": failures,
                "results": results,
                "enrichment": enrich_reports,
            });
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        CorporaAction::Enrich(args) => {
            let conn = open_db(&resolve_db_path(&args.db))?;
            let corpora = if let Some(id) = args.id.as_deref() {
                vec![get_corpus(&conn, id)?]
            } else {
                list_corpora(&conn, args.environment.as_deref(), args.include_noncurrent, None)?
            };
            let mut reports = Vec::new();
            let mut written = 0usize;
            for mut corpus in corpora {
                let before = corpus.labels.clone();
                if args.reset_features {
                    corpus.labels.retain(|l| !l.to_ascii_lowercase().starts_with("feature:"));
                }
                let mut report = enrich::enrich_corpus_entry(&mut corpus);
                if args.reset_features {
                    let lower = |v: &Vec<String>| v.iter().map(|x| x.to_ascii_lowercase()).collect::<Vec<_>>();
                    let (b, a) = (lower(&before), lower(&corpus.labels));
                    // only what really changed: labels re-detected are not "added"
                    report.added_labels.retain(|l| !b.contains(&l.to_ascii_lowercase()));
                    report.removed_labels = before.iter().filter(|l| !a.contains(&l.to_ascii_lowercase())).cloned().collect();
                    report.changed = report.changed || !report.removed_labels.is_empty();
                }
                if report.changed && !args.dry_run {
                    upsert_corpus(&conn, &corpus)?;
                    written += 1;
                }
                reports.push(enrich::report_json(&report));
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "operation": "corpora_enrich",
                    "dry_run": args.dry_run,
                    "written": written,
                    "reports": reports,
                }))?
            );
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
            "db_source": db_source(),
            "config_file": fqs_config_path(),
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

/// One /query, /run or /fcs request for the activity log (`--activity-log`),
/// filled in as the request goes and written once it is answered.
struct QueryRec {
    started: Instant,
    fields: serde_json::Map<String, Value>,
}

impl QueryRec {
    fn new(endpoint: &str, corpus: &str, query: &str) -> Self {
        let mut fields = serde_json::Map::new();
        fields.insert("endpoint".into(), json!(endpoint));
        fields.insert("corpus".into(), json!(corpus));
        fields.insert("query".into(), json!(query));
        Self { started: Instant::now(), fields }
    }
    fn set(&mut self, k: &str, v: Value) {
        self.fields.insert(k.to_string(), v);
    }
    fn caller(&mut self, c: &Caller) {
        self.set("role", json!(c.role));
        self.set("tier", json!(c.tier));
        self.set("role_verified", json!(c.verified));
        // the raw identity is resolved at write time (hash / plain / none)
        self.set("\u{0}user", json!(c.user));
    }
    /// Time spent waiting for an admission slot.
    fn queued(&mut self, since: Instant) {
        self.set("queued_ms", json!(since.elapsed().as_millis() as u64));
    }
    /// Whether the request had to open its corpus (and how long that took).
    fn opened(&mut self, open_ms: Option<u64>) {
        match open_ms {
            Some(ms) => {
                self.set("warm", json!(false));
                self.set("open_ms", json!(ms));
            }
            None => {
                self.set("warm", json!(true));
            }
        }
    }
    fn finish(&mut self, state: &HttpAppState, r: &Result<Json<Value>, (StatusCode, String)>) {
        match r {
            Ok(Json(v)) => self.finish_status(state, 200, None, Some(v)),
            Err((code, msg)) => self.finish_status(state, code.as_u16(), Some(msg), None),
        }
    }
    fn finish_status(&mut self, state: &HttpAppState, status: u16, error: Option<&str>, payload: Option<&Value>) {
        let Some(a) = state.activity.as_ref().filter(|a| a.logs_queries()) else { return };
        let mut f = std::mem::take(&mut self.fields);
        if let Some(u) = f.remove("\u{0}user") {
            if let Some(u) = u.as_str().and_then(|u| a.user_field(u)) {
                f.insert("user".into(), json!(u));
            }
        }
        f.insert("status".into(), json!(status));
        f.insert("elapsed_ms".into(), json!(self.started.elapsed().as_millis() as u64));
        if let Some(e) = error {
            // engine errors come as JSON: keep the reason fields, not the whole text
            let short: String = match serde_json::from_str::<Value>(e) {
                Ok(v) => {
                    for k in ["denied", "limit", "busy", "timed_out"] {
                        if let Some(x) = v.get(k) {
                            f.insert(k.into(), x.clone());
                        }
                    }
                    v.get("error").and_then(|x| x.as_str()).unwrap_or(e).to_string()
                }
                Err(_) => e.to_string(),
            };
            f.insert("error".into(), json!(short.chars().take(300).collect::<String>()));
        }
        if let Some(v) = payload {
            activity_result_summary(v, &mut f);
        }
        a.query(f);
    }
}

/// Hits / totals / engine path from a /query or /run answer (whatever is there).
fn activity_result_summary(v: &Value, f: &mut serde_json::Map<String, Value>) {
    let res = v.pointer("/raw/done/result").or_else(|| v.pointer("/raw/result"))
        .or_else(|| v.get("result"));
    let Some(res) = res else { return };
    if let Some(h) = res.get("hits").and_then(|x| x.as_array()) {
        f.insert("hits".into(), json!(h.len()));
    }
    if let Some(page) = res.get("page") {
        for (k, out) in [("total", "total"), ("total_exact", "total_exact")] {
            if let Some(x) = page.get(k) {
                f.insert(out.into(), x.clone());
            }
        }
    }
    for k in ["total_matches", "partitions"] {
        if let Some(x) = res.get(k) {
            f.insert(k.into(), x.clone());
        }
    }
    if let Some(p) = res.pointer("/debug/path").or_else(|| res.get("plan_path")) {
        f.insert("path".into(), p.clone());
    }
    if let Some(j) = v.pointer("/raw/job/state").or_else(|| v.pointer("/raw/done/job/state")) {
        f.insert("total_job".into(), j.clone());
    }
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
    let mut response = next.run(req).await;
    if path.starts_with("/admin/api") {
        admin::apply_api_security_headers(response.headers_mut());
    }
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
    pando_hcm: Option<Arc<HotCorpusManager>>,
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
                let exec = run_pando_query(corpus, &pando_query, start, size, query_options, pando_hcm.as_ref())?;
                (
                    effective,
                    exec.kind,
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
    let admin_dir = if args.enable_admin_http {
        let secret_ok = args
            .jwt_secret
            .as_ref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !secret_ok {
            anyhow::bail!(
                "--enable-admin-http requires --jwt-secret / FQS_SECRET (refusing unverified catalog writes)"
            );
        }
        Some(admin::resolve_admin_dir(args.admin_dir.as_deref()))
    } else {
        None
    };
    let db_path = resolve_db_path(&args.db);
    let _ = open_db(&db_path)?;
    run_housekeeping_once(&db_path, args.session_ttl_minutes);
    let (request_log_path, request_log_warning) = prepare_request_log_path(&db_path, args.log_file.clone());

    let limits = Limits::load(args.limits.as_deref(), args.jwt_secret.clone())
        .context("loading --limits")?;
    let activity = match &args.activity_log {
        None => None,
        Some(path) => {
            let users = UserMode::parse(&args.activity_log_users).map_err(anyhow::Error::msg)?;
            let log = ActivityLog::new(path.clone(), &args.activity_events, users,
                                       args.activity_salt.clone(), args.log_max_bytes, args.log_keep_files)
                .map_err(|e| anyhow::anyhow!("--activity-log: {e}"))?;
            eprintln!("[fqs] activity log: {} ({})", path.display(), log.status_json());
            Some(Arc::new(log))
        }
    };
    if limits.has_tiers() {
        eprintln!("[fqs] limits by tier: {}", limits.status_json());
    }
    let pando_hcm = if args.pando_cli_only {
        eprintln!("[fqs] pando hot path disabled (--pando-cli-only); using cold CLI");
        None
    } else {
        match PandoLib::load() {
            Ok(lib) => {
                eprintln!(
                    "[fqs] loaded libflexicorp_pando api={} build={}",
                    lib.api_version(),
                    lib.build_string()
                );
                if limits.has_tiers() && !lib.has_open_opts() {
                    eprintln!(
                        "[fqs] warning: libflexicorp_pando api {} cannot take engine options: tier limits \
                         (timeouts, hit limits, denied features) are not enforced by the engine; \
                         FQS admission still applies",
                        lib.api_version()
                    );
                }
                let hcm = HotCorpusManager::new(
                    lib,
                    HotCorpusConfig {
                        max_warm: args.pando_max_warm.max(1),
                        idle_ttl: Duration::from_secs(args.pando_idle_ttl_secs),
                        open_options: limits.engine_options(),
                    },
                );
                // Idle-TTL eviction runs on its own timer, not inline in every
                // request; a quarter of the TTL (never below 5s) is frequent
                // enough that a corpus is closed reasonably soon after going
                // idle, and infrequent enough that it's never the dominant cost
                // (see hot_corpus.rs) — even at a short TTL in a test setup.
                hcm.spawn_idle_sweeper(Duration::from_secs((args.pando_idle_ttl_secs / 4).max(5)));
                if let Some(a) = &activity {
                    hcm.set_activity(Arc::clone(a));
                    if a.logs_warm() && args.activity_state_secs > 0 {
                        hcm.spawn_state_logger(Duration::from_secs(args.activity_state_secs));
                    }
                }
                Some(hcm)
            }
            Err(err) => {
                eprintln!(
                    "[fqs] libflexicorp_pando unavailable ({err}); pando queries fall back to cold CLI"
                );
                None
            }
        }
    };

    let catalog = Arc::new(
        CorpusCatalog::load_from_db(&db_path)
            .with_context(|| format!("Failed to load corpus catalog from {}", db_path.display()))?,
    );
    eprintln!(
        "[fqs] corpus catalog {} (from {}): {} corpora",
        db_path.display(),
        db_source(),
        catalog.len()
    );

    let admin_bind = match args.admin_bind.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(spec) => {
            if admin_dir.is_none() {
                anyhow::bail!("--admin-bind requires --enable-admin-http");
            }
            Some(
                admin::parse_admin_bind(spec)
                    .map_err(|e| anyhow::anyhow!("--admin-bind: {e}"))?,
            )
        }
        None => None,
    };
    if admin_dir.is_some() && admin_bind.is_none() {
        eprintln!(
            "[fqs] warning: admin HTTP shares the public bind {}:{} — for production use --admin-bind 127.0.0.1:<port>",
            args.host, args.port
        );
    }
    if admin_dir.is_some() && activity.is_none() {
        eprintln!(
            "[fqs] warning: --enable-admin-http without --activity-log; admin writes will not be audit-logged"
        );
    }

    let settings_snapshot = Arc::new(build_settings_snapshot(
        &args,
        &db_path,
        &request_log_path,
        admin_dir.as_deref(),
        admin_bind.as_ref().map(|(h, p)| (h.as_str(), *p)),
        activity.as_deref(),
        args.limits.as_deref(),
        &limits,
    ));
    let state = HttpAppState {
        db_path: db_path.clone(),
        test_mode: args.test,
        host: args.host.clone(),
        port: args.port,
        fcs_database: args.fcs_database.clone(),
        fcs_base_url: args.fcs_base_url.as_deref().map(|u| u.trim_end_matches('/').to_string()),
        server_name: args.server_name.clone(),
        request_log_path: request_log_path.clone(),
        request_log_max_bytes: args.log_max_bytes,
        request_log_keep_files: args.log_keep_files,
        pando_hcm,
        catalog: catalog.clone(),
        limits: limits.clone(),
        activity: activity.clone(),
        admin_dir: admin_dir.clone(),
        admin_bind_separate: admin_bind.is_some(),
        admin_base_href: args
            .admin_base_href
            .as_deref()
            .and_then(|s| admin::normalize_admin_base_href(Some(s))),
        settings_snapshot,
    };
    if let Some(dir) = &admin_dir {
        if let Some((ah, ap)) = &admin_bind {
            eprintln!(
                "[fqs] admin HTTP enabled on {ah}:{ap} (separate bind): UI {} → /admin/ ; API /admin/api/*",
                dir.display()
            );
        } else {
            eprintln!(
                "[fqs] admin HTTP enabled: UI {} → /admin/ ; API /admin/api/* (JWT admin, aud=fqs-admin)",
                dir.display()
            );
        }
    }
    if let Some(a) = &activity {
        a.event("start", activity::fields(vec![
            ("pid", json!(std::process::id())),
            ("version", json!(env!("CARGO_PKG_VERSION"))),
            ("max_warm", json!(args.pando_max_warm.max(1))),
            ("idle_ttl_secs", json!(args.pando_idle_ttl_secs)),
            ("tiers", json!(limits.has_tiers())),
        ]));
    }
    let hk_db_path = db_path.clone();
    let hk_session_ttl = args.session_ttl_minutes;
    let hk_catalog = catalog.clone();
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(60)).await;
            run_housekeeping_once(&hk_db_path, hk_session_ttl);
            // Pick up CLI corpora add/delete/upsert without restarting serve.
            if let Err(err) = hk_catalog.reload(&hk_db_path) {
                eprintln!("[fqs] catalog reload failed: {err}");
            }
        }
    });
    let mut public_app = Router::new()
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
        .route("/backends", get(http_backends))
        .route("/info", get(http_pando_info))
        .route("/context", get(http_pando_context))
        .route("/status", get(http_pando_status))
        .route("/run", post(http_pando_run))
        .route("/session", get(http_pando_session_info).post(http_pando_session_create))
        .route("/session/close", post(http_pando_session_close))
        .route("/sessions", get(http_pando_sessions));

    let admin_routes = if admin_dir.is_some() {
        Some(
            Router::new()
                .route(
                    "/admin/api/corpora",
                    get(http_admin_list_corpora).put(http_admin_upsert_corpora),
                )
                .route(
                    "/admin/api/corpora/{id}",
                    get(http_admin_get_corpus).delete(http_admin_delete_corpus),
                )
                .route(
                    "/admin/api/corpora/{id}/validate",
                    post(http_admin_validate_corpus),
                )
                .route("/admin/api/health", get(http_admin_health))
                .route("/admin/api/settings", get(http_admin_settings))
                .route("/admin/api/activity", get(http_admin_activity))
                .route("/admin/api/reindex/jobs", get(http_admin_reindex_jobs))
                .route("/admin/api/scan", get(http_admin_scan).post(http_admin_scan_post))
                .route("/admin/api/backends", get(http_admin_backends))
                .route("/admin/api/frontends", get(http_admin_frontends))
                .route("/admin/api/coverage", get(http_admin_coverage))
                .route(
                    "/admin/api/frontends/{id}/restart",
                    post(http_admin_frontend_restart),
                )
                .route(
                    "/admin/api/frontends/{id}/publish",
                    post(http_admin_frontend_publish),
                )
                .route(
                    "/admin/api/frontends/{id}/corplist/append",
                    post(http_admin_frontend_publish),
                )
                .route(
                    "/admin/api/corpora/{id}/fcs-enabled",
                    post(http_admin_set_fcs_enabled),
                )
                .route("/admin/api/self", get(http_admin_self))
                .route("/admin/api/self/restart", post(http_admin_self_restart))
                .route("/admin", get(|| async { Redirect::temporary("/admin/") }))
                .route("/admin/", get(http_admin_index))
                .route("/admin/{*path}", get(http_admin_static)),
        )
    } else {
        None
    };

    let admin_app = match (admin_routes, admin_bind.is_some()) {
        (Some(ar), true) => Some(
            ar.layer(middleware::from_fn_with_state(state.clone(), http_log_middleware))
                .with_state(state.clone()),
        ),
        (Some(ar), false) => {
            public_app = public_app.merge(ar);
            None
        }
        (None, _) => None,
    };

    let public_app = public_app
        .layer(middleware::from_fn_with_state(state.clone(), http_log_middleware))
        .with_state(state.clone());

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
    let admin_address = admin_bind
        .as_ref()
        .map(|(h, p)| format!("{h}:{p}"));
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": true,
            "operation": "serve",
            "address": addr,
            "admin_address": admin_address,
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
            "enable_admin_http": admin_dir.is_some(),
            "admin_dir": admin_dir,
            "log_warning": request_log_warning,
            "http_runtime_warning": http_runtime_warning
        }))?
    );

    if let (Some(admin_app), Some((ah, ap))) = (admin_app, admin_bind) {
        let admin_addr = format!("{ah}:{ap}");
        let admin_listener = tokio::net::TcpListener::bind(&admin_addr)
            .await
            .with_context(|| format!("Failed to bind admin HTTP at {admin_addr}"))?;
        tokio::select! {
            r = axum::serve(listener, public_app) => {
                r.context("HTTP server failed")?;
            }
            r = axum::serve(admin_listener, admin_app) => {
                r.context("admin HTTP server failed")?;
            }
        }
    } else {
        axum::serve(listener, public_app)
            .await
            .context("HTTP server failed")?;
    }
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
        .arg(&requested_csv)
        // So flexicorp foreground --staging can stage+swap under a stable id
        // (FQS job id) instead of a random fg-* id, and so job logs correlate.
        .arg("--options")
        .arg(format!("reindex_job_id={job_id}"));
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
        "{python_bin} -m {flexicorp_module} reindex --api --backend {backend} --folder {project_root} --teitok yes --verbose --staging --reindex-backends {requested_csv} --options reindex_job_id={job_id}{options_preview}"
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
    let mut api_ok: Option<bool> = None;
    let mut api_errors: Vec<String> = Vec::new();
    if let Ok(parsed) = serde_json::from_str::<Value>(&stdout) {
        if let Some(s) = parsed.get("success").and_then(|v| v.as_bool()) {
            api_ok = Some(s);
        }
        if let Some(arr) = parsed
            .pointer("/done/errors")
            .and_then(|v| v.as_array())
            .or_else(|| parsed.get("errors").and_then(|v| v.as_array()))
        {
            for e in arr {
                if let Some(s) = e.as_str() {
                    let t = s.trim();
                    if !t.is_empty() {
                        api_errors.push(t.to_string());
                    }
                }
            }
        }
        result["raw"] = parsed;
    }

    // flexicorp --api historically exited 0 even when success:false; prefer JSON.
    let job_ok = status.success() && api_ok.unwrap_or(true);
    let conn2 = open_db(&db_path.to_path_buf())?;
    if job_ok {
        let _ = mark_reindex_job_finished(
            &conn2,
            job_id,
            true,
            Some("completed"),
            None,
            Some(&result),
        )?;
    } else {
        let err_text = if !api_errors.is_empty() {
            truncate_for_job_log(&api_errors.join("; "), 12000)
        } else if api_ok == Some(false) {
            truncate_for_job_log(
                if !stdout.trim().is_empty() {
                    stdout.trim()
                } else {
                    "flexicorp reindex reported success=false"
                },
                12000,
            )
        } else if !stderr.trim().is_empty() {
            truncate_for_job_log(stderr.trim(), 12000)
        } else if !stdout.trim().is_empty() {
            truncate_for_job_log(stdout.trim(), 12000)
        } else {
            format!("reindex process exited with status {}", exit_code)
        };
        if let Some(progress) = result.get_mut("progress") {
            *progress = json!({
                "phase": "failed",
                "percent": last_progress_percent.unwrap_or(0),
                "message": "failed"
            });
        }
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
    // Public: deliberately minimal (no db path, slots, engine options, admin flag).
    Json(json!({
        "ok": true,
        "service": "fqs",
        "version": env!("CARGO_PKG_VERSION"),
        "server_name": state.server_name,
    }))
}

fn detailed_health_body(state: &HttpAppState) -> Value {
    let mut body = json!({
        "ok": true,
        "service": "fqs",
        "mode": "http",
        "version": env!("CARGO_PKG_VERSION"),
        "server_name": state.server_name,
        "db_path": state.db_path.to_string_lossy(),
        "db_source": db_source(),
        "catalog_corpora": state.catalog.len(),
        "admin_http": state.admin_dir.is_some(),
        "admin_bind_separate": state.admin_bind_separate,
    });
    if let Some(a) = &state.activity {
        body["activity_log"] = a.status_json();
    }
    if let Some(hcm) = &state.pando_hcm {
        body["pando"] = hcm.status_json();
    } else {
        body["pando"] = json!({"available": false});
    }
    body["limits"] = state.limits.status_json();
    body
}

fn setting_item(key: &str, value: Value, source: &str, change: &str) -> Value {
    json!({
        "key": key,
        "value": value,
        "source": source,
        "change": change,
    })
}

fn build_settings_snapshot(
    args: &ServeArgs,
    db_path: &Path,
    request_log_path: &Path,
    admin_dir: Option<&Path>,
    admin_bind: Option<(&str, u16)>,
    activity: Option<&activity::ActivityLog>,
    limits_path: Option<&Path>,
    limits: &Limits,
) -> Value {
    let cfg_path = fqs_config_path();
    let cfg_exists = cfg_path.is_file();
    let admin_bind_s = admin_bind.map(|(h, p)| format!("{h}:{p}"));

    let mut sections = Vec::new();

    sections.push(json!({
        "id": "identity",
        "title": "Identity",
        "mutable": false,
        "items": [
            setting_item("version", json!(env!("CARGO_PKG_VERSION")), "build", "rebuild / redeploy FQS"),
            setting_item(
                "server_name",
                json!(args.server_name),
                "--server-name / FQS_SERVER_NAME",
                "fqs serve --server-name '…'  (restart)"
            ),
            setting_item(
                "test_mode",
                json!(args.test),
                "--test",
                "omit --test for production (restart)"
            ),
        ],
    }));

    sections.push(json!({
        "id": "bind",
        "title": "HTTP bind",
        "mutable": false,
        "items": [
            setting_item(
                "public_address",
                json!(format!("{}:{}", args.host, args.port)),
                "--host / --port",
                "fqs serve --host … --port …  (restart)"
            ),
            setting_item(
                "admin_http",
                json!(admin_dir.is_some()),
                "--enable-admin-http",
                "fqs serve --enable-admin-http  (requires FQS_SECRET; restart)"
            ),
            setting_item(
                "admin_bind",
                json!(admin_bind_s),
                "--admin-bind / FQS_ADMIN_BIND",
                "fqs serve --admin-bind 127.0.0.1:8790  (restart; recommended for production)"
            ),
            setting_item(
                "admin_dir",
                json!(admin_dir.map(|p| p.to_string_lossy().to_string())),
                "--admin-dir / FQS_ADMIN_DIR",
                "fqs serve --admin-dir /path/to/admin  (restart)"
            ),
        ],
    }));

    sections.push(json!({
        "id": "catalog",
        "title": "Catalog database",
        "mutable": false,
        "items": [
            setting_item(
                "db_path",
                json!(db_path.to_string_lossy()),
                &db_source(),
                "fqs serve --db …  or FQS_DB_PATH  or db_path in fqs.json  (restart)"
            ),
            setting_item(
                "fqs_config",
                json!(cfg_path.to_string_lossy()),
                "FQS_CONFIG / /etc/fqs/fqs.json",
                "edit fqs.json (db_path, scan_roots, frontends, fqs.restart); restart after path changes"
            ),
            setting_item(
                "fqs_config_present",
                json!(cfg_exists),
                "filesystem",
                "create /etc/fqs/fqs.json (or set FQS_CONFIG)"
            ),
        ],
    }));

    sections.push(json!({
        "id": "auth",
        "title": "Auth / role trust",
        "mutable": false,
        "note": "The JWT secret itself is never shown.",
        "items": [
            setting_item(
                "role_trust",
                json!(if limits.has_jwt() { "jwt" } else { "unverified" }),
                "--jwt-secret / FQS_SECRET",
                "export FQS_SECRET='…'  (required for --enable-admin-http; restart)"
            ),
            setting_item(
                "admin_token",
                json!("fqs admin-token --user ops --ttl 4h"),
                "CLI",
                "mint a short-lived JWT with aud=fqs-admin (max 4h)"
            ),
        ],
    }));

    sections.push(json!({
        "id": "limits",
        "title": "Global limits file",
        "mutable": false,
        "note": "Editing the global limits file from the GUI is out of scope. Per-corpus overrides live in Corpora → settings.limits.",
        "items": [
            setting_item(
                "limits_path",
                json!(limits_path.map(|p| p.to_string_lossy().to_string())),
                "--limits / FQS_LIMITS",
                "fqs serve --limits /path/to/limits.json  (restart)"
            ),
            setting_item(
                "tiers_loaded",
                json!(limits.has_tiers()),
                "limits file",
                "add a \"tiers\" object to the limits JSON"
            ),
        ],
        "config": limits.file_config().clone(),
        "live": limits.status_json(),
    }));

    sections.push(json!({
        "id": "pando_warm",
        "title": "Pando warm pool",
        "mutable": false,
        "items": [
            setting_item(
                "pando_max_warm",
                json!(args.pando_max_warm),
                "--pando-max-warm",
                "fqs serve --pando-max-warm N  (restart)"
            ),
            setting_item(
                "pando_idle_ttl_secs",
                json!(args.pando_idle_ttl_secs),
                "--pando-idle-ttl-secs",
                "fqs serve --pando-idle-ttl-secs N  (restart)"
            ),
            setting_item(
                "pando_cli_only",
                json!(args.pando_cli_only),
                "--pando-cli-only",
                "omit --pando-cli-only to use libflexicorp_pando when available (restart)"
            ),
            setting_item(
                "session_ttl_minutes",
                json!(args.session_ttl_minutes),
                "--session-ttl-minutes",
                "fqs serve --session-ttl-minutes N  (restart)"
            ),
        ],
    }));

    sections.push(json!({
        "id": "fcs",
        "title": "FCS / SRU",
        "mutable": false,
        "items": [
            setting_item(
                "fcs_database",
                json!(args.fcs_database),
                "--fcs-database",
                "fqs serve --fcs-database NAME  (restart)"
            ),
            setting_item(
                "fcs_base_url",
                json!(args.fcs_base_url),
                "--fcs-base-url / FQS_FCS_BASE_URL",
                "fqs serve --fcs-base-url https://…/fcs  (restart)"
            ),
        ],
    }));

    let activity_items = match activity {
        Some(a) => {
            let st = a.status_json();
            vec![
                setting_item(
                    "activity_log",
                    st.get("path").cloned().unwrap_or(Value::Null),
                    "--activity-log / FQS_ACTIVITY_LOG",
                    "fqs serve --activity-log /var/log/fqs/activity.jsonl  (restart; required for admin write audit)"
                ),
                setting_item(
                    "activity_events",
                    json!(format!(
                        "queries={} warm={}",
                        st.get("queries").and_then(Value::as_bool).unwrap_or(false),
                        st.get("warm").and_then(Value::as_bool).unwrap_or(false)
                    )),
                    "--activity-events / FQS_ACTIVITY_EVENTS",
                    "fqs serve --activity-events queries,warm|all  (restart)"
                ),
                setting_item(
                    "activity_users",
                    st.get("users").cloned().unwrap_or(Value::Null),
                    "--activity-log-users / FQS_ACTIVITY_USERS",
                    "fqs serve --activity-log-users hash|plain|none  (restart)"
                ),
                setting_item(
                    "activity_state_secs",
                    json!(args.activity_state_secs),
                    "--activity-state-secs",
                    "fqs serve --activity-state-secs N  (0 = no warm_state snapshots; restart)"
                ),
            ]
        }
        None => vec![setting_item(
            "activity_log",
            Value::Null,
            "--activity-log / FQS_ACTIVITY_LOG",
            "fqs serve --activity-log /var/log/fqs/activity.jsonl  (restart; recommended with admin HTTP)",
        )],
    };
    let mut logging_items = vec![
        setting_item(
            "request_log",
            json!(request_log_path.to_string_lossy()),
            "--log-file",
            "fqs serve --log-file /path/to/fqs.log  (restart)",
        ),
        setting_item(
            "log_max_bytes",
            json!(args.log_max_bytes),
            "--log-max-bytes",
            "fqs serve --log-max-bytes N  (restart)",
        ),
        setting_item(
            "log_keep_files",
            json!(args.log_keep_files),
            "--log-keep-files",
            "fqs serve --log-keep-files N  (restart)",
        ),
    ];
    logging_items.extend(activity_items);
    sections.push(json!({
        "id": "logging",
        "title": "Logging",
        "mutable": false,
        "items": logging_items,
    }));

    sections.push(json!({
        "id": "scan",
        "title": "Scan allowlist",
        "mutable": false,
        "note": "Request scan roots are intersected with this allowlist. Set FQS_SCAN_ROOTS or fqs.json scan_roots for a tight allowlist.",
        "change": "export FQS_SCAN_ROOTS='/srv/teitok:/data/corpora'  or add \"scan_roots\" to fqs.json (restart not required for next scan if only env/file changed — prefer restart so ops stay consistent)",
        "items": [],
    }));

    sections.push(json!({
        "id": "corpus_settings",
        "title": "Per-corpus settings",
        "mutable": "corpora_tab",
        "note": "Catalog fields (policy, preferred_backend, labels, …) are edited on the Corpora tab via PUT /admin/api/corpora — not here. Corpus settings/capabilities JSON is shown read-only in the admin UI.",
        "change": "Admin UI → Corpora, or: fqs corpora upsert-json …",
        "items": [],
    }));

    json!({
        "ok": true,
        "mutable": false,
        "policy": "Process and global settings are report-only in the admin UI. Change them via CLI flags, environment variables, or fqs.json, then restart FQS. Per-corpus form fields are edited on the Corpora tab; settings/capabilities JSON there is read-only (enrich, registration, or CLI upsert).",
        "fqs_config_path": cfg_path.to_string_lossy(),
        "sections": sections,
    })
}

fn enrich_settings_report(state: &HttpAppState) -> Value {
    let mut body = state.settings_snapshot.as_ref().clone();
    let catalog_roots: Vec<PathBuf> = state
        .catalog
        .list(None, true, None)
        .into_iter()
        .map(|c| c.project_root)
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    let allow = scan::configured_scan_allowlist(&catalog_roots);
    let scan_roots: Vec<Value> = allow
        .iter()
        .map(|r| json!({"path": r.path, "source": r.source}))
        .collect();
    if let Some(sections) = body.get_mut("sections").and_then(Value::as_array_mut) {
        for sec in sections.iter_mut() {
            match sec.get("id").and_then(Value::as_str) {
                Some("scan") => {
                    sec["items"] = json!([setting_item(
                        "allowlist",
                        json!(scan_roots),
                        "FQS_SCAN_ROOTS / fqs.json scan_roots / catalog parents / defaults",
                        "export FQS_SCAN_ROOTS='…' or set scan_roots in fqs.json"
                    )]);
                }
                Some("limits") => {
                    sec["live"] = state.limits.status_json();
                    sec["config"] = state.limits.file_config().clone();
                }
                _ => {}
            }
        }
    }
    body["scan_allowlist"] = json!(scan_roots);
    body["limits_live"] = state.limits.status_json();
    body
}

// --- Admin HTTP (v0) --------------------------------------------------------

fn to_admin_err(err: anyhow::Error) -> admin::AdminError {
    admin::AdminError::msg(StatusCode::BAD_REQUEST, err.to_string())
}

fn admin_audit(state: &HttpAppState, event: &str, caller: &Caller, mut fields: Map<String, Value>) {
    fields.insert("by".into(), json!(caller.user));
    fields.insert("role".into(), json!(caller.role));
    if let Some(a) = &state.activity {
        a.event(event, fields);
    }
}

fn corpus_entry_hash(entry: &CorpusEntry) -> String {
    let s = serde_json::to_string(entry).unwrap_or_default();
    let d = Sha256::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}
#[derive(Debug, Deserialize)]
struct AdminDeleteQuery {
    /// Soft-delete / deactivate (`is_current=0`). Default and only HTTP action.
    supersede: Option<String>,
    /// Refused: hard delete is CLI-only (`fqs corpora delete`).
    hard: Option<String>,
    /// Refused legacy hard-delete flag.
    force: Option<String>,
}

fn query_flag_true(v: Option<&str>) -> bool {
    matches!(
        v.map(str::trim).map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

#[derive(Debug, Deserialize)]
struct AdminValidateBody {
    full: Option<bool>,
    strict_full: Option<bool>,
}

async fn http_admin_index(State(state): State<HttpAppState>) -> Response {
    let Some(dir) = state.admin_dir.as_ref() else {
        return admin::AdminError::msg(StatusCode::NOT_FOUND, "admin HTTP disabled").into_response();
    };
    admin::static_response_with_base(dir, "index.html", state.admin_base_href.as_deref())
}

async fn http_admin_static(
    State(state): State<HttpAppState>,
    AxumPath(path): AxumPath<String>,
) -> Response {
    let Some(dir) = state.admin_dir.as_ref() else {
        return admin::AdminError::msg(StatusCode::NOT_FOUND, "admin HTTP disabled").into_response();
    };
    if path == "api" || path.starts_with("api/") {
        return admin::AdminError::msg(StatusCode::NOT_FOUND, "unknown admin API route").into_response();
    }
    let rel = if path.is_empty() { "index.html".to_string() } else { path };
    admin::static_response_with_base(dir, &rel, state.admin_base_href.as_deref())
}

async fn http_admin_health(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
) -> admin::AdminResult<Json<Value>> {
    let _ = admin::require_admin(&state.limits, &headers)?;
    let mut body = detailed_health_body(&state);
    let server_name = state.server_name.clone();
    let self_report = tokio::task::spawn_blocking(move || {
        services::probe_fqs_self(server_name.as_deref())
    })
    .await
    .map_err(|e| admin::AdminError::msg(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    body["fqs"] = self_report;
    Ok(Json(body))
}

async fn http_admin_settings(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
) -> admin::AdminResult<Json<Value>> {
    let _ = admin::require_admin(&state.limits, &headers)?;
    Ok(Json(enrich_settings_report(&state)))
}

#[derive(Debug, Deserialize)]
struct AdminActivityQuery {
    /// Max events to return (1–500, default 100).
    limit: Option<usize>,
    /// `interesting` (default, skip warm_state), `all`, `query`, `warm`, `admin`, or event name.
    event: Option<String>,
    /// Filter by corpus / corpus_id (case-insensitive).
    corpus: Option<String>,
}

async fn http_admin_activity(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumQuery(q): AxumQuery<AdminActivityQuery>,
) -> admin::AdminResult<Json<Value>> {
    let _ = admin::require_admin(&state.limits, &headers)?;
    let Some(log) = state.activity.as_ref() else {
        return Ok(Json(json!({
            "ok": true,
            "enabled": false,
            "hint": "Activity log is off. Start with --activity-log FILE (or FQS_ACTIVITY_LOG), then restart FQS.",
            "summary": null,
            "events": [],
        })));
    };
    let limit = q.limit.unwrap_or(100);
    let event = q.event.as_deref().unwrap_or("interesting");
    let corpus = q.corpus.as_deref();
    // Read on a blocking thread so a large tail does not stall the async runtime.
    let log = Arc::clone(log);
    let event = event.to_string();
    let corpus = corpus.map(str::to_string);
    let report = tokio::task::spawn_blocking(move || {
        log.admin_overview(limit, &event, corpus.as_deref(), 2 * 1024 * 1024)
    })
    .await
    .map_err(|e| admin::AdminError::msg(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(report))
}

async fn http_admin_self(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
) -> admin::AdminResult<Json<Value>> {
    let _ = admin::require_admin(&state.limits, &headers)?;
    let server_name = state.server_name.clone();
    let report = tokio::task::spawn_blocking(move || {
        services::probe_fqs_self(server_name.as_deref())
    })
    .await
    .map_err(|e| admin::AdminError::msg(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(report))
}

async fn http_admin_self_restart(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
) -> admin::AdminResult<Json<Value>> {
    let caller = admin::require_admin(&state.limits, &headers)?;
    let result = tokio::task::spawn_blocking(services::restart_fqs)
        .await
        .map_err(|e| admin::AdminError::msg(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match result {
        Ok(body) => {
            let ok = body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            admin_audit(
                &state,
                "admin_fqs_restart",
                &caller,
                activity::fields(vec![("ok", json!(ok))]),
            );
            Ok(Json(json!({
                "ok": ok,
                "operation": "admin_fqs_restart",
                "by": caller.user,
                "result": body,
            })))
        }
        Err(msg) => Err(admin::AdminError::msg(StatusCode::BAD_REQUEST, msg)),
    }
}

async fn http_admin_list_corpora(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
) -> admin::AdminResult<Json<Value>> {
    let caller = admin::require_admin(&state.limits, &headers)?;
    // Admin UI should see CLI/TEITOK upserts immediately (not wait for mtime poll).
    state.catalog.refresh(true);
    let corpora = state.catalog.list(None, true, None);
    Ok(Json(json!({
        "ok": true,
        "role": caller.role,
        "user": caller.user,
        "count": corpora.len(),
        "corpora": corpora,
    })))
}

async fn http_admin_get_corpus(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> admin::AdminResult<Json<Value>> {
    let _ = admin::require_admin(&state.limits, &headers)?;
    let corpus = state.catalog.get(&id).map_err(|e| {
        admin::AdminError::msg(StatusCode::NOT_FOUND, e.to_string())
    })?;
    Ok(Json(json!({"ok": true, "corpus": corpus})))
}

async fn http_admin_upsert_corpora(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    body: String,
) -> admin::AdminResult<Json<Value>> {
    let caller = admin::require_admin(&state.limits, &headers)?;
    let entries = parse_entries_from_json(&body).map_err(to_admin_err)?;
    if entries.is_empty() {
        return Err(admin::AdminError::msg(
            StatusCode::BAD_REQUEST,
            "no corpus entries in body",
        ));
    }
    let conn = open_db(&state.db_path).map_err(to_admin_err)?;
    let mut inserted = Vec::new();
    let mut updated = Vec::new();
    let mut audits = Vec::new();
    for entry in &entries {
        let before = get_corpus(&conn, &entry.id).ok();
        let before_hash = before.as_ref().map(corpus_entry_hash);
        let existed = before.is_some();
        upsert_corpus(&conn, entry).map_err(to_admin_err)?;
        let after_hash = corpus_entry_hash(entry);
        if existed {
            updated.push(entry.id.clone());
        } else {
            inserted.push(entry.id.clone());
        }
        audits.push(json!({
            "id": entry.id,
            "action": if existed { "update" } else { "insert" },
            "before_hash": before_hash,
            "after_hash": after_hash,
        }));
    }
    let n = state.catalog.reload(&state.db_path).map_err(to_admin_err)?;
    for a in &audits {
        admin_audit(
            &state,
            "admin_corpora_upsert",
            &caller,
            activity::fields(vec![
                ("corpus_id", a.get("id").cloned().unwrap_or(Value::Null)),
                ("action", a.get("action").cloned().unwrap_or(Value::Null)),
                ("before_hash", a.get("before_hash").cloned().unwrap_or(Value::Null)),
                ("after_hash", a.get("after_hash").cloned().unwrap_or(Value::Null)),
            ]),
        );
    }
    Ok(Json(json!({
        "ok": true,
        "operation": "admin_corpora_upsert",
        "by": caller.user,
        "inserted": inserted,
        "updated": updated,
        "catalog_corpora": n,
    })))
}

async fn http_admin_delete_corpus(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    AxumQuery(q): AxumQuery<AdminDeleteQuery>,
) -> admin::AdminResult<Json<Value>> {
    let caller = admin::require_admin(&state.limits, &headers)?;
    if query_flag_true(q.hard.as_deref()) || query_flag_true(q.force.as_deref()) {
        return Err(admin::AdminError::msg(
            StatusCode::BAD_REQUEST,
            "hard delete is not available via the admin API/GUI; use `fqs corpora delete --id …` (or deactivate with DELETE /admin/api/corpora/{id}?supersede=1)",
        ));
    }
    let _ = q.supersede; // optional flag; DELETE always deactivates
    let conn = open_db(&state.db_path).map_err(to_admin_err)?;
    let before = get_corpus(&conn, &id).map_err(|e| {
        admin::AdminError::msg(StatusCode::NOT_FOUND, e.to_string())
    })?;
    let before_hash = corpus_entry_hash(&before);
    if !before.is_current {
        return Ok(Json(json!({
            "ok": true,
            "operation": "admin_corpora_supersede",
            "by": caller.user,
            "id": id,
            "detail": {"superseded": true, "already": true},
            "catalog_corpora": state.catalog.len(),
        })));
    }
    mark_corpus_superseded(&conn, &id).map_err(to_admin_err)?;
    let catalog_n = state.catalog.reload(&state.db_path).map_err(to_admin_err)?;
    admin_audit(
        &state,
        "admin_corpora_supersede",
        &caller,
        activity::fields(vec![
            ("corpus_id", json!(id)),
            ("before_hash", json!(before_hash)),
        ]),
    );
    Ok(Json(json!({
        "ok": true,
        "operation": "admin_corpora_supersede",
        "by": caller.user,
        "id": id,
        "detail": {"superseded": true},
        "catalog_corpora": catalog_n,
    })))
}

async fn http_admin_validate_corpus(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    body: Option<Json<AdminValidateBody>>,
) -> admin::AdminResult<Json<Value>> {
    let caller = admin::require_admin(&state.limits, &headers)?;
    let full = body.as_ref().and_then(|b| b.full).unwrap_or(false);
    let strict_full = body.as_ref().and_then(|b| b.strict_full).unwrap_or(false);
    let corpus = state.catalog.get(&id).map_err(|e| {
        admin::AdminError::msg(StatusCode::NOT_FOUND, e.to_string())
    })?;
    let db_path = state.db_path.clone();
    let catalog = state.catalog.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = validate_corpus(&corpus, full, strict_full);
        let conn = open_db(&db_path)?;
        update_validation_result(&conn, &corpus.id, &result)?;
        catalog.reload(&db_path)?;
        Ok::<_, anyhow::Error>(result)
    })
    .await
    .map_err(|e| admin::AdminError::msg(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(to_admin_err)?;
    admin_audit(
        &state,
        "admin_corpora_validate",
        &caller,
        activity::fields(vec![
            ("corpus_id", json!(id)),
            ("full", json!(full)),
            ("strict_full", json!(strict_full)),
            ("ok", json!(result.ok)),
        ]),
    );
    Ok(Json(json!({
        "ok": true,
        "operation": "admin_corpora_validate",
        "by": caller.user,
        "full": full,
        "strict_full": strict_full,
        "result": result,
    })))
}

async fn http_admin_reindex_jobs(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumQuery(params): AxumQuery<HttpReindexJobsQuery>,
) -> admin::AdminResult<Json<Value>> {
    let _ = admin::require_admin(&state.limits, &headers)?;
    http_reindex_jobs(State(state), AxumQuery(params))
        .await
        .map_err(|(status, msg)| admin::AdminError::msg(status, msg))
}

#[derive(Debug, Deserialize)]
struct AdminScanQuery {
    /// Colon-separated roots; each must lie under the configured allowlist
    roots: Option<String>,
    max_depth: Option<u32>,
    max_candidates: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct AdminScanBody {
    roots: Option<Vec<String>>,
    max_depth: Option<u32>,
    max_candidates: Option<usize>,
}

async fn http_admin_scan(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumQuery(q): AxumQuery<AdminScanQuery>,
) -> admin::AdminResult<Json<Value>> {
    let roots = q.roots.as_ref().map(|s| {
        s.split(|c| c == ':' || c == ';')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>()
    });
    run_admin_scan(state, headers, roots, q.max_depth, q.max_candidates).await
}

async fn http_admin_scan_post(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    body: Option<Json<AdminScanBody>>,
) -> admin::AdminResult<Json<Value>> {
    let (roots, max_depth, max_candidates) = match body {
        Some(Json(b)) => (b.roots, b.max_depth, b.max_candidates),
        None => (None, None, None),
    };
    run_admin_scan(state, headers, roots, max_depth, max_candidates).await
}

async fn run_admin_scan(
    state: HttpAppState,
    headers: HeaderMap,
    roots: Option<Vec<String>>,
    max_depth: Option<u32>,
    max_candidates: Option<usize>,
) -> admin::AdminResult<Json<Value>> {
    let caller = admin::require_admin(&state.limits, &headers)?;
    let catalog_rows: Vec<(String, PathBuf, Value)> = state
        .catalog
        .list(None, true, None)
        .into_iter()
        .map(|c| (c.id, c.project_root, c.settings))
        .collect();
    let catalog_roots: Vec<PathBuf> = catalog_rows.iter().map(|(_, r, _)| r.clone()).collect();
    let fingerprints = scan::catalog_fingerprints(&catalog_rows);
    let allow = scan::configured_scan_allowlist(&catalog_roots);
    let requested = roots.clone().unwrap_or_default();
    let rejected: Vec<String> = requested
        .iter()
        .filter(|r| !scan::root_allowed_under(r, &allow))
        .cloned()
        .collect();
    let explicit = roots.filter(|v| !v.is_empty());
    if explicit.is_some() && !rejected.is_empty() && explicit.as_ref().map(|v| {
        v.iter().all(|r| rejected.iter().any(|x| x == r))
    }).unwrap_or(false) {
        return Err(admin::AdminError::new(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "error": "all requested scan roots lie outside the configured allowlist (FQS_SCAN_ROOTS / fqs.json scan_roots, else catalog/defaults)",
                "rejected_roots": rejected,
                "allowlist": allow,
            }),
        ));
    }
    let scan_roots = scan::resolve_scan_roots(explicit.as_deref(), &catalog_roots);
    let max_depth = max_depth.unwrap_or(4).min(8);
    let max_candidates = max_candidates.unwrap_or(500).clamp(1, 5000);
    let report = tokio::task::spawn_blocking(move || {
        scan::scan_filesystem(&scan_roots, &fingerprints, max_depth, max_candidates)
    })
    .await
    .map_err(|e| admin::AdminError::msg(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let new_n = report.candidates.iter().filter(|c| c.status == "new").count();
    let alias_n = report.candidates.iter().filter(|c| c.status == "alias").count();
    let reg_n = report.candidates.iter().filter(|c| c.status == "registered").count();
    let suggestions: Vec<Value> = report
        .candidates
        .iter()
        .filter(|c| c.status == "new")
        .map(scan::candidate_to_entry_json)
        .collect();

    admin_audit(
        &state,
        "admin_scan",
        &caller,
        activity::fields(vec![
            ("roots", json!(report.roots.iter().map(|r| &r.path).collect::<Vec<_>>())),
            ("rejected_roots", json!(rejected)),
            ("candidates", json!(report.candidates.len())),
            ("new", json!(new_n)),
        ]),
    );

    Ok(Json(json!({
        "ok": true,
        "operation": "admin_scan",
        "by": caller.user,
        "summary": {
            "roots": report.roots.len(),
            "candidates": report.candidates.len(),
            "new": new_n,
            "alias": alias_n,
            "registered": reg_n,
            "truncated": report.truncated,
            "skipped_outside": report.skipped_outside,
            "rejected_roots": rejected.len(),
        },
        "rejected_roots": rejected,
        "allowlist": allow,
        "roots": report.roots,
        "candidates": report.candidates,
        "register_suggestions": suggestions,
    })))
}

async fn http_admin_backends(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
) -> admin::AdminResult<Json<Value>> {
    let _ = admin::require_admin(&state.limits, &headers)?;
    let pando = state
        .pando_hcm
        .as_ref()
        .map(|h| h.status_json())
        .unwrap_or(json!({"available": false}));
    let report = tokio::task::spawn_blocking(move || services::probe_backends(pando))
        .await
        .map_err(|e| admin::AdminError::msg(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(report))
}

async fn http_admin_frontends(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
) -> admin::AdminResult<Json<Value>> {
    let _ = admin::require_admin(&state.limits, &headers)?;
    let mut hints = Vec::new();
    let mut corpus_index = serde_json::Map::new();
    for c in state.catalog.list(None, true, None) {
        let project_root = c.project_root.to_string_lossy();
        hints.extend(services::hints_from_catalog_row(
            &c.id,
            c.project_url.as_deref(),
            c.interface_preference.as_deref(),
            &c.settings,
            &c.source_kind,
            c.supports_xml,
            Some(project_root.as_ref()),
            &c.capabilities,
        ));
        let fcs_enabled = c
            .settings
            .get("fcs")
            .and_then(|f| f.get("enabled"))
            .and_then(|v| v.as_bool())
            .or_else(|| {
                c.capabilities
                    .get("fcs")
                    .and_then(|f| f.get("enabled"))
                    .and_then(|v| v.as_bool())
            });
        corpus_index.insert(
            c.id.clone(),
            json!({
                "id": c.id,
                "label": c.label,
                "preferred_backend": c.preferred_backend,
                "project_url": c.project_url,
                "project_root": if project_root.is_empty() { Value::Null } else { json!(project_root.as_ref()) },
                "http_policy_mode": c.http_policy_mode,
                "interface_preference": c.interface_preference,
                "source_kind": c.source_kind,
                "supports_xml": c.supports_xml,
                "fcs_enabled": fcs_enabled,
            }),
        );
    }
    let report = tokio::task::spawn_blocking(move || {
        let mut report = services::probe_frontends(&hints);
        services::attach_frontend_corpus_details(&mut report, &corpus_index);
        report
    })
    .await
    .map_err(|e| admin::AdminError::msg(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(report))
}

async fn http_admin_coverage(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
) -> admin::AdminResult<Json<Value>> {
    let _ = admin::require_admin(&state.limits, &headers)?;
    let corpora = state.catalog.list(None, true, None);
    let owned: Vec<(
        String,
        String,
        String,
        bool,
        String,
        Option<String>,
        String,
        bool,
        Option<String>,
        Option<String>,
        Value,
        Value,
    )> = corpora
        .into_iter()
        .map(|c| {
            (
                c.id,
                c.label,
                c.preferred_backend,
                c.is_current,
                c.http_policy_mode,
                c.interface_preference,
                c.source_kind,
                c.supports_xml,
                Some(c.project_root.to_string_lossy().into_owned())
                    .filter(|s| !s.is_empty()),
                c.project_url,
                c.settings,
                c.capabilities,
            )
        })
        .collect();
    let report = tokio::task::spawn_blocking(move || {
        let rows: Vec<services::CoverageCorpus<'_>> = owned
            .iter()
            .map(
                |(
                    id,
                    label,
                    preferred_backend,
                    is_current,
                    http_policy_mode,
                    interface_preference,
                    source_kind,
                    supports_xml,
                    project_root,
                    project_url,
                    settings,
                    capabilities,
                )| services::CoverageCorpus {
                    id,
                    label,
                    preferred_backend,
                    is_current: *is_current,
                    http_policy_mode,
                    interface_preference: interface_preference.as_deref(),
                    source_kind,
                    supports_xml: *supports_xml,
                    project_root: project_root.as_deref(),
                    project_url: project_url.as_deref(),
                    settings,
                    capabilities,
                },
            )
            .collect();
        services::compute_frontend_coverage(&rows)
    })
    .await
    .map_err(|e| admin::AdminError::msg(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(report))
}

#[derive(Debug, Deserialize)]
struct FrontendPublishBody {
    /// FQS catalogue id of the corpus to publish
    #[serde(default)]
    corpus_id: Option<String>,
    /// the frontend's name for it (KonText: corplist ident); default: the module's suggestion
    #[serde(default)]
    name: Option<String>,
    /// older admin UI builds: KonText ident
    #[serde(default)]
    ident: Option<String>,
    #[serde(default)]
    sentence_struct: Option<String>,
    /// module-specific options
    #[serde(default)]
    options: Option<Value>,
}

/// Publish a catalogue corpus to a frontend (POST /admin/api/frontends/{id}/publish; the
/// older /corplist/append is the same call). The frontend's module writes its files; FQS
/// keeps the catalogue in step (the frontend's name for the corpus, its public URL).
async fn http_admin_frontend_publish(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumPath(frontend_id): AxumPath<String>,
    Json(body): Json<FrontendPublishBody>,
) -> admin::AdminResult<Json<Value>> {
    let caller = admin::require_admin(&state.limits, &headers)?;
    let corpus_id = body
        .corpus_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| admin::AdminError::msg(StatusCode::BAD_REQUEST, "corpus_id is required".to_string()))?;
    // the catalogue row first: a corpus that does not exist must not change a frontend
    let entry = state.catalog.get(&corpus_id).map_err(|e| {
        admin::AdminError::msg(StatusCode::NOT_FOUND, format!("corpus '{corpus_id}' not found: {e:#}"))
    })?;
    // where the frontend reaches this FQS, unless fqs.json says (frontends[].fqs_url)
    let host = match state.host.as_str() {
        "0.0.0.0" | "::" | "" => "127.0.0.1".to_string(),
        h if h.contains(':') => format!("[{h}]"),
        h => h.to_string(),
    };
    let fqs_url = format!("http://{host}:{}", state.port);
    let name = body.name.clone().or(body.ident.clone()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let mut options = body.options.clone().unwrap_or_else(|| json!({}));
    if let (Some(ss), Some(o)) = (body.sentence_struct.as_deref(), options.as_object_mut()) {
        o.entry("sentence_struct").or_insert(json!(ss));
    }
    let index_dir = resolve_pando_index_dir(&entry).ok();
    let language = entry
        .settings
        .get("languages")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| entry.labels.iter().find_map(|l| l.strip_prefix("lang:").map(str::to_string)));
    let description = entry.settings.get("description").and_then(Value::as_str).map(str::to_string);
    let project_root = entry.project_root.to_string_lossy();
    let teitok = services::corpus_is_teitok_listable(
        entry.interface_preference.as_deref(),
        &entry.source_kind,
        entry.supports_xml,
        Some(project_root.as_ref()),
        entry.project_url.as_deref(),
        &entry.settings,
        &entry.capabilities,
    ) || entry.source_kind.to_ascii_lowercase().contains("teitok");
    let project_url = entry.project_url.clone();
    let fid = frontend_id.clone();
    let (cid, label) = (corpus_id.clone(), entry.label.clone());
    let result = tokio::task::spawn_blocking(move || {
        services::publish_to_frontend(
            &fid,
            &frontends::PublishRequest {
                name: name.as_deref(),
                corpus_id: &cid,
                label: &label,
                description: description.as_deref(),
                language: language.as_deref(),
                index_dir,
                fqs_url: &fqs_url,
                options: &options,
                teitok,
                project_url: project_url.as_deref(),
            },
        )
    })
    .await
    .map_err(|e| admin::AdminError::msg(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut body_out = result.map_err(|e| admin::AdminError::msg(StatusCode::BAD_REQUEST, e))?;

    // catalogue: merge what the module reports (e.g. settings.kontext.corpname)
    if let Some(patch) = body_out.get("catalog_settings").and_then(Value::as_object).cloned() {
        let mut entry = entry;
        if !entry.settings.is_object() {
            entry.settings = json!({});
        }
        let settings = entry.settings.as_object_mut().expect("object");
        for (block, vals) in patch {
            let b = settings.entry(block).or_insert_with(|| json!({}));
            if !b.is_object() {
                *b = json!({});
            }
            if let (Some(bo), Some(vo)) = (b.as_object_mut(), vals.as_object()) {
                for (k, v) in vo {
                    bo.insert(k.clone(), v.clone());
                }
            }
        }
        let conn = open_db(&state.db_path).map_err(to_admin_err)?;
        upsert_corpus(&conn, &entry).map_err(to_admin_err)?;
        state.catalog.refresh(true);
        if let Some(o) = body_out.as_object_mut() {
            o.insert("catalog_synced".into(), json!(true));
            o.insert("catalog_kontext_synced".into(), json!(true));
            o.insert("corpus_id".into(), json!(corpus_id));
        }
    }

    admin_audit(
        &state,
        "admin_frontend_publish",
        &caller,
        activity::fields(vec![
            ("frontend_id", json!(frontend_id)),
            ("corpus_id", json!(corpus_id)),
            ("name", body_out.get("name").cloned().unwrap_or(Value::Null)),
            ("steps", body_out.get("steps").cloned().unwrap_or(Value::Null)),
        ]),
    );
    Ok(Json(body_out))
}

#[derive(Debug, Deserialize)]
struct FcsEnabledBody {
    enabled: bool,
}

async fn http_admin_set_fcs_enabled(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<FcsEnabledBody>,
) -> admin::AdminResult<Json<Value>> {
    let caller = admin::require_admin(&state.limits, &headers)?;
    let mut entry = state.catalog.get(&id).map_err(|e| {
        admin::AdminError::msg(StatusCode::NOT_FOUND, format!("corpus '{id}' not found: {e:#}"))
    })?;
    let mut fcs = entry
        .settings
        .get("fcs")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Some(obj) = fcs.as_object_mut() {
        obj.insert("enabled".into(), json!(body.enabled));
    } else {
        fcs = json!({ "enabled": body.enabled });
    }
    if let Some(obj) = entry.settings.as_object_mut() {
        obj.insert("fcs".into(), fcs);
    } else {
        entry.settings = json!({ "fcs": { "enabled": body.enabled } });
    }
    let conn = open_db(&state.db_path).map_err(to_admin_err)?;
    upsert_corpus(&conn, &entry).map_err(to_admin_err)?;
    state.catalog.refresh(true);
    admin_audit(
        &state,
        "admin_fcs_enabled",
        &caller,
        activity::fields(vec![
            ("corpus_id", json!(id)),
            ("enabled", json!(body.enabled)),
        ]),
    );
    Ok(Json(json!({
        "ok": true,
        "id": id,
        "enabled": body.enabled,
    })))
}

async fn http_admin_frontend_restart(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> admin::AdminResult<Json<Value>> {
    let caller = admin::require_admin(&state.limits, &headers)?;
    let id_owned = id.clone();
    let result = tokio::task::spawn_blocking(move || services::restart_frontend(&id_owned))
        .await
        .map_err(|e| admin::AdminError::msg(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match result {
        Ok(body) => {
            let ok = body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            admin_audit(
                &state,
                "admin_frontend_restart",
                &caller,
                activity::fields(vec![
                    ("frontend_id", json!(id)),
                    ("ok", json!(ok)),
                ]),
            );
            Ok(Json(json!({
                "ok": ok,
                "operation": "admin_frontend_restart",
                "by": caller.user,
                "frontend_id": id,
                "result": body,
            })))
        }
        Err(msg) => Err(admin::AdminError::msg(StatusCode::BAD_REQUEST, msg)),
    }
}

async fn http_root_with_state(State(state): State<HttpAppState>) -> Json<Value> {
    let mut routes = vec![
            json!({"method":"GET", "path":"/", "description":"Route index"}),
            json!({"method":"GET", "path":"/health", "description":"Health check"}),
            json!({"method":"GET", "path":"/corpora", "description":"List corpora (request_role, environment, tag, frontend=teitok, facet=/facets=, q=, view=browse)"}),
            json!({"method":"GET", "path":"/labels", "description":"Browse labels + facet groups/counts (frontend=teitok, facet=…)"}),
            json!({"method":"GET", "path":"/fcs", "description":"FCS/SRU-style endpoint"}),
            json!({"method":"GET", "path":"/reindex/jobs", "description":"List reindex queue (status/corpus/limit)"}),
            json!({"method":"POST", "path":"/reindex/jobs", "description":"Enqueue reindex job (admin role)"}),
            json!({"method":"GET", "path":"/reindex/history", "description":"Reindex history log (corpus/limit)"}),
            json!({"method":"POST", "path":"/reindex/workers/heartbeat", "description":"Worker heartbeat + capacity"}),
            json!({"method":"POST", "path":"/reindex/jobs/mark-started", "description":"Worker callback: mark started"}),
            json!({"method":"POST", "path":"/reindex/jobs/mark-finished", "description":"Worker callback: mark finished"}),
            json!({"method":"POST", "path":"/query", "description":"Run query (JSON body: corpus, query, language?, start?, size?, request_role?)"}),
            json!({"method":"GET", "path":"/backends", "description":"Warm backend / HCM status"}),
            json!({"method":"GET", "path":"/info", "description":"Pando corpus info (?corpus=)"}),
            json!({"method":"GET", "path":"/context", "description":"Pando KWIC context (?corpus=&pos=&left=&right=)"}),
            json!({"method":"GET", "path":"/status", "description":"Pando async total job (?corpus=&job=)"}),
            json!({"method":"POST", "path":"/run", "description":"Pando CQL program (JSON: corpus, cql|query, session_id?, …)"}),
            json!({"method":"POST", "path":"/session", "description":"Pando hit-set session (JSON: corpus, session_id?, ttl_s?); /query name / from and /run use it"}),
            json!({"method":"GET", "path":"/session", "description":"Pando session's hit sets (?corpus=&session_id=)"}),
            json!({"method":"POST", "path":"/session/close", "description":"Close a pando session (JSON: corpus, session_id)"}),
            json!({"method":"GET", "path":"/sessions", "description":"Open pando sessions (?corpus=)"}),
    ];
    if state.admin_dir.is_some() && !state.admin_bind_separate {
        routes.extend([
            json!({"method":"GET", "path":"/admin/", "description":"Admin UI (JWT aud=fqs-admin)"}),
            json!({"method":"GET", "path":"/admin/api/corpora", "description":"Admin: list all corpora"}),
            json!({"method":"PUT", "path":"/admin/api/corpora", "description":"Admin: upsert corpus JSON"}),
            json!({"method":"GET", "path":"/admin/api/corpora/{id}", "description":"Admin: get corpus"}),
            json!({"method":"DELETE", "path":"/admin/api/corpora/{id}?supersede=1", "description":"Admin: deactivate/supersede corpus (hard delete is CLI-only)"}),
            json!({"method":"POST", "path":"/admin/api/corpora/{id}/validate", "description":"Admin: validate corpus"}),
            json!({"method":"GET", "path":"/admin/api/health", "description":"Admin: detailed health"}),
            json!({"method":"GET", "path":"/admin/api/settings", "description":"Admin: report-only effective process settings (CLI/env/fqs.json)"}),
            json!({"method":"GET", "path":"/admin/api/activity", "description":"Admin: activity-log overview (summary + recent events)"}),
            json!({"method":"GET", "path":"/admin/api/reindex/jobs", "description":"Admin: reindex jobs"}),
            json!({"method":"GET|POST", "path":"/admin/api/scan", "description":"Admin: scan disk (allowlisted roots)"}),
            json!({"method":"GET", "path":"/admin/api/backends", "description":"Admin: installed query backends + versions"}),
            json!({"method":"GET", "path":"/admin/api/frontends", "description":"Admin: known frontends + health"}),
            json!({"method":"GET", "path":"/admin/api/coverage", "description":"Admin: frontend coverage gaps (KonText corplist, FCS undecided)"}),
            json!({"method":"POST", "path":"/admin/api/frontends/{id}/publish", "description":"Admin: publish a catalogue corpus to a frontend through its frontend module (KonText: corplist, pando_corpora.json, Manatee registry shell)"}),
            json!({"method":"POST", "path":"/admin/api/corpora/{id}/fcs-enabled", "description":"Admin: set settings.fcs.enabled true/false"}),
            json!({"method":"POST", "path":"/admin/api/frontends/{id}/restart", "description":"Admin: restart configured frontend (fqs.json only)"}),
            json!({"method":"GET", "path":"/admin/api/self", "description":"Admin: this FQS version + update check"}),
            json!({"method":"POST", "path":"/admin/api/self/restart", "description":"Admin: restart FQS (fqs.restart in fqs.json only)"}),
        ]);
    }
    Json(json!({
        "ok": true,
        "service": "fqs",
        "version": env!("CARGO_PKG_VERSION"),
        "server_name": state.server_name,
        "admin_http_on_this_bind": state.admin_dir.is_some() && !state.admin_bind_separate,
        "routes": routes,
    }))
}

async fn http_list_corpora(
    State(state): State<HttpAppState>,
    AxumQuery(params): AxumQuery<HttpCorporaQuery>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, (StatusCode, String)> {
    let role = normalize_role(params.request_role.as_deref());
    let include_noncurrent = params.include_noncurrent.unwrap_or(false);
    let requested_facets =
        services::parse_facet_params(raw.as_deref(), params.facets.as_deref());
    let corpora = state.catalog.list(
        params.environment.as_deref(),
        include_noncurrent,
        params.tag.as_deref(),
    );
    let frontend = params
        .frontend
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    let q = params
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    let browse = params
        .view
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("browse"))
        .unwrap_or(false);

    let filtered: Vec<CorpusEntry> = corpora
        .into_iter()
        .filter(|c| is_http_access_allowed(c, &role) && is_http_operation_allowed(c, "catalog"))
        .filter(|c| {
            if frontend.as_deref() != Some("teitok") {
                return true;
            }
            let root = c.project_root.to_string_lossy();
            services::corpus_is_teitok_listable(
                c.interface_preference.as_deref(),
                &c.source_kind,
                c.supports_xml,
                Some(root.as_ref()),
                c.project_url.as_deref(),
                &c.settings,
                &c.capabilities,
            )
        })
        .filter(|c| {
            if requested_facets.is_empty() {
                return true;
            }
            let facets =
                services::browse_facets_for_corpus(&c.labels, &c.settings, &c.capabilities);
            services::corpus_matches_requested_facets(&facets, &requested_facets)
        })
        .filter(|c| {
            let Some(q) = q.as_deref() else {
                return true;
            };
            let id = c.id.to_ascii_lowercase();
            let label = c.label.to_ascii_lowercase();
            let fam = c
                .family_label
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase();
            id.contains(q) || label.contains(q) || fam.contains(q)
        })
        .collect();

    let rows: Value = if browse {
        Value::Array(
            filtered
                .iter()
                .map(browse_corpus_dto)
                .collect(),
        )
    } else {
        serde_json::to_value(&filtered).unwrap_or(Value::Array(vec![]))
    };

    Ok(Json(json!({
        "ok": true,
        "role": role,
        "frontend": frontend,
        "view": if browse { "browse" } else { "full" },
        "facets_applied": requested_facets.iter().map(|(g,v)| format!("{g}:{v}")).collect::<Vec<_>>(),
        "corpora": rows,
    })))
}

async fn http_browse_labels(
    State(state): State<HttpAppState>,
    AxumQuery(params): AxumQuery<HttpCorporaQuery>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, (StatusCode, String)> {
    let role = normalize_role(params.request_role.as_deref());
    let include_noncurrent = params.include_noncurrent.unwrap_or(false);
    let requested_facets =
        services::parse_facet_params(raw.as_deref(), params.facets.as_deref());
    let corpora = state.catalog.list(
        params.environment.as_deref(),
        include_noncurrent,
        None,
    );
    let frontend = params
        .frontend
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());

    let filtered: Vec<CorpusEntry> = corpora
        .into_iter()
        .filter(|c| is_http_access_allowed(c, &role) && is_http_operation_allowed(c, "catalog"))
        .filter(|c| {
            if frontend.as_deref() != Some("teitok") {
                return true;
            }
            let root = c.project_root.to_string_lossy();
            services::corpus_is_teitok_listable(
                c.interface_preference.as_deref(),
                &c.source_kind,
                c.supports_xml,
                Some(root.as_ref()),
                c.project_url.as_deref(),
                &c.settings,
                &c.capabilities,
            )
        })
        .collect();

    // Facet dictionary for the frontend set; optionally narrowed by already-selected facets.
    let facet_maps: Vec<_> = filtered
        .iter()
        .filter(|c| {
            if requested_facets.is_empty() {
                return true;
            }
            let facets =
                services::browse_facets_for_corpus(&c.labels, &c.settings, &c.capabilities);
            services::corpus_matches_requested_facets(&facets, &requested_facets)
        })
        .map(|c| services::browse_facets_for_corpus(&c.labels, &c.settings, &c.capabilities))
        .collect();

    let dict = services::facet_dictionary(&facet_maps);
    let mut legacy_labels: Vec<String> = filtered
        .iter()
        .flat_map(|c| c.labels.iter().cloned())
        .collect();
    legacy_labels.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));
    legacy_labels.dedup_by(|a, b| a.eq_ignore_ascii_case(b));

    let mut labels = dict
        .get("labels")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    for l in legacy_labels {
        if !labels.iter().any(|x| x.eq_ignore_ascii_case(&l)) {
            labels.push(l);
        }
    }
    labels.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));

    Ok(Json(json!({
        "ok": true,
        "role": role,
        "frontend": frontend,
        "labels": labels,
        "facets": dict.get("facets").cloned().unwrap_or(json!({})),
    })))
}

fn browse_corpus_dto(c: &CorpusEntry) -> Value {
    let facets = services::browse_facets_for_corpus(&c.labels, &c.settings, &c.capabilities);
    let description = c
        .settings
        .get("description")
        .or_else(|| c.capabilities.get("description"))
        .or_else(|| {
            c.capabilities
                .get("browse")
                .and_then(|b| b.get("description"))
        })
        .and_then(Value::as_str)
        .map(str::to_string);
    let root = c.project_root.to_string_lossy();
    let teitok_listable = services::corpus_is_teitok_listable(
        c.interface_preference.as_deref(),
        &c.source_kind,
        c.supports_xml,
        Some(root.as_ref()),
        c.project_url.as_deref(),
        &c.settings,
        &c.capabilities,
    );
    // Where the corpus can be opened (TEITOK project, KonText, CQPweb, Korp, FCS ...),
    // so a corpus list can show corpora from every interface on the server.
    let frontends: Vec<Value> = services::hints_from_catalog_row(
        &c.id,
        c.project_url.as_deref(),
        c.interface_preference.as_deref(),
        &c.settings,
        &c.source_kind,
        c.supports_xml,
        Some(root.as_ref()),
        &c.capabilities,
    )
    .into_iter()
    // only frontends with an address can be linked to
    .filter(|h| h.url.is_some())
    // a frontend switched off in the corpus settings (kontext.enabled = false, …) is not offered
    .filter(|h| {
        let block = match h.kind.as_str() {
            "cqpweb" => c.settings.get("cqpweb").or_else(|| c.settings.get("cqp_web")),
            k => c.settings.get(k),
        };
        block
            .and_then(|b| b.get("enabled"))
            .and_then(Value::as_bool)
            != Some(false)
    })
    .map(|h| {
        json!({
            "kind": h.kind,
            "label": h.label,
            "url": h.url,
            "corpus": h.corpus_alias.unwrap_or(h.corpus_id),
        })
    })
    .collect();
    // Public browse DTO: no project_root (local path). Clients use project_url + teitok_listable.
    json!({
        "frontends": frontends,
        "id": c.id,
        "label": c.label,
        "family_key": c.family_key,
        "family_label": c.family_label,
        "project_url": c.project_url,
        "preferred_backend": c.preferred_backend,
        "source_kind": c.source_kind,
        "interface_preference": c.interface_preference,
        "supports_xml": c.supports_xml,
        "teitok_listable": teitok_listable,
        "labels": c.labels,
        "facets": facets,
        "corpus_size": c.corpus_size,
        "description": description,
    })
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
    let _ = state.catalog.get(&req.corpus).map_err(to_http_err)?;
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
    headers: HeaderMap,
    Json(req): Json<HttpQueryRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut rec = QueryRec::new("/query", &req.corpus, &req.query);
    if state.activity.as_ref().is_some_and(|a| a.logs_queries()) {
        rec.set("start", json!(req.start.unwrap_or(0)));
        rec.set("size", json!(req.size.unwrap_or(25)));
        for (k, v) in [("language", &req.language), ("backend", &req.backend), ("session_id", &req.session_id),
                       ("name", &req.name), ("from", &req.from)] {
            if let Some(v) = v {
                rec.set(k, json!(v));
            }
        }
    }
    let r = http_query_inner(state.clone(), headers, req, &mut rec).await;
    rec.finish(&state, &r);
    r
}

async fn http_query_inner(
    state: HttpAppState,
    headers: HeaderMap,
    mut req: HttpQueryRequest,
    rec: &mut QueryRec,
) -> Result<Json<Value>, (StatusCode, String)> {
    let started = Instant::now();
    let corpus = state.catalog.get(&req.corpus).map_err(to_http_err)?;
    let caller = state.limits.caller(
        &headers,
        req.request_role.as_deref(),
        req.user.as_deref(),
        req.session_id.as_deref(),
        &header_client_ip(&headers),
    );
    let role = caller.role.clone();
    rec.caller(&caller);
    req.engine_tier = (!caller.tier.is_empty() && state.limits.has_tiers()).then(|| caller.tier.clone());
    req.engine_open_options = Some(state.limits.engine_options_for(&corpus.settings));
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
    let corpus_for_exec = corpus.clone();
    let corpus_id = req.corpus.clone();
    let query_text = req.query.clone();
    let backend_override = req.backend.clone();
    let query_opts = req.clone();
    let hcm = state.pando_hcm.clone();
    // heavy requests (a program) take an admission slot, held until the engine is done
    let permit = if limits::query_is_heavy(&req.query) && req.from.is_none() {
        let t0 = Instant::now();
        let p = state.limits.admit(&caller).await;
        rec.queued(t0);
        Some(p?)
    } else {
        None
    };
    // a search that runs as a child process (CQP, cold pando) waits for a process slot
    let process_permit = if spawns_process(&state, &corpus, req.backend.as_deref()) {
        let t0 = Instant::now();
        let p = state.limits.admit_process(&caller.tier).await;
        rec.set("process_queued_ms", json!(t0.elapsed().as_millis() as u64));
        Some(p?)
    } else {
        None
    };
    let joined = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _process_permit = process_permit;
        hot_corpus::track_open(|| execute_query(
            &corpus_for_exec,
            &corpus_id,
            &query_text,
            &language,
            start,
            size,
            backend_override.as_deref(),
            Some(&query_opts),
            hcm,
        ))
    })
    .await
    .map(|(r, open_ms)| {
        rec.opened(open_ms);
        r
    });
    let mut response = match joined {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
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
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("query worker join error: {e}"),
            ));
        }
    };
    if let Some(obj) = response.as_object_mut() {
        obj.insert(
            "policy".to_string(),
            json!({
                "test_mode": state.test_mode,
                "role": role,
                "tier": caller.tier,
                "role_verified": caller.verified,
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

#[derive(Debug, Deserialize)]
struct PandoCorpusQuery {
    corpus: String,
    session_id: Option<String>,
    job: Option<String>,
    pos: Option<String>,
    left: Option<String>,
    right: Option<String>,
    sentence: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PandoRunRequest {
    corpus: String,
    #[serde(default)]
    cql: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

async fn http_backends(State(state): State<HttpAppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "pando": state.pando_hcm.as_ref().map(|h| h.status_json()).unwrap_or(json!({"available": false})),
    }))
}

async fn pando_dispatch_get(
    state: &HttpAppState,
    corpus_id: &str,
    path: &str,
    query: &str,
) -> Result<Json<Value>, (StatusCode, String)> {
    let hcm = state
        .pando_hcm
        .clone()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "libflexicorp_pando hot path not available".to_string(),
            )
        })?;
    // Resolve corpus from the in-memory catalog on the async path; only FFI +
    // index_dir resolution stay in spawn_blocking.
    let corpus = state.catalog.get(corpus_id).map_err(to_http_err)?;
    let index_dir = resolve_pando_index_dir(&corpus).map_err(to_http_err)?;
    let corpus_id_owned = corpus_id.to_string();
    let path_owned = path.to_string();
    let query_owned = query.to_string();
    let open_opts = state.limits.engine_options_for(&corpus.settings);
    let result = tokio::task::spawn_blocking(move || -> Result<(i32, Value)> {
        let guard = HotGuard::acquire(hcm, &corpus_id_owned, &index_dir, false, Some(&open_opts))?;
        guard.request("GET", &path_owned, &query_owned, "")
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("worker join: {e}"),
        )
    })?
    .map_err(to_http_err)?;
    let (status, payload) = result;
    if status >= 400 {
        return Err(engine_error_response(status, &payload));
    }
    Ok(Json(payload))
}

async fn http_pando_info(
    State(state): State<HttpAppState>,
    AxumQuery(q): AxumQuery<PandoCorpusQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    pando_dispatch_get(&state, &q.corpus, "/info", "").await
}

async fn http_pando_status(
    State(state): State<HttpAppState>,
    AxumQuery(q): AxumQuery<PandoCorpusQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let job = q.job.as_deref().unwrap_or("");
    if job.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "job is required".to_string()));
    }
    let qs = format!("job={job}");
    pando_dispatch_get(&state, &q.corpus, "/status", &qs).await
}

async fn http_pando_context(
    State(state): State<HttpAppState>,
    AxumQuery(q): AxumQuery<PandoCorpusQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let pos = q.pos.as_deref().unwrap_or("");
    if pos.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "pos is required".to_string()));
    }
    let mut parts = vec![format!("pos={pos}")];
    if let Some(l) = &q.left {
        parts.push(format!("left={l}"));
    }
    if let Some(r) = &q.right {
        parts.push(format!("right={r}"));
    }
    if let Some(s) = &q.sentence {
        parts.push(format!("sentence={s}"));
    }
    let qs = parts.join("&");
    pando_dispatch_get(&state, &q.corpus, "/context", &qs).await
}

async fn http_pando_run(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    Json(req): Json<PandoRunRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let cql = req.cql.clone().filter(|s| !s.trim().is_empty()).or(req.query.clone()).unwrap_or_default();
    let mut rec = QueryRec::new("/run", &req.corpus, &cql);
    for k in ["session_id", "offset", "limit", "group_limit", "timeout_ms"] {
        if let Some(v) = req.extra.get(k) {
            rec.set(k, v.clone());
        }
    }
    let r = http_pando_run_inner(state.clone(), headers, req, &mut rec).await;
    rec.finish(&state, &r);
    r
}

async fn http_pando_run_inner(
    state: HttpAppState,
    headers: HeaderMap,
    req: PandoRunRequest,
    rec: &mut QueryRec,
) -> Result<Json<Value>, (StatusCode, String)> {
    let hcm = state.pando_hcm.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "libflexicorp_pando hot path not available".to_string(),
        )
    })?;
    let cql = req
        .cql
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or(req.query.clone())
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "cql or query required".to_string()))?;
    let str_field = |k: &str| req.extra.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let session_id = str_field("session_id").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let caller = state.limits.caller(
        &headers,
        str_field("request_role").as_deref(),
        str_field("user").as_deref(),
        session_id.as_deref(),
        &header_client_ip(&headers),
    );
    rec.caller(&caller);
    let mut body = json!({ "cql": cql });
    if let Some(obj) = body.as_object_mut() {
        for (k, v) in req.extra {
            if !matches!(k.as_str(), "corpus" | "request_role" | "user" | "tier") {
                obj.insert(k, v);
            }
        }
    }
    set_engine_tier(&mut body, &caller, &state.limits);
    if let Some(sid) = &session_id {
        if !valid_engine_session_id(sid) {
            return Err((StatusCode::BAD_REQUEST, "bad session_id (1-128 of A-Z a-z 0-9 _ - . :)".to_string()));
        }
    }
    let body_s = body.to_string();
    let corpus = state.catalog.get(&req.corpus).map_err(to_http_err)?;
    let index_dir = resolve_pando_index_dir(&corpus).map_err(to_http_err)?;
    let corpus_id = req.corpus.clone();
    // every /run program is heavy (counts, sorts, collocations)
    let t0 = Instant::now();
    let permit = state.limits.admit(&caller).await;
    rec.queued(t0);
    let permit = permit?;
    let open_opts = state.limits.engine_options_for(&corpus.settings);
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        hot_corpus::track_open(|| -> Result<(i32, Value)> {
            let guard = HotGuard::acquire(hcm, &corpus_id, &index_dir, false, Some(&open_opts))?;
            if let Some(sid) = &session_id {
                ensure_engine_session(&guard, sid)?;
            }
            guard.request("POST", "/run", "", &body_s)
        })
    })
    .await
    .map(|(r, open_ms)| {
        rec.opened(open_ms);
        r
    })
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("worker join: {e}"),
        )
    })?
    .map_err(to_http_err)?;
    let (status, payload) = result;
    if status >= 400 {
        return Err(engine_error_response(status, &payload));
    }
    Ok(Json(payload))
}

#[derive(Debug, Deserialize)]
struct PandoSessionRequest {
    corpus: String,
    session_id: Option<String>,
    ttl_s: Option<u64>,
}

/// One engine request (no admission: session bookkeeping is cheap).
async fn pando_dispatch(
    state: &HttpAppState,
    corpus_id: &str,
    method: &'static str,
    path: &'static str,
    query: String,
    body: String,
) -> Result<Json<Value>, (StatusCode, String)> {
    let hcm = state.pando_hcm.clone().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "libflexicorp_pando hot path not available".to_string())
    })?;
    let corpus = state.catalog.get(corpus_id).map_err(to_http_err)?;
    let index_dir = resolve_pando_index_dir(&corpus).map_err(to_http_err)?;
    let corpus_id = corpus_id.to_string();
    let open_opts = state.limits.engine_options_for(&corpus.settings);
    let (status, payload) = tokio::task::spawn_blocking(move || -> Result<(i32, Value)> {
        let guard = HotGuard::acquire(hcm, &corpus_id, &index_dir, false, Some(&open_opts))?;
        guard.request(method, path, &query, &body)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("worker join: {e}")))?
    .map_err(to_http_err)?;
    if status >= 400 {
        return Err(engine_error_response(status, &payload));
    }
    Ok(Json(payload))
}

fn session_qs(sid: &str) -> String {
    format!("session_id={}", sid)
}

async fn http_pando_session_create(
    State(state): State<HttpAppState>,
    Json(req): Json<PandoSessionRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut body = json!({});
    if let Some(sid) = req.session_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if !valid_engine_session_id(sid) {
            return Err((StatusCode::BAD_REQUEST, "bad session_id (1-128 of A-Z a-z 0-9 _ - . :)".to_string()));
        }
        body["session_id"] = json!(sid);
    }
    if let Some(t) = req.ttl_s {
        body["ttl_s"] = json!(t);
    }
    pando_dispatch(&state, &req.corpus, "POST", "/session", String::new(), body.to_string()).await
}

async fn http_pando_session_info(
    State(state): State<HttpAppState>,
    AxumQuery(q): AxumQuery<PandoCorpusQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let sid = q.session_id.as_deref().unwrap_or("").trim().to_string();
    if !valid_engine_session_id(&sid) {
        return Err((StatusCode::BAD_REQUEST, "session_id is required".to_string()));
    }
    pando_dispatch(&state, &q.corpus, "GET", "/session", session_qs(&sid), String::new()).await
}

async fn http_pando_session_close(
    State(state): State<HttpAppState>,
    Json(req): Json<PandoSessionRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let sid = req.session_id.as_deref().unwrap_or("").trim().to_string();
    if !valid_engine_session_id(&sid) {
        return Err((StatusCode::BAD_REQUEST, "session_id is required".to_string()));
    }
    pando_dispatch(&state, &req.corpus, "POST", "/session/close", String::new(),
                   json!({ "session_id": sid }).to_string()).await
}

async fn http_pando_sessions(
    State(state): State<HttpAppState>,
    AxumQuery(q): AxumQuery<PandoCorpusQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    pando_dispatch(&state, &q.corpus, "GET", "/sessions", String::new(), String::new()).await
}

// ── FCS (CLARIN Federated Content Search, SRU 1.2 / 2.0) ───────────────
//
// The endpoint itself is `fcs::*` (query parsing, translation, engines, SRU
// XML) and knows nothing about FQS. This part only maps catalogue rows to FCS
// resources and engines, and applies FQS's access rules and admission.

/// `capabilities.fcs` overlaid by `settings.fcs`.
fn fcs_config(corpus: &CorpusEntry) -> Value {
    let mut o = corpus.capabilities.get("fcs").and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(s) = corpus.settings.get("fcs").and_then(Value::as_object) {
        for (k, v) in s {
            o.insert(k.clone(), v.clone());
        }
    }
    Value::Object(o)
}

/// Which engine answers FCS for a corpus: `fcs.engine` (pando | cwb | kontext),
/// else a `settings.kontext` block, else the corpus's query backend.
fn fcs_dialect(corpus: &CorpusEntry, fcs_cfg: &Value) -> Option<fcs::translate::Dialect> {
    use fcs::translate::Dialect;
    let named = fcs_cfg.get("engine").and_then(Value::as_str).map(str::to_ascii_lowercase);
    let kind = match named.as_deref() {
        Some(k) => k.to_string(),
        None if corpus.settings.get("kontext").and_then(|k| k.get("url")).is_some() => "kontext".into(),
        None => resolve_effective_backend(corpus).ok()?,
    };
    match kind.as_str() {
        "pando" => Some(Dialect::Pando),
        "cqp" | "cwb" => Some(Dialect::Cwb),
        "kontext" | "manatee" | "noske" => Some(Dialect::Manatee),
        _ => None,
    }
}

/// KonText base URL and corpus name for a Manatee resource: `settings.kontext
/// {url, corpname}`, else the `hit_link` of a kontext preset (`base`, `corpus`).
fn fcs_kontext_settings(corpus: &CorpusEntry) -> (Option<String>, String) {
    let k = corpus.settings.get("kontext");
    let cfg = fcs_config(corpus);
    let link = cfg.get("hit_link").filter(|l| {
        l.get("frontend").and_then(Value::as_str).is_some_and(|f| f.starts_with("kontext"))
    });
    let url = k
        .and_then(|k| k.get("url"))
        .or_else(|| link.and_then(|l| l.get("base")))
        .and_then(Value::as_str)
        .map(|s| s.trim_end_matches('/').to_string());
    let corpname = k
        .and_then(|k| k.get("corpname"))
        .or_else(|| link.and_then(|l| l.get("corpus")))
        .or_else(|| corpus.settings.get("corpus_name"))
        .and_then(Value::as_str)
        .unwrap_or(&corpus.id)
        .to_string();
    (url, corpname)
}

fn fcs_resource(state: &HttpAppState, corpus: &CorpusEntry) -> Option<fcs::Resource> {
    let cfg = fcs_config(corpus);
    let dialect = fcs_dialect(corpus, &cfg)?;
    // links for users go to KonText's public URL (`settings.kontext.public_url`),
    // which can differ from the one FQS queries (`url`, e.g. a local address)
    let (mut default_hit_link, mut landing) = (None, corpus.project_url.clone());
    if dialect == fcs::translate::Dialect::Manatee {
        let (url, corpname) = fcs_kontext_settings(corpus);
        let public = corpus
            .settings
            .get("kontext")
            .and_then(|k| k.get("public_url"))
            .and_then(Value::as_str)
            .map(|s| s.trim_end_matches('/').to_string())
            .or(url);
        if let Some(u) = public {
            let c = fcs::engine::url_encode(&corpname);
            default_hit_link = Some(fcs::config::HitLink {
                template: format!("{u}/create_view?corpname={c}&q=q{{cql}}&pagesize=1&fromp={{n}}"),
                dialect: fcs::translate::Dialect::Manatee,
            });
            landing = landing.or(Some(format!("{u}/query?corpname={c}")));
        }
    }
    Some(fcs::Resource::from_spec(&fcs::ResourceSpec {
        id: &corpus.id,
        label: &corpus.label,
        landing_page: landing.as_deref(),
        fcs: &cfg,
        dialect,
        base_url: state.fcs_base_url.as_deref(),
        default_hit_link,
    }))
}

/// The FCS resources a caller may search (FCS-enabled, current, HTTP-visible).
fn fcs_resources(state: &HttpAppState, role: &str) -> Vec<(fcs::Resource, CorpusEntry)> {
    state
        .catalog
        .list(None, false, None)
        .into_iter()
        .filter(|c| is_fcs_enabled(c))
        .filter(|c| state.test_mode || (is_http_access_allowed(c, role) && is_http_operation_allowed(c, "query")))
        .filter_map(|c| fcs_resource(state, &c).map(|r| (r, c)))
        .collect()
}

/// Public host, port and path of `--fcs-base-url` (explain's serverInfo), if given.
fn fcs_public_address(base: &str) -> Option<(String, u16, String)> {
    let (scheme, rest) = base.split_once("://")?;
    let (hostport, path) = rest.split_once('/').unwrap_or((rest, ""));
    let default_port = if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h, p.parse().ok()?),
        _ => (hostport, default_port),
    };
    Some((host.to_string(), port, path.trim_matches('/').to_string()))
}

fn fcs_endpoint(state: &HttpAppState) -> fcs::Endpoint {
    // serverInfo names the public endpoint, not FQS's bind address
    let (host, port, database) = state
        .fcs_base_url
        .as_deref()
        .and_then(fcs_public_address)
        .unwrap_or_else(|| (state.host.clone(), state.port, state.fcs_database.clone()));
    fcs::Endpoint {
        host,
        port,
        database,
        base_url: state.fcs_base_url.clone(),
        title: state.server_name.clone().unwrap_or_else(|| "FQS corpus search".to_string()),
        description: "CLARIN-FCS endpoint of FQS (Flexicorp Query Server)".to_string(),
        default_records: 50,
        max_records: 1000,
    }
}

/// The `pando` command line for FCS without the warm library: `settings.pando_binary`,
/// else `PANDO_BINARY`, else next to the fqs binary, /usr/local/bin, /usr/bin,
/// /opt/pando/bin or ~/.local/bin (install.sh), else `pando` on the PATH.
fn fcs_pando_binary(corpus: &CorpusEntry) -> String {
    corpus
        .settings
        .get("pando_binary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("PANDO_BINARY").ok().filter(|s| !s.trim().is_empty()))
        .or_else(|| {
            // services often run with a minimal PATH: look where pando is usually installed
            let mut c: Vec<PathBuf> = Vec::new();
            if let Ok(exe) = std::env::current_exe() {
                if let Some(d) = exe.parent() {
                    c.push(d.join("pando"));
                }
            }
            c.push(PathBuf::from("/usr/local/bin/pando"));
            c.push(PathBuf::from("/usr/bin/pando"));
            c.push(PathBuf::from("/opt/pando/bin/pando"));
            if let Ok(h) = std::env::var("HOME") {
                c.push(PathBuf::from(h).join(".local/bin/pando"));
            }
            c.into_iter().find(|p| p.is_file()).map(|p| p.display().to_string())
        })
        .unwrap_or_else(|| "pando".to_string())
}

/// An engine for one resource, built inside the blocking worker.
fn fcs_engine(
    corpus: &CorpusEntry,
    dialect: fcs::translate::Dialect,
    hcm: Option<Arc<HotCorpusManager>>,
    extra: &serde_json::Map<String, Value>,
    open_options: Option<String>,
) -> std::result::Result<Box<dyn fcs::Engine>, String> {
    use fcs::translate::Dialect;
    let timeout = Duration::from_secs(
        corpus.settings.get("fcs_timeout_secs").and_then(Value::as_u64).unwrap_or(120),
    );
    match dialect {
        Dialect::Pando => {
            let transport: fcs::engine::PandoTransport =
                if let Some(url) = corpus.settings.get("pando_server").and_then(Value::as_str) {
                    fcs::engine::pando_http_transport(url, timeout)
                } else if let Some(hcm) = hcm {
                    let index_dir = resolve_pando_index_dir(corpus).map_err(|e| e.to_string())?;
                    let guard = HotGuard::acquire(hcm, &corpus.id, &index_dir, false, open_options.as_deref())
                        .map_err(|e| e.to_string())?;
                    Box::new(move |body: &str| {
                        guard
                            .request("POST", "/query", "", body)
                            .map(|(st, v)| (st.clamp(0, 999) as u16, v))
                            .map_err(|e| e.to_string())
                    })
                } else {
                    // no warm library: pando's own command line, one process per request
                    let index_dir = resolve_pando_index_dir(corpus).map_err(|e| e.to_string())?;
                    fcs::engine::pando_cli_transport(fcs_pando_binary(corpus), index_dir)
                };
            Ok(Box::new(fcs::engine::PandoEngine { transport, extra: extra.clone() }))
        }
        Dialect::Cwb => {
            let corpus_name = corpus
                .settings
                .get("corpus_name")
                .and_then(Value::as_str)
                .or_else(|| corpus.settings.get("cqp_corpus").and_then(Value::as_str))
                .unwrap_or(&corpus.id)
                .to_string();
            let cqp = corpus
                .settings
                .get("cqp_binary")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| std::env::var("CQP_BINARY").ok().filter(|s| !s.trim().is_empty()))
                .unwrap_or_else(|| "cqp".to_string());
            let (_, registry) = resolve_cqp_registry(corpus);
            Ok(Box::new(fcs::engine::CwbEngine {
                cqp,
                registry,
                corpus: corpus_name,
                cwd: Some(resolve_cqp_cwd(corpus)),
            }))
        }
        Dialect::Manatee => {
            let (url, corpname) = fcs_kontext_settings(corpus);
            let url = url.ok_or("settings.kontext.url is not set")?;
            let k = corpus.settings.get("kontext");
            let extra_params = k
                .and_then(|k| k.get("params"))
                .and_then(Value::as_object)
                .map(|m| m.iter().filter_map(|(a, b)| b.as_str().map(|b| (a.clone(), b.to_string()))).collect())
                .unwrap_or_default();
            Ok(Box::new(fcs::engine::KontextEngine {
                url,
                corpname,
                action: k.and_then(|k| k.get("action")).and_then(Value::as_str).unwrap_or("view").to_string(),
                timeout,
                extra_params,
            }))
        }
    }
}

fn fcs_xml(xml: String) -> Response {
    ([(header::CONTENT_TYPE, "application/xml; charset=utf-8")], xml).into_response()
}

async fn http_fcs(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumQuery(params): AxumQuery<HashMap<String, String>>,
) -> Response {
    let ep = fcs_endpoint(&state);
    // FCS clients (the aggregator) are anonymous unless a front end signs a token
    let caller = state.limits.caller(&headers, None, None, None, &header_client_ip(&headers));
    let visible = fcs_resources(&state, &caller.role);
    let resources: Vec<fcs::Resource> = visible.iter().map(|(r, _)| r.clone()).collect();
    let req = match fcs::parse_request(&params, &ep) {
        Ok(r) => r,
        Err(refusal) => return fcs_xml(fcs::explain_refusal_xml(&refusal, &ep, &resources)),
    };
    match req.operation {
        fcs::Operation::Explain => return fcs_xml(fcs::explain(&req, &ep, &resources)),
        fcs::Operation::Scan => {
            return fcs_xml(fcs::refusal_xml(&(
                req.version,
                req.operation,
                fcs::Diagnostic::sru(4, Some("scan"), "Unsupported operation"),
            )));
        }
        fcs::Operation::SearchRetrieve => {}
    }
    let mut rec = QueryRec::new("/fcs", &req.context.join(","), &req.query);
    rec.caller(&caller);
    rec.set("start", json!(req.start));
    rec.set("size", json!(req.max));
    rec.set("query_type", json!(if req.query_type == fcs::QueryType::Fcs { "fcs" } else { "cql" }));
    let plan = match fcs::prepare_search(&req, &resources) {
        Ok(p) => p,
        Err(refusal) => {
            rec.finish_status(&state, 200, Some(&refusal.2.message), None);
            return fcs_xml(fcs::refusal_xml(&refusal));
        }
    };
    rec.set("resources", json!(plan.parts.iter().map(|p| p.resource.id.clone()).collect::<Vec<_>>()));
    rec.set("native", json!(plan.parts.iter().map(|p| p.native.queries.clone()).collect::<Vec<_>>()));
    // cqp runs as a child process: one process slot for the whole request
    let spawns = plan.parts.iter().any(|p| match p.resource.dialect {
        fcs::translate::Dialect::Cwb => true,
        // pando without the warm library or a pando-server runs its command line
        fcs::translate::Dialect::Pando => {
            state.pando_hcm.is_none()
                && visible.iter().any(|(r, c)| r.id == p.resource.id && c.settings.get("pando_server").is_none())
        }
        fcs::translate::Dialect::Manatee => false,
    });
    let t0 = Instant::now();
    let permit = if spawns {
        match state.limits.admit_process(&caller.tier).await {
            Ok(p) => Some(p),
            Err((code, msg)) => {
                rec.finish_status(&state, code.as_u16(), Some(&msg), None);
                return fcs_xml(fcs::refusal_xml(&(
                    req.version,
                    req.operation,
                    fcs::Diagnostic::sru(2, Some("server busy"), "System temporarily unavailable; try again shortly"),
                )));
            }
        }
    } else {
        None
    };
    rec.queued(t0);
    let corpora: HashMap<String, CorpusEntry> = visible.into_iter().map(|(r, c)| (r.id, c)).collect();
    let hcm = state.pando_hcm.clone();
    let mut extra = serde_json::Map::new();
    if state.limits.has_tiers() && !caller.tier.is_empty() {
        extra.insert("tier".into(), json!(caller.tier));
    }
    let limits = state.limits.clone();
    let xml = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let engine_for = |r: &fcs::Resource| {
            let c = corpora.get(&r.id).ok_or_else(|| format!("corpus '{}' is gone", r.id))?;
            fcs_engine(c, r.dialect, hcm.clone(), &extra, Some(limits.engine_options_for(&c.settings)))
        };
        fcs::search(&req, &ep, plan, &engine_for)
    })
    .await;
    match xml {
        Ok(x) => {
            let n = x.split("numberOfRecords>").nth(1).and_then(|s| s.split('<').next()).and_then(|s| s.parse::<u64>().ok());
            if let Some(n) = n {
                rec.set("total", json!(n));
            }
            let diag = x.contains("<diag:uri>");
            rec.finish_status(&state, 200, diag.then_some("diagnostic"), None);
            fcs_xml(x)
        }
        Err(e) => {
            rec.finish_status(&state, 500, Some(&e.to_string()), None);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("worker join: {e}")).into_response()
        }
    }
}

fn to_http_err(err: anyhow::Error) -> (StatusCode, String) {
    if let Some(e) = err.downcast_ref::<EngineHttpError>() {
        return engine_error_response(e.status as i32, &e.payload);
    }
    (StatusCode::BAD_REQUEST, err.to_string())
}

fn normalize_role(role: Option<&str>) -> String {
    limits::normalize_role(role)
}

fn header_client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string()))
        .unwrap_or_else(|| "-".to_string())
}

/// A pando engine answer with an HTTP error status (403 denied, 408 timeout,
/// 413 too large, 404 unknown session / set, …): passed on with its status and
/// its JSON (as the error text).
#[derive(Debug)]
struct EngineHttpError {
    status: u16,
    payload: Value,
}

impl std::fmt::Display for EngineHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = self.payload.get("error").and_then(|v| v.as_str()).unwrap_or("pando error");
        write!(f, "pando HTTP {}: {}", self.status, msg)
    }
}

impl std::error::Error for EngineHttpError {}

fn engine_error_response(status: i32, payload: &Value) -> (StatusCode, String) {
    (
        StatusCode::from_u16(status as u16).unwrap_or(StatusCode::BAD_REQUEST),
        payload.to_string(),
    )
}

/// Pando session ids: 1-128 of A-Z a-z 0-9 _ - . : (pando's SessionManager::valid_id).
fn valid_engine_session_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

/// Make sure the engine has session `sid` (idempotent: an existing one is reused).
fn ensure_engine_session(guard: &HotGuard, sid: &str) -> Result<()> {
    let (status, payload) = guard.request("POST", "/session", "", &json!({ "session_id": sid }).to_string())?;
    if status >= 400 {
        return Err(EngineHttpError { status: status as u16, payload }.into());
    }
    Ok(())
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

/// Whether a search on `corpus` runs as a child process: the CQP backend (python
/// flexicorp + cqp) or pando without the in-process library (cold CLI).
fn spawns_process(state: &HttpAppState, corpus: &CorpusEntry, backend_override: Option<&str>) -> bool {
    let backend = match backend_override.map(str::trim).filter(|s| !s.is_empty()) {
        Some(b) => b.to_string(),
        None => match resolve_effective_backend(corpus) {
            Ok(b) => b,
            Err(_) => return false,
        },
    };
    match backend.as_str() {
        "cqp" => true,
        "pando" => state.pando_hcm.is_none(),
        _ => false,
    }
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

fn normalize_pando_query(query: &str) -> String {
    query.trim_end().trim_end_matches(';').trim_end().to_string()
}

fn is_fcs_enabled(corpus: &CorpusEntry) -> bool {
    fcs_config(corpus).get("enabled").and_then(Value::as_bool).unwrap_or(false)
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
    share_db_files_with_group(path);
    init_schema(&conn)?;
    Ok(conn)
}

/// The service (user `fqs`) and TEITOK's PHP (`www-data`, in group `fqs`) both
/// write the catalog; the directory is setgid `fqs` (install: mode 2775). A
/// catalog file created with umask 022 (0644) is then writable for its creator
/// only, and the other side fails with "attempt to write a readonly database".
/// When the parent directory is group-writable, give the files we own
/// (`fqs.db`, `-wal`, `-shm`, `-journal`) group write too. SQLite creates the
/// journal / WAL files with the database file's mode, so fixing `fqs.db` keeps
/// them right as well.
#[cfg(unix)]
fn share_db_files_with_group(path: &Path) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Some(parent) = path.parent() else { return };
    let Ok(pm) = fs::metadata(if parent.as_os_str().is_empty() { Path::new(".") } else { parent }) else {
        return;
    };
    if pm.mode() & 0o020 == 0 {
        return; // directory not group-writable: a single-user setup, leave modes alone
    }
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        let p = PathBuf::from(p);
        let Ok(m) = fs::metadata(&p) else { continue };
        if m.mode() & 0o060 == 0o060 {
            continue;
        }
        let mut perm = m.permissions();
        perm.set_mode((m.mode() | 0o060) & 0o7777);
        // only the owner may chmod: a file the other user created is fixed when
        // that user next opens the catalog (or by the installer's chmod)
        let _ = fs::set_permissions(&p, perm);
    }
}

#[cfg(not(unix))]
fn share_db_files_with_group(_path: &Path) {}


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
    // Display / API lists: newest first (same idea as reindex_history / activity log).
    // Queued dispatch still uses pick_next_queued_job_for_worker (priority DESC, requested_at ASC).
    let order = match st.as_deref() {
        Some("queued") => {
            // Active queue view: fair FIFO within priority.
            " ORDER BY priority DESC, requested_at ASC, job_id ASC"
        }
        Some("running") => " ORDER BY COALESCE(started_at, requested_at) DESC, job_id DESC",
        _ => {
            // all / completed / failed / … — reverse+head, not chronological tail.
            " ORDER BY COALESCE(finished_at, started_at, updated_at, requested_at) DESC, job_id DESC"
        }
    };
    if st.is_some() {
        sql.push_str(" AND status = ?1");
        if corpus.is_some() {
            sql.push_str(" AND corpus_id = ?2");
            sql.push_str(order);
            sql.push_str(" LIMIT ?3");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![st, corpus, limit as i64], row_to_reindex_job)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            return Ok(rows);
        }
        sql.push_str(order);
        sql.push_str(" LIMIT ?2");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![st, limit as i64], row_to_reindex_job)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        return Ok(rows);
    }
    if corpus.is_some() {
        sql.push_str(" AND corpus_id = ?1");
        sql.push_str(order);
        sql.push_str(" LIMIT ?2");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![corpus, limit as i64], row_to_reindex_job)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        return Ok(rows);
    }
    sql.push_str(order);
    sql.push_str(" LIMIT ?1");
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
        // the catalogue follows the new index: features and languages, size, date
        if let Err(e) = refresh_catalog_after_reindex(conn, &existing.corpus_id) {
            eprintln!("[fqs] catalogue update after reindex of {}: {e:#}", existing.corpus_id);
        }
    }
    get_reindex_job(conn, job_id)
}

/// After a successful reindex: detect the corpus's features and languages again (the
/// same as `fqs corpora enrich`), take its size from the new Pando index, and note the
/// update time — so that corpus lists show what the corpus now contains.
fn refresh_catalog_after_reindex(conn: &Connection, corpus_id: &str) -> Result<()> {
    let mut entry = get_corpus(conn, corpus_id)?;
    let _ = enrich::enrich_corpus_entry(&mut entry);
    if let Some(n) = pando_index_size(&entry) {
        entry.corpus_size = Some(n);
        entry.corpus_size_updated_at = Some(now_rfc3339());
    }
    entry.last_corpus_update_at = Some(now_rfc3339());
    upsert_corpus(conn, &entry)
}

/// Tokens in the corpus's Pando index (`size=` in its corpus.info): exact and cheap,
/// unlike counting a query's hits (the probe query needs a `word` attribute).
fn pando_index_size(entry: &CorpusEntry) -> Option<i64> {
    let dir = resolve_pando_index_dir(entry).ok()?;
    let info = fs::read_to_string(dir.join("corpus.info")).ok()?;
    info.lines()
        .find_map(|l| l.strip_prefix("size="))
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
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
    // no size given (registration from TEITOK, a scan, the admin form): the Pando index's
    let (corpus_size, corpus_size_updated_at) = match entry.corpus_size {
        Some(_) => (entry.corpus_size, entry.corpus_size_updated_at.clone()),
        None => match pando_index_size(entry) {
            Some(n) => (Some(n), Some(now_rfc3339())),
            None => (None, None),
        },
    };
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
            corpus_size,
            corpus_size_updated_at,
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
                    // the index's own size; the probe's hit count only as a fallback
                    corpus_size = pando_index_size(corpus).or(size);
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

    // the size from the Pando index itself: also for a quick validation (no query), and
    // when the probe gives none
    if corpus_size.is_none() {
        if let Some(n) = pando_index_size(corpus) {
            checks.push(format!("pando index: {n} tokens (corpus.info)"));
            corpus_size = Some(n);
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
    kind: String,
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

fn run_pando_query(
    corpus: &CorpusEntry,
    query_text: &str,
    start: u32,
    size: u32,
    query_options: Option<&HttpQueryRequest>,
    pando_hcm: Option<&Arc<HotCorpusManager>>,
) -> Result<PandoExecResult> {
    let index_dir = resolve_pando_index_dir(corpus)?;
    let window = query_options.and_then(|q| q.window);
    let sentence = query_options
        .and_then(|q| q.context_scope.as_deref())
        .map(|s| {
            let t = s.trim().to_lowercase();
            t == "s" || t == "sentence" || t == "sent"
        })
        .unwrap_or(false);
    // Prefer async when HCM is available so totals can progress in the background;
    // TEITOK can still poll /status later. Sync true remains the CLI default.
    let total_mode = if pando_hcm.is_some() { "async" } else { "true" };

    if let Some(hcm) = pando_hcm {
        let guard = HotGuard::acquire(
            Arc::clone(hcm),
            &corpus.id,
            &index_dir,
            false,
            query_options.and_then(|q| q.engine_open_options.as_deref()),
        )?;
        let mut extra = serde_json::Map::new();
        if let Some(q) = query_options {
            if let Some(t) = &q.engine_tier {
                extra.insert("tier".into(), json!(t));
            }
            if let Some(ms) = q.timeout_ms {
                extra.insert("timeout_ms".into(), json!(ms));
            }
            if let Some(n) = q.sample.filter(|n| *n > 0) {
                extra.insert("sample".into(), json!(n));
            }
            let synthetic_default = corpus
                .settings
                .pointer("/pando/synthetic_fragments")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if q.fragment.unwrap_or(synthetic_default) {
                extra.insert("fragment".into(), json!(true));
            }
            if q.shuffle == Some(true) {
                extra.insert("shuffle".into(), json!(true));
            }
            if let Some(sd) = q.seed {
                extra.insert("seed".into(), json!(sd));
            }
            // hit-set session: only when the caller stores or pages a set
            let sid = q.session_id.as_deref().map(str::trim).unwrap_or("");
            if (q.name.is_some() || q.from.is_some()) && valid_engine_session_id(sid) {
                ensure_engine_session(&guard, sid)?;
                extra.insert("session_id".into(), json!(sid));
                if let Some(n) = &q.name {
                    extra.insert("name".into(), json!(n));
                }
                if let Some(f) = &q.from {
                    extra.insert("from".into(), json!(f));
                }
            } else if q.name.is_some() || q.from.is_some() {
                anyhow::bail!("'name' / 'from' need a session_id (1-128 of A-Z a-z 0-9 _ - . :)");
            }
        }
        let body = pando_query_body(query_text, start, size, window, sentence, total_mode, &extra);
        let (status, mut engine) = guard.request("POST", "/query", "", &body)?;
        if status >= 400 {
            return Err(EngineHttpError { status: status as u16, payload: engine }.into());
        }
        // a TEITOK project: the hits' own XML from its xidx (as the flexicorp-pando command
        // line gives it), unless plain text was asked for or the hits carry synthetic XML
        let wants_xml = query_options
            .and_then(|q| q.context_format.as_deref())
            .map(|f| !f.trim().eq_ignore_ascii_case("text"))
            .unwrap_or(true);
        if wants_xml && !extra.contains_key("fragment") {
            let root = &corpus.project_root;
            if root.join("xidx").join("tokens.bin").is_file() {
                let scope = query_options
                    .and_then(|q| q.context_scope.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("s");
                if let Err(e) = add_xidx_fragments(&guard, root, &mut engine, scope, window.unwrap_or(5) as i32) {
                    eprintln!("[fqs] {}: xidx fragments: {e:#}", corpus.id);
                }
            }
        }
        let payload = wrap_pando_server_as_fqs_raw(engine);
        return Ok(PandoExecResult {
            kind: "flexicorp-pando-lib".to_string(),
            binary: hcm.lib().build_string(),
            index_dir: index_dir.display().to_string(),
            exit_code: 0,
            payload,
        });
    }

    // Cold CLI fallback (Path A / library missing)
    let binaries = resolve_flexicorp_pando_bins(corpus);
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
        if let Some(w) = window {
            cmd.arg("--context").arg(w.to_string());
        }
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
        kind: "flexicorp-pando-cli".to_string(),
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

/// The JSON objects of an upsert payload as given (to see which fields it sets).
fn raw_entries_from_json(payload: &str) -> Vec<Value> {
    match serde_json::from_str::<Value>(payload) {
        Ok(Value::Array(a)) => a,
        Ok(v @ Value::Object(_)) => vec![v],
        _ => Vec::new(),
    }
}

/// The admin's choices inside settings, which registration payloads from TEITOK do not
/// carry: `fcs.enabled`, `kontext.{corpname,public_url,url}` (and `description`, below).
const ADMIN_OWNED_SETTINGS: &[(&str, &str)] = &[
    ("fcs", "enabled"),
    ("kontext", "corpname"),
    ("kontext", "public_url"),
    ("kontext", "url"),
];

/// Upsert of an existing row: fields the payload leaves out keep their stored value;
/// settings and capabilities are replaced as given, except the admin-owned keys above
/// when the payload does not set them.
fn merge_upsert_entry(old: &CorpusEntry, new: &CorpusEntry, raw: &Value) -> Result<CorpusEntry> {
    let Some(raw_obj) = raw.as_object() else {
        return Ok(new.clone());
    };
    let old_v = serde_json::to_value(old)?;
    let mut new_v = serde_json::to_value(new)?;
    if let (Some(o), Some(n)) = (old_v.as_object(), new_v.as_object_mut()) {
        for (k, v) in o {
            if k == "id" || raw_obj.contains_key(k) {
                continue;
            }
            n.insert(k.clone(), v.clone());
        }
    }
    let mut merged: CorpusEntry = serde_json::from_value(new_v)?;
    if !merged.settings.is_object() {
        merged.settings = json!({});
    }
    // the description written in the admin (or in the TEITOK project and sent along)
    if let Some(d) = old.settings.get("description") {
        let set_in_payload = raw_obj.get("settings").and_then(|s| s.get("description")).is_some();
        if !set_in_payload {
            if let Some(o) = merged.settings.as_object_mut() {
                o.insert("description".into(), d.clone());
            }
        }
    }
    for (block, key) in ADMIN_OWNED_SETTINGS {
        let Some(old_val) = old.settings.get(*block).and_then(|b| b.get(*key)) else {
            continue;
        };
        let set_in_payload = raw_obj
            .get("settings")
            .and_then(|s| s.get(*block))
            .and_then(|b| b.get(*key))
            .is_some();
        if set_in_payload {
            continue;
        }
        let settings = merged.settings.as_object_mut().expect("object");
        let b = settings.entry(block.to_string()).or_insert_with(|| json!({}));
        if !b.is_object() {
            *b = json!({});
        }
        if let Some(bo) = b.as_object_mut() {
            bo.insert(key.to_string(), old_val.clone());
        }
    }
    Ok(merged)
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

#[cfg(test)]
mod fcs_address_tests {
    #[test]
    fn public_address() {
        assert_eq!(super::fcs_public_address("https://lindat.cz/services/test-kontext/fcs"),
                   Some(("lindat.cz".into(), 443, "services/test-kontext/fcs".into())));
        assert_eq!(super::fcs_public_address("http://localhost:8797/fcs"), Some(("localhost".into(), 8797, "fcs".into())));
    }
}

#[cfg(test)]
mod upsert_merge_tests {
    use super::*;

    fn entry(v: Value) -> CorpusEntry {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn reregistration_keeps_admin_choices_and_omitted_fields() {
        let old = entry(json!({
            "id": "c", "label": "My corpus", "project_root": "/p", "preferred_backend": "auto", "corpus_size": 42,
            "project_url": "/teitok/c/index.php",
            "settings": {"fcs": {"enabled": false}, "kontext": {"corpname": "c_k", "enabled": true}, "query_backend": "pando"},
            "capabilities": {"fcs": {"enabled": true}}
        }));
        // what fqs.php / create-project send again
        let raw = json!({
            "id": "c", "label": "My corpus", "project_root": "/p", "preferred_backend": "pando",
            "settings": {"kontext": {"enabled": true, "public": false, "corpus_id": "c"}},
            "capabilities": {"fcs": {"enabled": true}}
        });
        let new = entry(raw.clone());
        let m = merge_upsert_entry(&old, &new, &raw).unwrap();
        assert_eq!(m.preferred_backend, "pando");
        assert_eq!(m.corpus_size, Some(42));
        assert_eq!(m.project_url.as_deref(), Some("/teitok/c/index.php"));
        assert_eq!(m.settings["fcs"]["enabled"], false);
        assert_eq!(m.settings["kontext"]["corpname"], "c_k");
        assert_eq!(m.settings["kontext"]["corpus_id"], "c");
        // other settings are replaced as given
        assert!(m.settings.get("query_backend").is_none());
        // an explicit value in the payload wins
        let raw2 = json!({"id": "c", "label": "x", "project_root": "/p", "preferred_backend": "auto", "settings": {"fcs": {"enabled": true}}});
        let m2 = merge_upsert_entry(&old, &entry(raw2.clone()), &raw2).unwrap();
        assert_eq!(m2.settings["fcs"]["enabled"], true);
    }
}

#[cfg(test)]
mod catalog_fresh_tests {
    use super::*;

    #[test]
    fn a_row_written_a_moment_ago_is_found() {
        let d = std::env::temp_dir().join(format!("fqs-cat-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let db = d.join("fqs.db");
        let conn = open_db(&db).unwrap();
        let cat = CorpusCatalog::load_from_db(&db).unwrap();
        // a lookup now: the catalog has just reloaded (throttled for a second)
        assert!(cat.get("ntrex").is_err());
        let e: CorpusEntry = serde_json::from_value(json!({
            "id": "ntrex", "label": "NTREX", "project_root": "/p", "preferred_backend": "auto"
        })).unwrap();
        upsert_corpus(&conn, &e).unwrap();
        // TEITOK: upsert, then enqueue right away
        assert_eq!(cat.get("ntrex").unwrap().label, "NTREX");
        let _ = fs::remove_dir_all(&d);
    }
}
