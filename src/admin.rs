//! FQS Admin HTTP: JWT-admin gate (`aud=fqs-admin`, short `exp`), static UI, JSON errors.
//!
//! API handlers live in `main.rs` and call [`require_admin`]. Static files under
//! `fqs/admin/` are served at `/admin/` when `--enable-admin-http` is set.

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde_json::{json, Map, Value};
use sha2::Sha256;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::limits::{normalize_role, Caller, Limits};

/// Audience claim required on tokens accepted by the admin API (not TEITOK query JWTs).
pub const FQS_ADMIN_AUD: &str = "fqs-admin";

/// Maximum admin token lifetime (`exp - iat`), seconds (4 hours).
pub const ADMIN_TOKEN_MAX_TTL_SECS: i64 = 4 * 3600;

/// Admin API / static error with `application/json` body.
#[derive(Debug)]
pub struct AdminError {
    status: StatusCode,
    body: Value,
}

impl AdminError {
    pub fn new(status: StatusCode, body: Value) -> Self {
        Self { status, body }
    }

    pub fn msg(status: StatusCode, error: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({"ok": false, "error": error.into()}),
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let mut res = (
            self.status,
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            self.body.to_string(),
        )
            .into_response();
        apply_api_security_headers(res.headers_mut());
        res
    }
}

/// Headers for admin API JSON (catalog / diagnostics — do not cache or sniff).
pub fn apply_api_security_headers(headers: &mut axum::http::HeaderMap) {
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
}

pub type AdminResult<T> = Result<T, AdminError>;

/// Require a verified admin JWT with `aud: "fqs-admin"`, required `iat`/`exp`, and TTL ≤ 4h.
pub fn require_admin(limits: &Limits, headers: &HeaderMap) -> AdminResult<Caller> {
    let Some(claims) = limits.verified_bearer_claims(headers) else {
        return Err(AdminError::msg(
            StatusCode::UNAUTHORIZED,
            "admin API requires Authorization: Bearer <HS256 JWT> signed with FQS_SECRET \
             (claims: role=admin, aud=fqs-admin, iat, exp; max TTL 4h)",
        ));
    };

    let role = normalize_role(claims.get("role").and_then(Value::as_str));
    if role != "admin" {
        return Err(AdminError::msg(
            StatusCode::FORBIDDEN,
            format!("admin API requires role admin (got '{role}')"),
        ));
    }

    if !aud_allows_fqs_admin(&claims) {
        return Err(AdminError::msg(
            StatusCode::FORBIDDEN,
            format!(
                "admin API requires aud \"{FQS_ADMIN_AUD}\" (TEITOK/query tokens are not accepted here)"
            ),
        ));
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let Some(exp) = claims.get("exp").and_then(Value::as_i64) else {
        return Err(AdminError::msg(
            StatusCode::UNAUTHORIZED,
            "admin JWT must include exp",
        ));
    };
    if now > exp {
        return Err(AdminError::msg(
            StatusCode::UNAUTHORIZED,
            "admin JWT expired",
        ));
    }
    let Some(iat) = claims.get("iat").and_then(Value::as_i64) else {
        return Err(AdminError::msg(
            StatusCode::UNAUTHORIZED,
            "admin JWT must include iat",
        ));
    };
    if iat > now + 60 {
        return Err(AdminError::msg(
            StatusCode::UNAUTHORIZED,
            "admin JWT iat is in the future",
        ));
    }
    if exp < iat {
        return Err(AdminError::msg(
            StatusCode::UNAUTHORIZED,
            "admin JWT exp must be after iat",
        ));
    }
    if exp - iat > ADMIN_TOKEN_MAX_TTL_SECS {
        return Err(AdminError::msg(
            StatusCode::UNAUTHORIZED,
            format!(
                "admin JWT TTL (exp-iat) exceeds maximum of {}s ({}h)",
                ADMIN_TOKEN_MAX_TTL_SECS,
                ADMIN_TOKEN_MAX_TTL_SECS / 3600
            ),
        ));
    }

    let user = claims
        .get("user")
        .or_else(|| claims.get("uid"))
        .or_else(|| claims.get("sub"))
        .and_then(Value::as_str)
        .unwrap_or("admin")
        .to_string();

    Ok(Caller {
        role,
        tier: "admin".into(),
        user,
        verified: true,
    })
}

fn aud_allows_fqs_admin(claims: &Map<String, Value>) -> bool {
    match claims.get("aud") {
        Some(Value::String(s)) => s == FQS_ADMIN_AUD,
        Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(FQS_ADMIN_AUD)),
        _ => false,
    }
}

