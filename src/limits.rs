//! Limits by tier: who is asking (role → tier), how many heavy requests may run
//! (admission), and what the engine may do for them (pando tier limits).
//!
//! * Role: the caller (TEITOK, KonText) says `request_role` — `visitor` (not
//!   logged in), `user` (logged in), `admin` (corpus / server admin). With a
//!   shared secret (`--jwt-secret`, env `FQS_SECRET`, the one TEITOK already uses
//!   for its `Authorization: Bearer` HS256 token) the role is taken only from a
//!   valid, unexpired token's `role` claim, and a request without one is a
//!   visitor. Without a secret the request's `request_role` is believed, as
//!   before (reported as `"role_trust": "unverified"` in /health).
//! * Tier: the role's entry in `tiers` (else `default_tier`).
//! * Admission: a heavy request (a `/run` program, a `/query` with a `;`
//!   program) takes a slot of its tier (`slots`) and one of the server-wide
//!   `heavy_slots`, waiting at most `queue_ms` (else 503 + Retry-After); a user
//!   (the token's `sub` / `user`, else the FQS session id, else the client
//!   address) may run at most `per_user` heavy requests at once (else 429).
//!   Page requests, /status, /info, /context and sessions never queue.
//! * Engine: the same tier objects go to pando (`flexicorp_pando_open_opts`
//!   options `tiers` / `default_tier` / `trust_tier: true`), and FQS puts
//!   `"tier"` into every engine request body (dropping any the client sent).
//!
//! Limits file (`--limits FILE`, env `FQS_LIMITS`):
//! ```json
//! {"heavy_slots": 8, "default_tier": "visitor",
//!  "tiers": {
//!    "visitor": {"slots": 2, "per_user": 1, "queue_ms": 3000,
//!                "timeout_ms": 20000, "total_timeout_ms": 60000, "max_count_hits": 2000000,
//!                "max_hits": 500000, "threads": 1, "deny": ["transitive", "regex_no_prefix"]},
//!    "user":    {"slots": 4, "per_user": 2, "queue_ms": 10000, "timeout_ms": 60000, …},
//!    "admin":   {"per_user": 4, "queue_ms": 30000}},
//!  "pando": {"query_threads": 4, "session_max_hits": 5000000}}
//! ```
//!
//! * Processes: every request that runs as a child process (the CQP backend:
//!   `python -m flexicorp` + `cqp`; the cold pando CLI when libflexicorp_pando is
//!   missing) — pages included — takes one of `process_slots` (default: half the
//!   CPUs, at least 2), waiting at most `process_queue_ms` (default 30000) before
//!   503 busy. A burst of searches then queues instead of starting a process each.
//!   Hot pando requests run in-process and never take one.

use anyhow::{Context, Result};
use axum::http::{HeaderMap, StatusCode};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde_json::{json, Map, Value};
use sha2::Sha256;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone, Debug)]
struct TierAdmission {
    slots: Option<Arc<Semaphore>>,
    slots_n: usize,
    per_user: usize,
    queue: Duration,
}

pub struct Limits {
    /// The file as given (tiers etc.), for /health.
    config: Value,
    default_tier: String,
    tier_names: Vec<String>,
    tiers: HashMap<String, TierAdmission>,
    global: Option<Arc<Semaphore>>,
    global_n: usize,
    jwt_secret: Option<Vec<u8>>,
    users: Mutex<HashMap<String, usize>>,
    process: Arc<Semaphore>,
    process_n: usize,
    process_queue: Duration,
}

fn default_process_slots() -> usize {
    std::thread::available_parallelism().map(|n| n.get() / 2).unwrap_or(2).max(2)
}

/// Who is asking, as resolved for one request.
#[derive(Clone, Debug)]
pub struct Caller {
    pub role: String,
    pub tier: String,
    pub user: String,
    pub verified: bool,
}

/// Held while a heavy request runs (moved into its blocking task).
pub struct AdmitPermit {
    _tier: Option<OwnedSemaphorePermit>,
    _global: Option<OwnedSemaphorePermit>,
    user: Option<(Arc<Limits>, String)>,
}

impl Drop for AdmitPermit {
    fn drop(&mut self) {
        if let Some((l, u)) = self.user.take() {
            let mut g = l.users.lock().expect("users lock");
            if let Some(n) = g.get_mut(&u) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    g.remove(&u);
                }
            }
        }
    }
}

