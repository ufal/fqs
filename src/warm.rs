//! `POST /warm`: a frontend says a corpus is about to be queried — TEITOK's search page opening,
//! as KonText's choice of a corpus — and FQS reads the corpus's main index files into the file
//! cache in the background, so the first query does not pay the cold reads (over NFS above all).
//! A pando corpus is also opened into the warm pool (that alone reads nothing: it maps its files).
//!
//! Guards: a corpus warmed within `cooldown` is skipped, at most `max_running` warm-ups run at
//! once, and one reads at most `max_bytes`, the most queried attributes first (word forms,
//! lemmas), so a warm-up cannot compete with real queries for long.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What a warm request got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Started in the background.
    Accepted,
    /// Warmed (or being warmed) within the cooldown: nothing to do.
    Recent,
    /// Too many warm-ups running: try again later.
    Busy,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Accepted => "accepted",
            Outcome::Recent => "recent",
            Outcome::Busy => "busy",
        }
    }
}

/// Who may warm what, now.
pub struct Warmer {
    recent: Mutex<HashMap<String, Instant>>,
    running: AtomicUsize,
    pub max_running: usize,
    pub cooldown: Duration,
    pub max_bytes: u64,
}

impl Warmer {
    pub fn new(max_running: usize, cooldown: Duration, max_bytes: u64) -> Self {
        Warmer { recent: Mutex::new(HashMap::new()), running: AtomicUsize::new(0), max_running: max_running.max(1), cooldown, max_bytes }
    }

    /// Settings from the environment: FQS_WARM_MAX_RUNNING (2), FQS_WARM_COOLDOWN_SECS (600),
    /// FQS_WARM_MAX_MB (1024).
    pub fn from_env() -> Self {
        let num = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(d);
        Warmer::new(
            num("FQS_WARM_MAX_RUNNING", 2) as usize,
            Duration::from_secs(num("FQS_WARM_COOLDOWN_SECS", 600)),
            num("FQS_WARM_MAX_MB", 1024) * 1024 * 1024,
        )
    }

    /// Claim a warm-up of `corpus_id`; on Accepted the caller must call `finish` when done.
    pub fn try_start(&self, corpus_id: &str) -> Outcome {
        let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if let Some(t) = recent.get(corpus_id) {
            if now.duration_since(*t) < self.cooldown {
                return Outcome::Recent;
            }
        }
        if self.running.load(Ordering::SeqCst) >= self.max_running {
            return Outcome::Busy;
        }
        self.running.fetch_add(1, Ordering::SeqCst);
        recent.insert(corpus_id.to_string(), now);
        recent.retain(|_, t| now.duration_since(*t) < self.cooldown);
        Outcome::Accepted
    }

    pub fn finish(&self) {
        let _ = self.running.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| Some(n.saturating_sub(1)));
    }

    #[cfg(test)]
    pub fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }
}

/// The files of an index folder in the order to read them: those starting with one of
/// `priority` (in that order), then the rest, smallest first (structures, lexicons, small
/// attributes before large ones).
pub fn ordered_files(dir: &Path, priority: &[&str]) -> Vec<(PathBuf, u64)> {
    let mut files: Vec<(PathBuf, u64)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let md = e.metadata().ok()?;
                    md.is_file().then(|| (e.path(), md.len()))
                })
                .collect()
        })
        .unwrap_or_default();
    let rank = |p: &Path| -> usize {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        priority.iter().position(|pre| name.starts_with(pre)).unwrap_or(priority.len())
    };
    files.sort_by(|a, b| rank(&a.0).cmp(&rank(&b.0)).then(a.1.cmp(&b.1)).then(a.0.cmp(&b.0)));
    files
}