/// Mint an HS256 admin token for the admin API (`aud=fqs-admin`, clamped TTL).
pub fn mint_admin_token(secret: &str, user: &str, ttl_secs: u64) -> Result<String, String> {
    if secret.trim().is_empty() {
        return Err("JWT secret is empty".into());
    }
    let ttl = (ttl_secs as i64)
        .clamp(60, ADMIN_TOKEN_MAX_TTL_SECS);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs() as i64;
    let claims = json!({
        "role": "admin",
        "aud": FQS_ADMIN_AUD,
        "user": user,
        "iat": now,
        "exp": now + ttl,
    });
    sign_hs256(secret, &claims)
}

fn sign_hs256(secret: &str, claims: &Value) -> Result<String, String> {
    let e = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let h = e(br#"{"alg":"HS256","typ":"JWT"}"#);
    let p = e(claims.to_string().as_bytes());
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|e| e.to_string())?;
    mac.update(format!("{h}.{p}").as_bytes());
    Ok(format!("{h}.{p}.{}", e(&mac.finalize().into_bytes())))
}

/// Resolve the directory that holds `index.html` / `app.js` for the admin UI.
///
/// Order: explicit `--admin-dir` / `FQS_ADMIN_DIR`, else `admin/` next to the
/// running binary, else `admin/` under the current working directory, else
/// `CARGO_MANIFEST_DIR/admin` (dev builds).
pub fn resolve_admin_dir(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let candidate = parent.join("admin");
            if candidate.join("index.html").is_file() {
                return candidate;
            }
        }
    }
    let cwd = PathBuf::from("admin");
    if cwd.join("index.html").is_file() {
        return cwd;
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("admin");
    if manifest.join("index.html").is_file() {
        return manifest;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("admin")
}

fn admin_security_headers() -> [(header::HeaderName, HeaderValue); 3] {
    [
        (
            header::HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                 frame-ancestors 'none'; base-uri 'self'; form-action 'self'",
            ),
        ),
        (
            header::HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        ),
        (
            header::HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ),
    ]
}

pub fn static_response_with_base(
    admin_dir: &Path,
    rel: &str,
    base_href: Option<&str>,
) -> Response {
    let safe = rel.trim_start_matches('/').replace('\\', "/");
    if safe.is_empty() || safe.contains("..") || safe.starts_with('/') {
        return AdminError::msg(StatusCode::BAD_REQUEST, "bad path").into_response();
    }
    let path = admin_dir.join(&safe);
    match std::fs::read(&path) {
        Ok(mut bytes) => {
            if safe == "index.html" {
                if let Some(base) = normalize_admin_base_href(base_href) {
                    bytes = inject_base_href(&bytes, &base);
                }
            }
            let ctype = content_type(&safe);
            let mut res = (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, HeaderValue::from_static(ctype)),
                    (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
                ],
                bytes,
            )
                .into_response();
            for (name, value) in admin_security_headers() {
                res.headers_mut().insert(name, value);
            }
            res
        }
        Err(_) => AdminError::new(
            StatusCode::NOT_FOUND,
            json!({
                "ok": false,
                "error": format!(
                    "admin static file not found: {} (admin_dir={})",
                    safe,
                    admin_dir.display()
                ),
            }),
        )
        .into_response(),
    }
}

/// Ensure a public admin base path ends with `/` and is a safe same-origin path.
pub fn normalize_admin_base_href(raw: Option<&str>) -> Option<String> {
    let s = raw.map(str::trim).filter(|s| !s.is_empty())?;
    if s.contains("://") || s.contains('\n') || s.contains('<') || s.contains('"') {
        return None;
    }
    let mut out = s.to_string();
    if !out.starts_with('/') {
        out.insert(0, '/');
    }
    if !out.ends_with('/') {
        out.push('/');
    }
    Some(out)
}