pub fn normalize_role(role: Option<&str>) -> String {
    match role.unwrap_or("visitor").trim().to_lowercase().as_str() {
        "admin" | "server_admin" | "corpus_admin" | "superuser" => "admin".to_string(),
        "user" | "logged_in" | "registered" | "member" | "editor" => "user".to_string(),
        _ => "visitor".to_string(),
    }
}

impl Limits {
    /// No file: no tiers, no admission; the role is believed (legacy).
    pub fn none(jwt_secret: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            config: json!({}),
            default_tier: String::new(),
            tier_names: Vec::new(),
            tiers: HashMap::new(),
            global: None,
            global_n: 0,
            jwt_secret: jwt_secret.filter(|s| !s.is_empty()).map(String::into_bytes),
            users: Mutex::new(HashMap::new()),
            process: Arc::new(Semaphore::new(default_process_slots())),
            process_n: default_process_slots(),
            process_queue: Duration::from_secs(30),
        })
    }

    pub fn load(path: Option<&Path>, jwt_secret: Option<String>) -> Result<Arc<Self>> {
        let Some(path) = path else { return Ok(Self::none(jwt_secret)) };
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Self::from_value(config, jwt_secret)
    }

    pub fn from_value(config: Value, jwt_secret: Option<String>) -> Result<Arc<Self>> {
        let mut tiers = HashMap::new();
        let mut tier_names = Vec::new();
        if let Some(obj) = config.get("tiers").and_then(Value::as_object) {
            for (name, t) in obj {
                let slots_n = t.get("slots").and_then(Value::as_u64).unwrap_or(0) as usize;
                tiers.insert(
                    name.clone(),
                    TierAdmission {
                        slots: (slots_n > 0).then(|| Arc::new(Semaphore::new(slots_n))),
                        slots_n,
                        per_user: t.get("per_user").and_then(Value::as_u64).unwrap_or(0) as usize,
                        queue: Duration::from_millis(t.get("queue_ms").and_then(Value::as_u64).unwrap_or(5000)),
                    },
                );
                tier_names.push(name.clone());
            }
        }
        let default_tier = config
            .get("default_tier")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| if tiers.contains_key("visitor") { "visitor".into() } else { String::new() });
        let global_n = config.get("heavy_slots").and_then(Value::as_u64).unwrap_or(0) as usize;
        let process_n = config
            .get("process_slots")
            .and_then(Value::as_u64)
            .map(|n| n.max(1) as usize)
            .unwrap_or_else(default_process_slots);
        let process_queue =
            Duration::from_millis(config.get("process_queue_ms").and_then(Value::as_u64).unwrap_or(30000));
        Ok(Arc::new(Self {
            config,
            default_tier,
            tier_names,
            tiers,
            global: (global_n > 0).then(|| Arc::new(Semaphore::new(global_n))),
            global_n,
            jwt_secret: jwt_secret.filter(|s| !s.is_empty()).map(String::into_bytes),
            users: Mutex::new(HashMap::new()),
            process: Arc::new(Semaphore::new(process_n)),
            process_n,
            process_queue,
        }))
    }

    /// A slot for a request that runs as a child process (see "Processes" above),
    /// held until the process is done; queues up to `process_queue_ms`.
    pub async fn admit_process(&self, tier: &str) -> Result<OwnedSemaphorePermit, (StatusCode, String)> {
        match tokio::time::timeout(self.process_queue, Arc::clone(&self.process).acquire_owned()).await {
            Ok(Ok(p)) => Ok(p),
            _ => Err((
                StatusCode::SERVICE_UNAVAILABLE,
                busy_json("the server is busy with other searches; try again shortly", tier, 5),
            )),
        }
    }

    pub fn has_tiers(&self) -> bool {
        !self.tiers.is_empty()
    }

    pub fn has_jwt(&self) -> bool {
        self.jwt_secret.is_some()
    }

    /// The loaded limits JSON (tiers, `pando`, slots) — never includes the JWT secret.
    pub fn file_config(&self) -> &Value {
        &self.config
    }

    /// Options for `flexicorp_pando_open_opts`: the file's `pando` object plus the
    /// tiers (FQS-only members are ignored by the engine), trusted tier.
    pub fn engine_options(&self) -> Option<String> {
        Some(self.engine_options_value(&Value::Null).to_string())
    }

    /// The same for one corpus: its catalog `settings.limits` override the file —
    /// `{"tiers": {"visitor": {"max_count_hits": 500000, …}}, "pando": {"query_threads": 8}}`
    /// (members of a tier are replaced one by one; a tier only named here is added).
    /// A 5-billion-token corpus can so have lower caps than the demo corpora.
    pub fn engine_options_for(&self, corpus_settings: &Value) -> String {
        self.engine_options_value(corpus_settings.get("limits").unwrap_or(&Value::Null)).to_string()
    }

    fn engine_options_value(&self, over: &Value) -> Value {
        let mut o = self.config.get("pando").and_then(Value::as_object).cloned().unwrap_or_default();
        if let Some(p) = over.get("pando").and_then(Value::as_object) {
            for (k, v) in p {
                o.insert(k.clone(), v.clone());
            }
        }
        let mut tiers = self.config.get("tiers").and_then(Value::as_object).cloned().unwrap_or_default();
        if let Some(t) = over.get("tiers").and_then(Value::as_object) {
            for (name, members) in t {
                let entry = tiers.entry(name.clone()).or_insert_with(|| json!({}));
                if let (Some(dst), Some(src)) = (entry.as_object_mut(), members.as_object()) {
                    for (k, v) in src {
                        dst.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        if !tiers.is_empty() {
            o.insert("tiers".into(), Value::Object(tiers));
            o.insert("default_tier".into(), json!(self.default_tier));
            o.insert("trust_tier".into(), json!(true));
        }
        o.insert("embedded_in".into(), json!(format!("fqs {}", env!("CARGO_PKG_VERSION"))));
        Value::Object(o)
    }

    /// Resolve role / tier / user key for a request.
    pub fn caller(
        &self,
        headers: &HeaderMap,
        request_role: Option<&str>,
        user_hint: Option<&str>,
        session_id: Option<&str>,
        client_ip: &str,
    ) -> Caller {
        let (role, sub, verified) = match &self.jwt_secret {
            Some(secret) => match bearer_claims(headers, secret) {
                Some(claims) => (
                    normalize_role(claims.get("role").and_then(Value::as_str)),
                    claims
                        .get("user")
                        .or_else(|| claims.get("uid"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    true,
                ),
                None => ("visitor".to_string(), None, false),
            },
            None => (normalize_role(request_role), None, false),
        };
        let tier = if self.tiers.contains_key(&role) { role.clone() } else { self.default_tier.clone() };
        let user = sub
            .or_else(|| if self.jwt_secret.is_none() { user_hint.map(str::to_string) } else { None })
            .filter(|s| !s.is_empty())
            .or_else(|| session_id.filter(|s| !s.is_empty()).map(|s| format!("session:{s}")))
            .unwrap_or_else(|| format!("ip:{client_ip}"));
        Caller { role, tier, user, verified }
    }

    /// Verified HS256 claims from `Authorization: Bearer` (signature + optional exp).
    /// Used by the admin gate, which applies stricter aud/iat/exp rules on top.
    pub fn verified_bearer_claims(&self, headers: &HeaderMap) -> Option<Map<String, Value>> {
        bearer_claims(headers, self.jwt_secret.as_ref()?)
    }

    /// Admission for a heavy request of `caller`.
    pub async fn admit(self: &Arc<Self>, caller: &Caller) -> Result<AdmitPermit, (StatusCode, String)> {
        let t = self.tiers.get(&caller.tier).cloned();
        let per_user = t.as_ref().map(|t| t.per_user).unwrap_or(0);
        let queue = t.as_ref().map(|t| t.queue).unwrap_or(Duration::from_secs(5));
        let mut permit = AdmitPermit { _tier: None, _global: None, user: None };
        if per_user > 0 {
            let mut g = self.users.lock().expect("users lock");
            let n = g.entry(caller.user.clone()).or_insert(0);
            if *n >= per_user {
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    busy_json(&format!(
                        "you already have {n} heavy request(s) running (limit {per_user} for {} users); try again when they finish",
                        caller.tier
                    ), &caller.tier, 2),
                ));
            }
            *n += 1;
            permit.user = Some((Arc::clone(self), caller.user.clone()));
        }
        let deadline = tokio::time::Instant::now() + queue;
        if let Some(sem) = t.as_ref().and_then(|t| t.slots.clone()) {
            match tokio::time::timeout_at(deadline, sem.acquire_owned()).await {
                Ok(Ok(p)) => permit._tier = Some(p),
                _ => {
                    return Err((
                        StatusCode::SERVICE_UNAVAILABLE,
                        busy_json(&format!("the server is busy with {} requests; try again shortly", caller.tier), &caller.tier, 5),
                    ))
                }
            }
        }
        if let Some(sem) = self.global.clone() {
            match tokio::time::timeout_at(deadline, sem.acquire_owned()).await {
                Ok(Ok(p)) => permit._global = Some(p),
                _ => {
                    return Err((
                        StatusCode::SERVICE_UNAVAILABLE,
                        busy_json("the server is busy; try again shortly", &caller.tier, 5),
                    ))
                }
            }
        }
        Ok(permit)
    }

    pub fn status_json(&self) -> Value {
        let mut tiers = Map::new();
        for name in &self.tier_names {
            let t = &self.tiers[name];
            tiers.insert(
                name.clone(),
                json!({
                    "slots": t.slots_n,
                    "slots_free": t.slots.as_ref().map(|s| s.available_permits()),
                    "per_user": t.per_user,
                    "queue_ms": t.queue.as_millis() as u64,
                    "engine": self.config["tiers"][name].clone(),
                }),
            );
        }
        let busy_users = self.users.lock().map(|g| g.len()).unwrap_or(0);
        json!({
            "tiers": tiers,
            "default_tier": self.default_tier,
            "heavy_slots": self.global_n,
            "heavy_slots_free": self.global.as_ref().map(|s| s.available_permits()),
            "users_running_heavy": busy_users,
            "process_slots": self.process_n,
            "process_slots_free": self.process.available_permits(),
            "process_queue_ms": self.process_queue.as_millis() as u64,
            "role_trust": if self.jwt_secret.is_some() { "jwt" } else { "unverified" },
        })
    }
}

fn busy_json(msg: &str, tier: &str, retry_s: u64) -> String {
    json!({"ok": false, "error": msg, "busy": true, "tier": tier, "retry_after_s": retry_s}).to_string()
}

fn b64url(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .ok()
}

/// The claims of a valid HS256 `Authorization: Bearer` token (signature, `exp`).
pub fn bearer_claims(headers: &HeaderMap, secret: &[u8]) -> Option<Map<String, Value>> {
    let auth = headers.get("authorization")?.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer "))?.trim();
    let mut parts = token.split('.');
    let (h, p, s) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let header: Value = serde_json::from_slice(&b64url(h)?).ok()?;
    if header.get("alg").and_then(Value::as_str) != Some("HS256") {
        return None;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).ok()?;
    mac.update(format!("{h}.{p}").as_bytes());
    mac.verify_slice(&b64url(s)?).ok()?;
    let claims: Value = serde_json::from_slice(&b64url(p)?).ok()?;
    let claims = claims.as_object()?.clone();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    if let Some(exp) = claims.get("exp").and_then(Value::as_i64) {
        if now > exp {
            return None;
        }
    }
    Some(claims)
}

/// Heavy = may count or aggregate every hit: a /run program, a query with a
/// program (`;`). Pages (async totals run in the engine's own bounded pool) are light.
pub fn query_is_heavy(query: &str) -> bool {
    query.contains(';')
}

/// Replace any client-supplied engine tier with the caller's.
pub fn set_engine_tier(body: &mut Value, caller: &Caller, limits: &Limits) {
    if let Some(o) = body.as_object_mut() {
        o.remove("tier");
        if limits.has_tiers() && !caller.tier.is_empty() {
            o.insert("tier".into(), json!(caller.tier));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(secret: &str, claims: Value) -> String {
        let e = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let h = e(br#"{"alg":"HS256","typ":"JWT"}"#);
        let p = e(claims.to_string().as_bytes());
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{h}.{p}").as_bytes());
        format!("{h}.{p}.{}", e(&mac.finalize().into_bytes()))
    }

    fn headers(tok: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {tok}").parse().unwrap());
        h
    }

    fn limits(secret: Option<&str>) -> Arc<Limits> {
        Limits::from_value(
            json!({"heavy_slots": 2, "tiers": {"visitor": {"slots": 1, "per_user": 1, "queue_ms": 50},
                                                "user": {"per_user": 2}, "admin": {}}}),
            secret.map(str::to_string),
        )
        .unwrap()
    }

    #[test]
    fn roles_from_jwt_only_when_secret() {
        let l = limits(Some("s3cret"));
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let good = token("s3cret", json!({"role": "admin", "exp": now + 60, "user": "maarten"}));
        let c = l.caller(&headers(&good), Some("visitor"), None, None, "1.2.3.4");
        assert_eq!((c.role.as_str(), c.tier.as_str(), c.user.as_str(), c.verified), ("admin", "admin", "maarten", true));
        let forged = token("wrong", json!({"role": "admin", "exp": now + 60}));
        let c = l.caller(&headers(&forged), Some("admin"), None, None, "1.2.3.4");
        assert_eq!((c.role.as_str(), c.tier.as_str()), ("visitor", "visitor"));
        let expired = token("s3cret", json!({"role": "admin", "exp": now - 1}));
        assert_eq!(l.caller(&headers(&expired), None, None, None, "x").role, "visitor");
        let c = l.caller(&HeaderMap::new(), Some("admin"), None, Some("abc"), "x");
        assert_eq!((c.role.as_str(), c.user.as_str()), ("visitor", "session:abc"));
        let open = limits(None);
        let c = open.caller(&HeaderMap::new(), Some("logged_in"), Some("u1"), None, "x");
        assert_eq!((c.role.as_str(), c.tier.as_str(), c.user.as_str(), c.verified), ("user", "user", "u1", false));
    }

    #[test]
    fn per_corpus_overrides() {
        let l = limits(None);
        let o: Value = serde_json::from_str(&l.engine_options_for(&json!({
            "limits": {"tiers": {"visitor": {"max_count_hits": 7}, "guest": {"timeout_ms": 5}},
                       "pando": {"query_threads": 8}}}))).unwrap();
        assert_eq!(o["tiers"]["visitor"]["max_count_hits"], 7);
        assert_eq!(o["tiers"]["visitor"]["per_user"], 1);          // kept from the file
        assert_eq!(o["tiers"]["guest"]["timeout_ms"], 5);
        assert_eq!(o["query_threads"], 8);
        assert_eq!(o["trust_tier"], true);
        let plain: Value = serde_json::from_str(&l.engine_options_for(&json!({}))).unwrap();
        assert!(plain["tiers"]["visitor"].get("max_count_hits").is_none());
    }

    #[tokio::test]
    async fn admission_per_user_and_slots() {
        let l = limits(None);
        let v1 = Caller { role: "visitor".into(), tier: "visitor".into(), user: "a".into(), verified: false };
        let v2 = Caller { user: "b".into(), ..v1.clone() };
        let p1 = l.admit(&v1).await.expect("first visitor");
        assert_eq!(l.admit(&v1).await.err().map(|e| e.0), Some(StatusCode::TOO_MANY_REQUESTS));
        // the one visitor slot is taken: another visitor waits queue_ms, then 503
        assert_eq!(l.admit(&v2).await.err().map(|e| e.0), Some(StatusCode::SERVICE_UNAVAILABLE));
        drop(p1);
        let p2 = l.admit(&v2).await.expect("slot free again");
        let u = Caller { role: "user".into(), tier: "user".into(), user: "c".into(), verified: false };
        let p3 = l.admit(&u).await.expect("user uses the second global slot");
        // global heavy_slots (2) exhausted: admin waits its default queue (5 s)… use a short check
        let a = Caller { role: "admin".into(), tier: "admin".into(), user: "d".into(), verified: false };
        let t0 = std::time::Instant::now();
        let r = tokio::time::timeout(Duration::from_millis(200), l.admit(&a)).await;
        assert!(r.is_err() && t0.elapsed() < Duration::from_secs(1), "admin waits for a global slot");
        drop((p2, p3));
        assert!(l.admit(&a).await.is_ok());
    }
}
