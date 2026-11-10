//! `/corpora?permitted=1` and `/corpora/{id}/permitted` over HTTP (access.rs): a real `fqs
//! serve` on a small catalogue, asked with signed tokens carrying groups, in each access mode, and
//! once without a secret (nothing unsigned is believed). `cargo test --test access_permitted`.

use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SECRET: &str = "test-secret";
const BIN: &str = env!("CARGO_BIN_EXE_fqs");

struct Server {
    child: Child,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn workdir(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("access-permitted-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A catalogue: an open corpus, one for `acad`/`cnc`, one for any signed-in user, one admin-only.
fn catalogue(dir: &Path) -> PathBuf {
    let db = dir.join("fqs.db");
    let root = dir.to_string_lossy().to_string();
    let row = |id: &str, extra: Value| {
        let mut r = json!({"id": id, "label": id, "project_root": root, "preferred_backend": "pando"});
        r.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        r
    };
    let rows = json!([
        row("open1", json!({})),
        row("acad1", json!({"settings": {"access": ["acad", "cnc"], "description": "d"}})),
        row("sso1", json!({"settings": {"access": "authenticated"}})),
        row("private1", json!({"http_policy_mode": "auth_required"})),
    ]);
    let out = Command::new(BIN)
        .args(["corpora", "upsert-json", "--db"])
        .arg(&db)
        .args(["--json", &rows.to_string()])
        .output()
        .unwrap();
    assert!(out.status.success(), "upsert-json: {}", String::from_utf8_lossy(&out.stderr));
    db
}

fn serve(dir: &Path, db: &Path, mode: &str, secret: Option<&str>) -> Server {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut cmd = Command::new(BIN);
    cmd.arg("serve")
        .arg("--db")
        .arg(db)
        .args(["--port", &port.to_string(), "--no-reindex-worker", "--log-file"])
        .arg(dir.join(format!("fqs-{mode}.log")))
        .env("FQS_ACCESS_MODE", mode)
        .env_remove("FQS_SECRET")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(s) = secret {
        cmd.args(["--jwt-secret", s]);
    }
    let server = Server { child: cmd.spawn().unwrap(), url: format!("http://127.0.0.1:{port}") };
    let t0 = Instant::now();
    while ureq::get(&format!("{}/health", server.url)).call().is_err() {
        assert!(t0.elapsed() < Duration::from_secs(20), "fqs serve did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    server
}

fn token(secret: &str, role: &str, groups: &[&str]) -> String {
    let e = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let claims = json!({"iss": "kontext-lindat", "aud": "fqs", "sub": "jan", "user": "jan", "role": role, "groups": groups, "exp": now + 120});
    let h = e(br#"{"alg":"HS256","typ":"JWT"}"#);
    let p = e(claims.to_string().as_bytes());
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    format!("{h}.{p}.{}", e(&mac.finalize().into_bytes()))
}

fn get(url: &str, tok: Option<&str>) -> (u16, Value) {
    let mut req = ureq::get(url);
    if let Some(t) = tok {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.call() {
        Ok(r) => (r.status(), r.into_json().unwrap()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_json().unwrap_or(Value::Null)),
        Err(e) => panic!("{url}: {e}"),
    }
}

fn permitted_ids(s: &Server, tok: Option<&str>, extra: &str) -> Vec<String> {
    let (code, body) = get(&format!("{}/corpora?permitted=1{extra}", s.url), tok);
    assert_eq!(code, 200);
    let rows = body["corpora"].as_array().unwrap();
    assert!(rows.iter().all(|r| r["permitted"] == true), "every row says permitted: {body}");
    let mut ids: Vec<String> = rows.iter().map(|r| r["id"].as_str().unwrap().to_string()).collect();
    ids.sort();
    ids
}

fn one(s: &Server, id: &str, tok: Option<&str>) -> bool {
    let (code, body) = get(&format!("{}/corpora/{id}/permitted", s.url), tok);
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["corpus"], id);
    body["permitted"].as_bool().unwrap()
}

#[test]
fn permitted_in_every_mode_with_signed_groups() {
    let dir = workdir("secret");
    let db = catalogue(&dir);
    let member = token(SECRET, "user", &["cnc"]);
    let other = token(SECRET, "user", &["students"]);
    let admin = token(SECRET, "admin", &[]);
    let forged = token("not-the-secret", "admin", &["acad"]);
    for mode in ["off", "report", "enforce"] {
        let s = serve(&dir, &db, mode, Some(SECRET));
        assert_eq!(permitted_ids(&s, None, ""), ["open1"], "{mode}: anonymous");
        assert_eq!(permitted_ids(&s, Some(&forged), ""), ["open1"], "{mode}: forged token is anonymous");
        assert_eq!(permitted_ids(&s, Some(&other), ""), ["open1", "sso1"], "{mode}: signed in, other group");
        assert_eq!(permitted_ids(&s, Some(&member), ""), ["acad1", "open1", "sso1"], "{mode}: member");
        assert_eq!(permitted_ids(&s, Some(&admin), ""), ["acad1", "open1", "private1", "sso1"], "{mode}: admin");
        // other filters still apply
        assert_eq!(permitted_ids(&s, Some(&member), "&q=acad"), ["acad1"], "{mode}: q=");

        assert!(!one(&s, "acad1", None), "{mode}");
        assert!(!one(&s, "acad1", Some(&other)), "{mode}");
        assert!(one(&s, "acad1", Some(&member)), "{mode}");
        assert!(!one(&s, "sso1", None), "{mode}");
        assert!(one(&s, "sso1", Some(&other)), "{mode}");
        assert!(one(&s, "open1", None), "{mode}");
        assert!(!one(&s, "private1", Some(&member)), "{mode}");
        assert!(one(&s, "private1", Some(&admin)), "{mode}");
        let (code, body) = get(&format!("{}/corpora/nope/permitted", s.url), Some(&member));
        assert_eq!((code, body["ok"].as_bool()), (404, Some(false)), "{mode}");

        // the groups of a corpus: for admins only
        let (_, body) = get(&format!("{}/corpora?permitted=1", s.url), Some(&member));
        let acad = body["corpora"].as_array().unwrap().iter().find(|r| r["id"] == "acad1").unwrap().clone();
        assert!(acad["settings"].get("access").is_none(), "{mode}: access hidden from a user");
        assert_eq!(acad["settings"]["description"], "d");
        let (_, body) = get(&format!("{}/corpora?permitted=1", s.url), Some(&admin));
        let acad = body["corpora"].as_array().unwrap().iter().find(|r| r["id"] == "acad1").unwrap().clone();
        assert_eq!(acad["settings"]["access"], json!(["acad", "cnc"]), "{mode}: access shown to an admin");

        // the plain list is unchanged: no `permitted` field, mode-dependent as before
        let (_, body) = get(&format!("{}/corpora", s.url), None);
        let rows = body["corpora"].as_array().unwrap();
        assert!(rows.iter().all(|r| r.get("permitted").is_none()));
        let listed = rows.iter().any(|r| r["id"] == "acad1");
        assert_eq!(listed, mode != "enforce", "{mode}: /corpora lists acad1 unless enforced");
    }
}

#[test]
fn without_secret_nothing_unsigned_counts() {
    let dir = workdir("nosecret");
    let db = catalogue(&dir);
    let s = serve(&dir, &db, "off", None);
    // a token FQS cannot verify, and request_role, give the caller nothing
    let unverifiable = token(SECRET, "user", &["acad"]);
    assert_eq!(permitted_ids(&s, Some(&unverifiable), ""), ["open1"]);
    assert_eq!(permitted_ids(&s, None, "&request_role=admin"), ["open1"]);
    assert!(!one(&s, "acad1", Some(&unverifiable)));
    assert!(!one(&s, "sso1", None));
    assert!(one(&s, "open1", None));
}
