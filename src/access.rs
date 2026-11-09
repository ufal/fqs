//! Per-corpus access by group, as KonText has it (lindat-corpora/fqs-authorization.md): a corpus's
//! `settings.access` lists the groups that may use it (written by `fqs corpora import` from
//! KonText's corplist `access="…"`); the caller's groups come from the front-end, in its signed
//! token (`groups`). A caller may use the corpus when the two share a group.
//!
//! Caller groups: the verified token's `groups` (a list, or a comma-separated string), plus
//! `anonymous` for everyone and `authenticated` for a signed-in caller (role user / admin). Only a
//! verified token carries groups: without FQS_SECRET nothing a request says about itself is checked,
//! so groups are not taken from it either.
//!
//! The mode (`access.mode` in the limits file, env FQS_ACCESS_MODE; a corpus's
//! `settings.access_mode` overrides it, so one corpus can be switched on first):
//! - `off` (default): `access` is not looked at, as before;
//! - `report`: decided, and every "would refuse" is written to the activity log (event
//!   `access_report`: corpus, route, role, groups, user as the log writes users), but answered as
//!   before;
//! - `enforce`: refused (403), and left out of corpus lists and FCS.
//! An admin caller passes (`access.admin_bypass`, default true): FQS's admin is the server's
//! operator. A corpus without `access` is open, as before.

use crate::limits::Caller;
use serde_json::{json, Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    Report,
    Enforce,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "" => Some(Mode::Off),
            "report" => Some(Mode::Report),
            "enforce" => Some(Mode::Enforce),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Report => "report",
            Mode::Enforce => "enforce",
        }
    }
}

/// The server's setting: env FQS_ACCESS_MODE, else the limits file's `access.mode`, else off.
/// An unknown value counts as `report` (noticed in the log, refuses nothing).
#[derive(Clone, Debug)]
pub struct Config {
    pub mode: Mode,
    pub admin_bypass: bool,
}

impl Config {
    pub fn from(limits_file: &Value) -> Config {
        let a = limits_file.get("access");
        let raw = std::env::var("FQS_ACCESS_MODE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| a.and_then(|a| a.get("mode")).and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        let mode = Mode::parse(&raw).unwrap_or_else(|| {
            eprintln!("[fqs] access mode '{raw}' is not off, report or enforce: using report");
            Mode::Report
        });
        let admin_bypass = a.and_then(|a| a.get("admin_bypass")).and_then(Value::as_bool).unwrap_or(true);
        Config { mode, admin_bypass }
    }

    /// The mode for one corpus: its `settings.access_mode` when that is valid, else the server's.
    pub fn mode_for(&self, settings: &Value) -> Mode {
        settings.get("access_mode").and_then(Value::as_str).and_then(Mode::parse).unwrap_or(self.mode)
    }

    pub fn status_json(&self) -> Value {
        json!({"mode": self.mode.as_str(), "admin_bypass": self.admin_bypass})
    }
}

/// The groups a corpus is open to (`settings.access`), None when it does not say (open).
pub fn corpus_groups(settings: &Value) -> Option<Vec<String>> {
    let a = settings.get("access")?;
    let list: Vec<String> = match a {
        Value::Array(v) => v.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Value::String(s) => s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        _ => return None,
    };
    (!list.is_empty()).then_some(list)
}

/// The caller's groups: the token's, `anonymous`, and `authenticated` when signed in.
pub fn caller_groups(caller: &Caller) -> Vec<String> {
    let mut g = caller.groups.clone();
    g.push("anonymous".into());
    if caller.role == "user" || caller.role == "admin" {
        g.push("authenticated".into());
    }
    g.sort();
    g.dedup();
    g
}

/// May `caller` use a corpus with these settings? (The rule alone, whatever the mode.)
pub fn allowed(cfg: &Config, settings: &Value, caller: &Caller) -> bool {
    let Some(open_to) = corpus_groups(settings) else { return true };
    if cfg.admin_bypass && caller.role == "admin" {
        return true;
    }
    let mine = caller_groups(caller);
    open_to.iter().any(|g| mine.contains(g))
}