/// Read files (in `ordered_files` order) until `max_bytes`: the page cache keeps them for the
/// queries to come. Returns (files read, bytes read).
pub fn read_into_cache(dir: &Path, priority: &[&str], max_bytes: u64) -> (usize, u64) {
    let mut buf = vec![0u8; 1 << 20];
    let (mut n_files, mut total) = (0usize, 0u64);
    for (path, _) in ordered_files(dir, priority) {
        if total >= max_bytes {
            break;
        }
        let Ok(mut f) = File::open(&path) else { continue };
        n_files += 1;
        loop {
            if total >= max_bytes {
                break;
            }
            match f.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => total += n as u64,
            }
        }
    }
    (n_files, total)
}

/// A CWB corpus's data folder: the `HOME` line of its registry file (`registry_dir/<name>`, lower case).
pub fn cwb_data_dir(registry_dir: &Path, corpus_name: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(registry_dir.join(corpus_name.to_lowercase())).ok()?;
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("HOME")?;
        let dir = rest.trim().trim_matches('"').trim();
        (!dir.is_empty() && rest.starts_with(char::is_whitespace)).then(|| PathBuf::from(dir))
    })
}

/// The attributes read first: pando (form, lemma) and CWB (word, lemma) naming.
pub const PANDO_PRIORITY: &[&str] = &["form.", "lemma."];
pub const CWB_PRIORITY: &[&str] = &["word.", "lemma."];

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fqs-warm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn write(dir: &Path, name: &str, size: usize) {
        File::create(dir.join(name)).unwrap().write_all(&vec![7u8; size]).unwrap();
    }

    #[test]
    fn cooldown_and_concurrency() {
        let w = Warmer::new(1, Duration::from_secs(60), 1 << 20);
        assert_eq!(w.try_start("a"), Outcome::Accepted);
        assert_eq!(w.try_start("a"), Outcome::Recent, "the same corpus again: within the cooldown");
        assert_eq!(w.try_start("b"), Outcome::Busy, "one at a time");
        w.finish();
        assert_eq!(w.try_start("b"), Outcome::Accepted);
        w.finish();
        assert_eq!(w.running(), 0);
        let w0 = Warmer::new(2, Duration::from_secs(0), 1);
        assert_eq!(w0.try_start("a"), Outcome::Accepted);
        w0.finish();
        assert_eq!(w0.try_start("a"), Outcome::Accepted, "no cooldown: again");
    }

    #[test]
    fn order_and_cap() {
        let d = tmpdir("order");
        write(&d, "zz.big", 5000);
        write(&d, "lemma.rev", 3000);
        write(&d, "form.dat", 2000);
        write(&d, "corpus.info", 10);
        let names: Vec<String> = ordered_files(&d, PANDO_PRIORITY).iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["form.dat", "lemma.rev", "corpus.info", "zz.big"]);
        assert_eq!(read_into_cache(&d, PANDO_PRIORITY, 1 << 30), (4, 10010), "everything under the cap");
        let (files, bytes) = read_into_cache(&d, PANDO_PRIORITY, 2500);
        assert_eq!(files, 2, "stops at the cap: form first, then part of lemma");
        assert!(bytes >= 2500 && bytes <= 5000);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cwb_home() {
        let d = tmpdir("reg");
        std::fs::write(d.join("tt_mycorpus"), "NAME \"My corpus\"\nID tt_mycorpus\nHOME \"/data/cwb/tt_mycorpus\"\nATTRIBUTE word\n").unwrap();
        std::fs::write(d.join("bare"), "ID bare\nHOME /data/cwb/bare\n").unwrap();
        std::fs::write(d.join("nohome"), "ID nohome\nHOMEPAGE x\n").unwrap();
        assert_eq!(cwb_data_dir(&d, "TT_MYCORPUS"), Some(PathBuf::from("/data/cwb/tt_mycorpus")), "registry names are lower case");
        assert_eq!(cwb_data_dir(&d, "bare"), Some(PathBuf::from("/data/cwb/bare")));
        assert_eq!(cwb_data_dir(&d, "nohome"), None, "HOMEPAGE is not HOME");
        assert_eq!(cwb_data_dir(&d, "missing"), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
