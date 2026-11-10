//! "Which corpora may this caller use?" for front-ends that take their corpus permissions from FQS
//! (lindat-corpora/fqs-authorization.md §8: KonText's corpus list and `corpus_access`).
//!
//! The answer is the group rule of access.rs (`settings.access` against the caller's groups), asked
//! **whatever the access mode** (off / report / enforce, server-wide or per corpus): the mode decides
//! what FQS itself refuses, this answer is what a front-end that relies on it refuses. So FQS can stay
//! in `report` for its other clients while KonText enforces from this answer.
//!
//! Before the groups, the corpus's http policy for the caller (`disabled`: nobody; `auth_required`:
//! admins) and its `query` operation, as the query gate has them: a corpus nobody may query is not
//! "permitted".
//!
//! The caller is only what a verified token says. Without one (no token, a bad or expired one, or no
//! FQS secret at all, when a request's `request_role` is otherwise believed) the caller is anonymous:
//! no groups, no `authenticated`, no admin bypass.

use crate::access::{self, Config};
use crate::limits::Caller;
use serde_json::Value;

/// The caller this answer is for: the verified token's, else an anonymous visitor.
pub fn effective_caller(caller: &Caller) -> Caller {
    if caller.verified {
        return caller.clone();
    }
    Caller { role: "visitor".into(), tier: caller.tier.clone(), user: caller.user.clone(), verified: false, groups: Vec::new(), owned_groups: Vec::new() }
}

/// The http policy of a corpus for a role (`http_policy_mode`), as the query gate applies it.
pub fn policy_allows(policy: &str, role: &str) -> bool {
    match policy {
        "disabled" => false,
        "auth_required" => role == "admin",
        _ => true,
    }
}

/// May `caller` use a corpus with this policy, `query` operation and settings? Mode-independent.
pub fn permitted(cfg: &Config, policy: &str, query_op: bool, settings: &Value, caller: &Caller) -> bool {
    let c = effective_caller(caller);
    query_op && policy_allows(policy, &c.role) && access::allowed(cfg, settings, &c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::Mode;
    use serde_json::json;

    fn caller(role: &str, verified: bool, groups: &[&str]) -> Caller {
        Caller { role: role.into(), tier: role.into(), user: "u".into(), verified, groups: groups.iter().map(|g| g.to_string()).collect(), owned_groups: Vec::new() }
    }

    fn cfg(mode: Mode) -> Config {
        Config { mode, admin_bypass: true }
    }

    const MODES: [Mode; 3] = [Mode::Off, Mode::Report, Mode::Enforce];

    #[test]
    fn without_access_list_open() {
        for m in MODES {
            assert!(permitted(&cfg(m), "public_query", true, &json!({}), &caller("visitor", false, &[])));
            assert!(permitted(&cfg(m), "public_query", true, &json!({"access": []}), &caller("visitor", false, &[])));
        }
    }

    #[test]
    fn groups_decide_in_every_mode() {
        let acad = json!({"access": ["acad", "cnc"]});
        let sso = json!({"access": "authenticated"});
        let anon = json!({"access": ["anonymous"]});
        for m in MODES {
            let c = cfg(m);
            assert!(!permitted(&c, "public_query", true, &acad, &caller("visitor", false, &[])), "{m:?}: anonymous, group list");
            assert!(!permitted(&c, "public_query", true, &acad, &caller("user", true, &["other"])), "{m:?}: other group");
            assert!(permitted(&c, "public_query", true, &acad, &caller("user", true, &["cnc"])), "{m:?}: member");
            assert!(!permitted(&c, "public_query", true, &sso, &caller("visitor", false, &[])), "{m:?}: not signed in");
            assert!(permitted(&c, "public_query", true, &sso, &caller("user", true, &[])), "{m:?}: signed in");
            assert!(permitted(&c, "public_query", true, &anon, &caller("visitor", false, &[])), "{m:?}: anonymous list");
            assert!(permitted(&c, "public_query", true, &acad, &caller("admin", true, &[])), "{m:?}: admin bypass");
            // a corpus switched off on its own (settings.access_mode) answers the same
            let early_off = json!({"access": ["acad"], "access_mode": "off"});
            assert!(!permitted(&c, "public_query", true, &early_off, &caller("user", true, &[])), "{m:?}: per-corpus off");
        }
        let strict = Config { mode: Mode::Report, admin_bypass: false };
        assert!(!permitted(&strict, "public_query", true, &acad, &caller("admin", true, &[])));
    }

    #[test]
    fn unverified_callers_are_anonymous() {
        let acad = json!({"access": ["acad"]});
        let sso = json!({"access": ["authenticated"]});
        let c = cfg(Mode::Off);
        // no secret: request_role is believed elsewhere, groups never; here neither counts
        assert!(!permitted(&c, "public_query", true, &acad, &caller("user", false, &["acad"])));
        assert!(!permitted(&c, "public_query", true, &sso, &caller("user", false, &[])));
        assert!(!permitted(&c, "public_query", true, &acad, &caller("admin", false, &[])));
        assert!(!permitted(&c, "auth_required", true, &json!({}), &caller("admin", false, &[])));
    }

    #[test]
    fn policy_and_query_first() {
        let c = cfg(Mode::Off);
        let open = json!({});
        assert!(!permitted(&c, "disabled", true, &open, &caller("admin", true, &[])));
        assert!(!permitted(&c, "auth_required", true, &open, &caller("user", true, &[])));
        assert!(permitted(&c, "auth_required", true, &open, &caller("admin", true, &[])));
        assert!(!permitted(&c, "public_query", false, &open, &caller("admin", true, &[])), "no query operation");
    }
}