/// The groups of a token's `groups` claim (a list, or a comma-separated string).
pub fn groups_from_claim(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Some(Value::String(s)) => s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// The outcome of the check on one request.
pub enum Check {
    /// go on (allowed, mode off, or only reported)
    Pass,
    /// refuse, with this message
    Refuse(String),
}

/// Check one request to corpus `id` on `route`; a "would refuse" in report mode is written with `log`.
pub fn check(cfg: &Config, id: &str, settings: &Value, caller: &Caller, route: &str, log: impl FnOnce(Map<String, Value>)) -> Check {
    let mode = cfg.mode_for(settings);
    if mode == Mode::Off || allowed(cfg, settings, caller) {
        return Check::Pass;
    }
    let open_to = corpus_groups(settings).unwrap_or_default();
    if mode == Mode::Report {
        let mut f = Map::new();
        f.insert("corpus".into(), json!(id));
        f.insert("route".into(), json!(route));
        f.insert("role".into(), json!(caller.role));
        f.insert("groups".into(), json!(caller_groups(caller)));
        f.insert("access".into(), json!(open_to));
        f.insert("verified".into(), json!(caller.verified));
        f.insert("decision".into(), json!("would_refuse"));
        log(f);
        return Check::Pass;
    }
    Check::Refuse(format!("corpus '{id}' is restricted to {} (your groups: {})", open_to.join(", "), caller_groups(caller).join(", ")))
}

/// A corpus's settings as a non-admin sees them: without its groups (`access`) and `access_mode`.
pub fn hide_from_non_admin(settings: &mut Value) {
    if let Some(o) = settings.as_object_mut() {
        o.remove("access");
        o.remove("access_mode");
    }
}

/// Is the corpus listed for this caller (corpus lists, FCS)? Only enforce leaves corpora out.
pub fn listed(cfg: &Config, settings: &Value, caller: &Caller) -> bool {
    cfg.mode_for(settings) != Mode::Enforce || allowed(cfg, settings, caller)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(role: &str, groups: &[&str]) -> Caller {
        Caller { role: role.into(), tier: role.into(), user: "u".into(), verified: true, groups: groups.iter().map(|g| g.to_string()).collect(), owned_groups: Vec::new() }
    }

    #[test]
    fn the_rule() {
        let cfg = Config { mode: Mode::Enforce, admin_bypass: true };
        let open = json!({});
        let anon = json!({"access": ["anonymous"]});
        let signed = json!({"access": "authenticated"});
        let acad = json!({"access": ["acad", "cnc"]});
        assert!(allowed(&cfg, &open, &caller("visitor", &[])), "no access list: open");
        assert!(allowed(&cfg, &anon, &caller("visitor", &[])));
        assert!(!allowed(&cfg, &signed, &caller("visitor", &[])));
        assert!(allowed(&cfg, &signed, &caller("user", &[])));
        assert!(!allowed(&cfg, &acad, &caller("user", &["other"])));
        assert!(allowed(&cfg, &acad, &caller("user", &["cnc"])));
        assert!(allowed(&cfg, &acad, &caller("admin", &[])), "admin bypass");
        assert!(!allowed(&Config { admin_bypass: false, ..cfg.clone() }, &acad, &caller("admin", &[])));
    }

    #[test]
    fn modes() {
        let acad = json!({"access": ["acad"]});
        let v = caller("visitor", &[]);
        let mut logged = Vec::new();
        let report = Config { mode: Mode::Report, admin_bypass: true };
        assert!(matches!(check(&report, "c", &acad, &v, "/query", |f| logged.push(f)), Check::Pass));
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0]["decision"], "would_refuse");
        assert_eq!(logged[0]["groups"], json!(["anonymous"]));
        let enforce = Config { mode: Mode::Enforce, admin_bypass: true };
        assert!(matches!(check(&enforce, "c", &acad, &v, "/query", |_| panic!("no log")), Check::Refuse(_)));
        assert!(!listed(&enforce, &acad, &v));
        assert!(listed(&report, &acad, &v), "report mode hides nothing");
        // one corpus switched on before the rest
        let early = json!({"access": ["acad"], "access_mode": "enforce"});
        let off = Config { mode: Mode::Off, admin_bypass: true };
        assert!(matches!(check(&off, "c", &early, &v, "/query", |_| ()), Check::Refuse(_)));
        assert!(matches!(check(&off, "c", &acad, &v, "/query", |_| panic!("off")), Check::Pass));
    }

    #[test]
    fn hidden_settings() {
        let mut s = json!({"access": ["acad"], "access_mode": "enforce", "description": "d"});
        hide_from_non_admin(&mut s);
        assert_eq!(s, json!({"description": "d"}));
    }

    #[test]
    fn claims_and_modes_parse() {
        assert_eq!(groups_from_claim(Some(&json!(["a", " b ", ""]))), vec!["a", "b"]);
        assert_eq!(groups_from_claim(Some(&json!("a, b"))), vec!["a", "b"]);
        assert!(groups_from_claim(None).is_empty());
        assert_eq!(Mode::parse("Report"), Some(Mode::Report));
        assert_eq!(Mode::parse("nonsense"), None);
    }
}
