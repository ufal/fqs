//! Who may see and who may publish a stored subcorpus.
//!
//! Visibility, checked on every use: `private` (the owner only), `groups` (callers who share one
//! of its groups: the verified token's `groups`, plus `anonymous` / `authenticated` as for corpus
//! access, access.rs), `public` (everyone who may use the corpus). An admin sees all.
//!
//! Publishing (making one `groups` or `public`) is a server setting, not stored data, so changing
//! it needs no migration; a subcorpus published under a looser rule stays published until its
//! owner or an admin withdraws it. `subcorpora.publish` in the limits file, env
//! FQS_SUBCORPUS_PUBLISH overriding it:
//! - `admins` (default): only admins publish;
//! - `group_owners`: also a caller to groups their token says they own (claim `owned_groups`);
//!   `public` stays with admins;
//! - `any_user`: any signed-in caller (role user / admin), to groups they are in, or public.

use crate::access;
use crate::limits::Caller;
use anyhow::{bail, Result};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Publish {
    Admins,
    GroupOwners,
    AnyUser,
}

impl Publish {
    pub fn parse(s: &str) -> Option<Publish> {
        match s.trim().to_ascii_lowercase().as_str() {
            "admins" | "" => Some(Publish::Admins),
            "group_owners" => Some(Publish::GroupOwners),
            "any_user" => Some(Publish::AnyUser),
            _ => None,
        }
    }

