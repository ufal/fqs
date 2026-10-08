// Build stamp: which commit a binary was built from, since the Cargo version only
// changes on releases. FQS_BUILD = "<commit>[-dirty], <commit date>", or "unknown"
// outside a git checkout (e.g. a source tarball).
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    let commit = git(&["describe", "--always", "--dirty", "--abbrev=7"]);
    let date = git(&["log", "-1", "--format=%cs"]);
    let stamp = match (commit, date) {
        (Some(c), Some(d)) => format!("{c}, {d}"),
        (Some(c), None) => c,
        _ => "unknown".to_string(),
    };
    println!("cargo:rustc-env=FQS_BUILD={stamp}");
    println!(
        "cargo:rustc-env=FQS_VERSION_LONG={} ({stamp})",
        std::env::var("CARGO_PKG_VERSION").unwrap_or_default()
    );
    // rebuild the stamp when the commit or the working tree changes
    for p in [".git/HEAD", ".git/index", ".git/refs"] {
        println!("cargo:rerun-if-changed={p}");
    }
}
