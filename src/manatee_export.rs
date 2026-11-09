//! `fqs manatee-export`: a compiled Manatee corpus written as pando's JSONL v2, to build a pando
//! corpus from a Manatee corpus whose vertical is gone (`pando-index --format jsonl FILE DIR`).
//! The writing is libfqs_manatee's (manatee/fqs_manatee_export.cc, fqs-manatee 0.6); this is the
//! command around it. It runs in this process, not in a worker: a crash ends only the command.
//!
//! The corpus is checked first, as before a query (fqsm_check): a registry that does not describe
//! the data is refused, instead of writing a wrong corpus.

use crate::manatee_lib::ManateeLib;
use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct ManateeExportArgs {
    /// The corpus's registry file
    #[arg(long, value_name = "FILE")]
    registry: PathBuf,
    /// Where to write the JSONL ("-": stdout)
    #[arg(long, value_name = "FILE", default_value = "-")]
    out: String,
    /// Dynamic attributes too (DYNAMIC in the registry: derived from others, left out by default)
    #[arg(long)]
    dynamic: bool,
    /// Only the first N positions (a sample to check the conversion on)
    #[arg(long, value_name = "N", default_value_t = 0)]
    limit: u64,
    /// libfqs_manatee to use (default: FQS_MANATEE_LIB, else the one in /etc/fqs/fqs.env)
    #[arg(long, value_name = "FILE")]
    lib: Option<PathBuf>,
    /// Write it even when the corpus check finds that the registry does not describe the data
    #[arg(long)]
    skip_check: bool,
}

/// FQS_MANATEE_LIB from the environment, else from the service's environment file.
fn lib_path(given: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = given {
        return Ok(p.to_path_buf());
    }
    if let Ok(p) = std::env::var("FQS_MANATEE_LIB") {
        if !p.trim().is_empty() {
            return Ok(PathBuf::from(p.trim()));
        }
    }
    for env in ["/etc/fqs/fqs.env", "/usr/local/etc/fqs/fqs.env"] {
        if let Ok(text) = std::fs::read_to_string(env) {
            if let Some(v) = text.lines().find_map(|l| l.trim().strip_prefix("FQS_MANATEE_LIB=")) {
                let v = v.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return Ok(PathBuf::from(v));
                }
            }
        }
    }
    bail!("no libfqs_manatee: give --lib, or set FQS_MANATEE_LIB")
}

pub fn run(a: ManateeExportArgs) -> Result<()> {
    let lib = ManateeLib::load(&lib_path(a.lib.as_deref())?)?;
    let registry = std::fs::canonicalize(&a.registry).with_context(|| format!("--registry {}", a.registry.display()))?;
    let id = registry.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "corpus".into());
    if !a.skip_check {
        let v = lib.check(&id, &registry)?;
        if !v.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let problem = v.get("problem").and_then(Value::as_str).unwrap_or("the corpus check failed");
            bail!("{id}: the registry does not describe the data ({problem}); nothing written (--skip-check writes it anyway)");
        }
    }
    let summary = lib.export_jsonl(&id, &registry, &a.out, a.dynamic, a.limit)?;
    eprintln!("{}", serde_json::to_string_pretty(&summary)?);
    if a.out != "-" {
        eprintln!("then: pando-index --format jsonl {} <pando corpus folder>", a.out);
    }
    Ok(())
}