    /// env FQS_SUBCORPUS_PUBLISH, else the limits file's `subcorpora.publish`, else admins. An
    /// unknown value counts as `admins` (the strictest), with a warning.
    pub fn from_config(limits_file: &Value) -> Publish {
        let raw = std::env::var("FQS_SUBCORPUS_PUBLISH")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| limits_file.pointer("/subcorpora/publish").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        Publish::parse(&raw).unwrap_or_else(|| {
            eprintln!("[fqs] subcorpora.publish '{raw}' is not admins, group_owners or any_user: using admins");
            Publish::Admins
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Groups,
    Public,
}

impl Visibility {
    pub fn parse(s: &str) -> Option<Visibility> {
        match s.trim() {
            "private" => Some(Visibility::Private),
            "groups" => Some(Visibility::Groups),
            "public" => Some(Visibility::Public),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Groups => "groups",
            Visibility::Public => "public",
        }
    }
}

pub fn is_admin(caller: &Caller) -> bool {
    caller.role == "admin"
}

/// The owner a subcorpus created by `caller` gets: the caller's user id; None for a caller FQS only
/// knows by session or address (they cannot own one).
pub fn owner_of(caller: &Caller) -> Option<String> {
    let u = caller.user.trim();
    (!u.is_empty() && !u.starts_with("ip:") && !u.starts_with("session:")).then(|| u.to_string())
}

/// May `caller` use (see, query with) a subcorpus with this owner and visibility?
pub fn may_see(owner: &str, visibility: Visibility, groups: &[String], caller: &Caller) -> bool {
    if is_admin(caller) || owner_of(caller).as_deref() == Some(owner) {
        return true;
    }
    match visibility {
        Visibility::Private => false,
        Visibility::Public => true,
        Visibility::Groups => {
            let mine = access::caller_groups(caller);
            groups.iter().any(|g| mine.contains(g))
        }
    }
}

/// May `caller` change or delete a subcorpus of `owner`? The owner and admins.
pub fn may_manage(owner: &str, caller: &Caller) -> bool {
    is_admin(caller) || owner_of(caller).as_deref() == Some(owner)
}

/// May `caller` give a subcorpus this visibility (making it private is always allowed)?
pub fn check_publish(policy: Publish, visibility: Visibility, groups: &[String], caller: &Caller) -> Result<()> {
    match visibility {
        Visibility::Private => return Ok(()),
        Visibility::Groups if groups.is_empty() => bail!("visibility 'groups' needs a list of groups"),
        _ => {}
    }
    if is_admin(caller) {
        return Ok(());
    }
    let signed_in = caller.role == "user";
    match (policy, visibility) {
        (Publish::Admins, _) => bail!("only admins may publish subcorpora on this server (subcorpora.publish = admins)"),
        (Publish::GroupOwners, Visibility::Public) => bail!("only admins may make a subcorpus public on this server (subcorpora.publish = group_owners)"),
        (Publish::GroupOwners, _) => {
            let not_owned: Vec<&String> = groups.iter().filter(|g| !caller.owned_groups.contains(g)).collect();
            if !not_owned.is_empty() {
                bail!("you can publish only to groups you own; not yours: {}", not_owned.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
            }
            Ok(())
        }
        (Publish::AnyUser, _) if !signed_in => bail!("sign in to publish a subcorpus"),
        (Publish::AnyUser, Visibility::Public) => Ok(()),
        (Publish::AnyUser, _) => {
            let mine = access::caller_groups(caller);
            let not_mine: Vec<&String> = groups.iter().filter(|g| !mine.contains(g)).collect();
            if !not_mine.is_empty() {
                bail!("you can publish only to groups you are in; not yours: {}", not_mine.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn caller(role: &str, user: &str, groups: &[&str], owned: &[&str]) -> Caller {
        Caller {
            role: role.into(),
            tier: role.into(),
            user: user.into(),
            verified: true,
            groups: groups.iter().map(|g| g.to_string()).collect(),
            owned_groups: owned.iter().map(|g| g.to_string()).collect(),
        }
    }

    fn g(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn visibility() {
        let alice = caller("user", "alice", &["acad"], &[]);
        let bob = caller("user", "bob", &["other"], &[]);
        let anon = caller("visitor", "ip:1.2.3.4", &[], &[]);
        let admin = caller("admin", "root", &[], &[]);
        assert!(may_see("alice", Visibility::Private, &[], &alice));
        assert!(!may_see("alice", Visibility::Private, &[], &bob));
        assert!(may_see("alice", Visibility::Private, &[], &admin));
        assert!(may_see("bob", Visibility::Groups, &g(&["acad"]), &alice));
        assert!(!may_see("alice", Visibility::Groups, &g(&["acad"]), &bob));
        assert!(may_see("alice", Visibility::Groups, &g(&["anonymous"]), &anon), "FQS's own groups count");
        assert!(may_see("alice", Visibility::Public, &[], &anon));
        assert!(may_manage("alice", &alice) && !may_manage("alice", &bob) && may_manage("alice", &admin));
        assert_eq!(owner_of(&anon), None);
        assert_eq!(owner_of(&caller("visitor", "session:x", &[], &[])), None);
        assert_eq!(owner_of(&alice).as_deref(), Some("alice"));
    }

    #[test]
    fn publishing() {
        let alice = caller("user", "alice", &["acad"], &["team"]);
        let admin = caller("admin", "root", &[], &[]);
        let anon = caller("visitor", "ip:x", &[], &[]);
        let (pr, gr, pu) = (Visibility::Private, Visibility::Groups, Visibility::Public);
        for p in [Publish::Admins, Publish::GroupOwners, Publish::AnyUser] {
            assert!(check_publish(p, pr, &[], &anon).is_ok(), "private: always");
            assert!(check_publish(p, pu, &[], &admin).is_ok(), "admins: always");
            assert!(check_publish(p, gr, &[], &admin).is_err(), "groups need a list");
        }
        assert!(check_publish(Publish::Admins, gr, &g(&["acad"]), &alice).is_err());
        assert!(check_publish(Publish::GroupOwners, gr, &g(&["team"]), &alice).is_ok());
        assert!(check_publish(Publish::GroupOwners, gr, &g(&["acad"]), &alice).is_err(), "member, not owner");
        assert!(check_publish(Publish::GroupOwners, pu, &[], &alice).is_err());
        assert!(check_publish(Publish::AnyUser, gr, &g(&["acad", "authenticated"]), &alice).is_ok());
        assert!(check_publish(Publish::AnyUser, gr, &g(&["cnc"]), &alice).is_err());
        assert!(check_publish(Publish::AnyUser, pu, &[], &alice).is_ok());
        assert!(check_publish(Publish::AnyUser, pu, &[], &anon).is_err());
    }

    #[test]
    fn config() {
        assert_eq!(Publish::parse("Any_User"), Some(Publish::AnyUser), "case does not matter");
        assert_eq!(Publish::parse("any_user"), Some(Publish::AnyUser));
        assert_eq!(Publish::from_config(&serde_json::json!({"subcorpora": {"publish": "group_owners"}})), Publish::GroupOwners);
        assert_eq!(Publish::from_config(&serde_json::json!({"subcorpora": {"publish": "everyone"}})), Publish::Admins);
        assert_eq!(Publish::from_config(&serde_json::json!({})), Publish::Admins);
    }
}