fn inject_base_href(html: &[u8], base: &str) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(html) else {
        return html.to_vec();
    };
    if text.contains("<base ") {
        return html.to_vec();
    }
    let tag = format!(r#"<base href="{base}" />"#);
    if let Some(i) = text.find("<head>") {
        let mut out = String::with_capacity(text.len() + tag.len() + 1);
        out.push_str(&text[..=i + 5]); // include `<head>`
        out.push('\n');
        out.push_str("  ");
        out.push_str(&tag);
        out.push_str(&text[i + 6..]);
        return out.into_bytes();
    }
    if let Some(i) = text.find("<head ") {
        if let Some(end) = text[i..].find('>') {
            let at = i + end + 1;
            let mut out = String::with_capacity(text.len() + tag.len() + 1);
            out.push_str(&text[..at]);
            out.push('\n');
            out.push_str("  ");
            out.push_str(&tag);
            out.push_str(&text[at..]);
            return out.into_bytes();
        }
    }
    html.to_vec()
}

fn content_type(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// Parse `host:port` for `--admin-bind` / `FQS_ADMIN_BIND`.
pub fn parse_admin_bind(spec: &str) -> Result<(String, u16), String> {
    let s = spec.trim();
    if s.is_empty() {
        return Err("empty admin-bind".into());
    }
    // IPv6: [addr]:port
    if let Some(rest) = s.strip_prefix('[') {
        let (host, port_part) = rest
            .split_once("]:")
            .ok_or_else(|| "admin-bind IPv6 must look like [addr]:port".to_string())?;
        let port: u16 = port_part
            .parse()
            .map_err(|_| format!("invalid admin-bind port in '{s}'"))?;
        return Ok((format!("[{host}]"), port));
    }
    let (host, port_s) = s
        .rsplit_once(':')
        .ok_or_else(|| "admin-bind must be host:port".to_string())?;
    let port: u16 = port_s
        .parse()
        .map_err(|_| format!("invalid admin-bind port in '{s}'"))?;
    if host.is_empty() {
        return Err("admin-bind host is empty".into());
    }
    Ok((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::Limits;

    fn token(secret: &str, claims: Value) -> String {
        sign_hs256(secret, &claims).unwrap()
    }

    #[test]
    fn admin_requires_aud_iat_exp() {
        let limits = Limits::none(Some("s3cret".into()));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut h = HeaderMap::new();
        assert_eq!(
            require_admin(&limits, &h).err().map(|e| e.status),
            Some(StatusCode::UNAUTHORIZED)
        );

        // TEITOK-style admin token without aud — rejected for admin API
        let teitok = token(
            "s3cret",
            json!({"role": "admin", "iat": now, "exp": now + 60, "user": "ops"}),
        );
        h.insert("authorization", format!("Bearer {teitok}").parse().unwrap());
        assert_eq!(
            require_admin(&limits, &h).err().map(|e| e.status),
            Some(StatusCode::FORBIDDEN)
        );

        // TTL too long
        let long = token(
            "s3cret",
            json!({
                "role": "admin",
                "aud": FQS_ADMIN_AUD,
                "iat": now,
                "exp": now + ADMIN_TOKEN_MAX_TTL_SECS + 1,
                "user": "ops"
            }),
        );
        h.insert("authorization", format!("Bearer {long}").parse().unwrap());
        assert_eq!(
            require_admin(&limits, &h).err().map(|e| e.status),
            Some(StatusCode::UNAUTHORIZED)
        );

        let admin = mint_admin_token("s3cret", "ops", 3600).unwrap();
        h.insert("authorization", format!("Bearer {admin}").parse().unwrap());
        let c = require_admin(&limits, &h).unwrap();
        assert_eq!(c.role, "admin");
        assert_eq!(c.user, "ops");
    }

    #[test]
    fn parse_admin_bind_ok() {
        assert_eq!(
            parse_admin_bind("127.0.0.1:8790").unwrap(),
            ("127.0.0.1".into(), 8790)
        );
    }

    #[test]
    fn inject_base_href_after_head() {
        let html = b"<html><head>\n<title>x</title></head></html>";
        let out = inject_base_href(html, "/services/test-kontext/fqsadmin/");
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#"<base href="/services/test-kontext/fqsadmin/" />"#));
        assert!(normalize_admin_base_href(Some("services/foo")).unwrap().ends_with('/'));
        assert!(normalize_admin_base_href(Some("http://evil")).is_none());
    }
}
