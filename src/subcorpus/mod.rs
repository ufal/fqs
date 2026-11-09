//! Stored, named subcorpora (EF-12, pando/dev/EMBEDDER-FEEDBACK-2026-11.md): kept in FQS so every
//! front end (KonText, TEITOK, the portal) shares them.
//!
//! - definition.rs: the recipe (regions, documents, token query, expression over other names)
//! - store.rs: the `subcorpora` table (catalogue database)
//! - policy.rs: who sees one (visibility) and who may publish (server setting)
//! - ranges.rs: position sets, set operations, Manatee's `.subc` file
//! - to_pando.rs / to_manatee.rs: the recipe per engine
//! - resolve.rs: resolving a recipe on its engine (at creation, and again after a reindex)
//! - apply.rs: a request's `subcorpus` (`use` / `within`) for `/query`, `/run`, `/info`
//! - api.rs: `GET/POST /corpora/{id}/subcorpora`, `GET/PATCH/DELETE /corpora/{id}/subcorpora/{name}`
//!
//! Engines: pando (definition text per request; `POST /subcorpus` to resolve) and native Manatee
//! (a `.subc` written by libfqs_manatee, the corpus opened as that subcorpus) and CWB (CQP's own
//! subcorpora: undump + save, activated before the query; cwb.rs).

pub mod api;
pub mod apply;
pub mod cwb;
pub mod definition;
pub mod policy;
pub mod ranges;
pub mod resolve;
pub mod store;
pub mod to_manatee;
pub mod to_pando;

/// An answer other than 500: the HTTP status and the message.
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub msg: String,
}

impl Refusal {
    pub fn new(status: u16, msg: impl Into<String>) -> Refusal {
        Refusal { status, msg: msg.into() }
    }
    pub fn bad(msg: impl Into<String>) -> Refusal {
        Refusal::new(400, msg)
    }
    pub fn forbidden(msg: impl Into<String>) -> Refusal {
        Refusal::new(403, msg)
    }
    pub fn conflict(msg: impl Into<String>) -> Refusal {
        Refusal::new(409, msg)
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Refusal {}

/// `bail!` with a status.
#[macro_export]
macro_rules! refuse {
    ($status:expr, $($arg:tt)*) => {
        return Err(anyhow::Error::new($crate::subcorpus::Refusal::new($status, format!($($arg)*))))
    };
}
