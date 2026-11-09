//! Resolving a stored recipe on its corpus's engine: once at creation (the size is stored), and
//! again whenever the corpus's index changed since (pando: its `index_id`; Manatee: a stamp of the
//! registry and data files) or the engine is another one.
//!
//! - pando: the recipe as a pando definition (to_pando.rs), `POST /subcorpus {"definition"}` on the
//!   warm library → {"tokens", "ranges", "encoding", "index_id"}.
//! - Manatee: each part written to a `.subc` by libfqs_manatee in a worker (to_manatee.rs), parts
//!   and expressions joined in FQS (ranges.rs), the result written as the subcorpus's `.subc`
//!   (`<catalogue folder>/subcorpora/<corpus>/…`, env FQS_SUBCORPUS_DIR). A new resolution gets a
//!   new file: a worker that has the old one open keeps reading the old one, never a half-written one.
//!
//! - CWB: each part a CQP query whose ranges come back with `dump` (cwb.rs), joined in FQS, kept
//!   with CQP's `undump` + `save` in `<…>/subcorpora/<corpus>/cwb/`.
//!
//! Names in an expression are the owner's own subcorpora first, else public ones of the corpus.

use super::definition::{Definition, Expr};
use super::policy::Visibility;
use super::ranges::{self, Range};
use super::store::{self, Resolution, Subcorpus};
use super::{cwb, to_manatee, to_pando, Refusal};
use crate::hot_corpus::HotGuard;
use crate::{manatee_lib, manatee_pool, refuse, CorpusEntry, HttpAppState};
use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// A piece count above which (with short pieces) the creator is told the subcorpus is fragmented.
const FRAGMENTED_PIECES: u64 = 100_000;

/// The engine a request on `corpus` runs on, if it has subcorpora: "pando" or "manatee".
pub fn engine(corpus: &CorpusEntry, backend_override: Option<&str>) -> Result<String> {
    let b = match backend_override.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => b.to_string(),
        None => crate::resolve_effective_backend(corpus)?,
    };
    match b.as_str() {
        "pando" => Ok(b),
        "manatee" if manatee_pool::global().is_some() => Ok(b),
        "manatee" => refuse!(400, "subcorpora on Manatee corpora need FQS's native Manatee (FQS_MANATEE_LIB)"),
        "cqp" if crate::cqp_prefers_flexicorp(corpus) => {
            refuse!(400, "subcorpora on CWB corpora run on FQS's own CQP route; corpus '{}' uses flexicorp's (a TEITOK project)", corpus.id)
        }
        "cqp" => Ok(b),
        other => refuse!(400, "subcorpora work on pando and Manatee corpora; corpus '{}' is served by {other}", corpus.id),
    }
}

/// Where Manatee subcorpus files go.
fn subc_dir(state: &HttpAppState, corpus_id: &str) -> PathBuf {
    let base = std::env::var("FQS_SUBCORPUS_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state.db_path.parent().map(Path::to_path_buf).unwrap_or_default().join("subcorpora"));
    base.join(corpus_id.replace(['/', '\\'], "_"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What the pando index is: `index_id` of its corpus.info (else `build_id`, else "").
pub fn pando_index_id(index_dir: &Path) -> String {
    let text = std::fs::read_to_string(index_dir.join("corpus.info")).unwrap_or_default();
    let get = |k: &str| text.lines().find_map(|l| l.trim().strip_prefix(k).and_then(|r| r.strip_prefix('=')).map(|v| v.trim().to_string()));
    get("index_id").or_else(|| get("build_id")).unwrap_or_default()
}

/// A Manatee corpus's stamp: its registry file, and the name, size and time of every file in its
/// data folder. Any recompilation changes it.
pub fn manatee_stamp(registry: &Path) -> String {
    files_stamp(registry, manatee_lib::data_dir(registry))
}

/// A registry file and the name, size and time of every file in a data folder, as one stamp.
pub fn files_stamp(registry: &Path, data_dir: Option<PathBuf>) -> String {
    let mut h = Sha256::new();
    h.update(std::fs::read(registry).unwrap_or_default());
    if let Some(dir) = data_dir {
        let mut entries: Vec<(String, u64, i64)> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let m = e.metadata().ok()?;
                        let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as i64;
                        Some((e.file_name().to_string_lossy().into_owned(), m.len(), t))
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        for (n, l, t) in entries {
            h.update(format!("{n}\t{l}\t{t}\n"));
        }
    }
    hex(&h.finalize()[..8])
}

/// The context of one resolution.
pub struct Ctx<'a> {
    pub state: &'a HttpAppState,
    pub corpus: &'a CorpusEntry,
    pub engine: String,
    pub conn: &'a Connection,
}

impl Ctx<'_> {
    fn registry(&self) -> Result<PathBuf> {
        manatee_lib::registry_file(&self.corpus.settings, &self.corpus.id)
            .ok_or_else(|| anyhow!("no Manatee registry for '{}' (settings.manatee_registry, or MANATEE_REGISTRY)", self.corpus.id))
    }

    /// The CWB corpus name, its registry file and data folder.
    fn cwb(&self) -> (String, PathBuf, Option<PathBuf>) {
        let (_, name, _, reg_dir) = crate::cqp_base_command(self.corpus);
        let reg_dir = PathBuf::from(reg_dir);
        let data = crate::warm::cwb_data_dir(&reg_dir, &name);
        (name.clone(), reg_dir.join(name.to_lowercase()), data)
    }

    /// The current index of the corpus on this engine.
    pub fn against(&self) -> Result<String> {
        Ok(match self.engine.as_str() {
            "pando" => pando_index_id(&crate::resolve_pando_index_dir(self.corpus)?),
            "cqp" => {
                let (_, reg, data) = self.cwb();
                files_stamp(&reg, data)
            }
            _ => manatee_stamp(&self.registry()?),
        })
    }

    /// Is the stored resolution's file still there?
    fn file_there(&self, r: &Resolution) -> bool {
        let Some(p) = r.subc_path.as_deref().map(Path::new) else { return self.engine == "pando" };
        match self.engine.as_str() {
            "cqp" => cwb::saved(&self.cwb().0, p),
            _ => p.is_file(),
        }
    }

    /// A subcorpus named in an expression of `owner`'s: their own, else a public one.
    pub fn named(&self, owner: &str, name: &str) -> Result<Subcorpus> {
        if let Some(s) = store::get(self.conn, &self.corpus.id, owner, name)? {
            return Ok(s);
        }
        let public: Vec<Subcorpus> = store::by_name(self.conn, &self.corpus.id, name)?.into_iter().filter(|s| s.visibility == Visibility::Public).collect();
        match public.len() {
            1 => Ok(public.into_iter().next().expect("one")),
            0 => refuse!(400, "no subcorpus '{name}' of yours (or public) on corpus '{}'", self.corpus.id),
            _ => refuse!(409, "several public subcorpora are called '{name}' on corpus '{}'; copy the one you mean under a name of your own", self.corpus.id),
        }
    }

    /// The pando definition of `def` (an expression: its names expanded as `owner` sees them).
    pub fn pando_definition(&self, owner: &str, def: &Definition) -> Result<String> {
        let mut lookup = |n: &str| -> Result<Definition> { Ok(self.named(owner, n)?.definition) };
        to_pando::definition(def, &mut lookup)
    }

    /// Fill in what the engine needs to be explicit before a definition is stored (the document
    /// structure and attribute of a documents definition); check the names of an expression.
    pub fn normalize(&self, owner: &str, def: Definition) -> Result<Definition> {
        Ok(match def {
            Definition::Documents { ids, structure, attr } if structure.is_none() || attr.is_none() => {
                let (s, a) = match self.engine.as_str() {
                    "pando" => (to_pando::DOC_STRUCT.to_string(), to_pando::DOC_ATTR.to_string()),
                    "cqp" => (cwb::DOC_STRUCT.to_string(), cwb::DOC_ATTR.to_string()),
                    _ => to_manatee::doc_attr(manatee_lib::fullref(&self.registry()?).as_deref()),
                };
                Definition::Documents { ids, structure: Some(structure.unwrap_or(s)), attr: Some(attr.unwrap_or(a)) }
            }
            Definition::Expression { expression } => {
                for n in Expr::parse(&expression)?.names() {
                    self.named(owner, &n)?;
                }
                Definition::Expression { expression }
            }
            d => d,
        })
    }

    /// Resolve a definition of `owner`'s (`key` names its file: corpus, owner, name).
    pub fn resolve(&self, owner: &str, name: &str, def: &Definition) -> Result<(Resolution, Vec<String>)> {
        let against = self.against()?;
        let res = match self.engine.as_str() {
            "pando" => {
                let text = self.pando_definition(owner, def)?;
                let v = pando_subcorpus(self.state, self.corpus, &text)?;
                let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
                // stored: the index_id FQS reads from corpus.info (what `fresh` compares with), not
                // pando's answer, which is the open (warm) index's and may be the previous one
                store::resolution_now("pando", n("tokens"), n("ranges"), v.get("encoding").and_then(Value::as_str).unwrap_or("ranges"), &against, None)
            }
            engine => {
                let rs = self.ranges_of(owner, def, 0)?;
                if rs.is_empty() {
                    refuse!(400, "the subcorpus definition selects no tokens of corpus '{}'", self.corpus.id);
                }
                let mut key = Sha256::new();
                key.update(format!("{}\u{0}{owner}\u{0}{name}", self.corpus.id));
                let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
                // a CQP subcorpus name: a letter first, letters, digits, '_'
                let stem = format!("Fqs_{}_{nanos}", hex(&key.finalize()[..8]));
                let dir = subc_dir(self.state, &self.corpus.id);
                let path = if engine == "cqp" {
                    cwb::save(self.corpus, &dir.join("cwb"), &stem, &rs)?
                } else {
                    let p = dir.join(format!("{stem}.subc"));
                    ranges::write_subc(&p, &rs)?;
                    p
                };
                store::resolution_now(engine, ranges::tokens(&rs), rs.len() as u64, "ranges", &against, Some(path.display().to_string()))
            }
        };
        if res.tokens == 0 {
            refuse!(400, "the subcorpus definition selects no tokens of corpus '{}'", self.corpus.id);
        }
        let mut warnings = Vec::new();
        if res.ranges > FRAGMENTED_PIECES && res.tokens / res.ranges.max(1) < 4 {
            warnings.push(format!(
                "the subcorpus is fragmented ({} pieces for {} tokens): queries restricted to it are slower than on a region-based one",
                res.ranges, res.tokens
            ));
        }
        Ok((res, warnings))
    }

    /// The stored subcorpus, resolved again when its index or engine changed (or its file is gone).
    pub fn fresh(&self, mut s: Subcorpus) -> Result<Subcorpus> {
        let stale = match &s.resolution {
            None => true,
            Some(r) => {
                r.engine != self.engine || r.against != self.against()? || !self.file_there(r)
            }
        };
        if stale {
            let (res, _) = self.resolve(&s.owner, &s.name, &s.definition)?;
            store::set_resolution(self.conn, s.id, &res)?;
            if let Some(old) = s.resolution.replace(res) {
                remove_files(self.corpus, &old);
            }
        }
        Ok(s)
    }

    /// The ranges of a definition on Manatee or CWB: its parts from the engine, an expression
    /// joined here from its names' stored files.
    fn ranges_of(&self, owner: &str, def: &Definition, depth: usize) -> Result<Vec<Range>> {
        if depth > to_pando::MAX_DEPTH {
            refuse!(400, "subcorpus expressions nest more than {} deep (a name defined through itself?)", to_pando::MAX_DEPTH);
        }
        if let Definition::Expression { expression } = def {
            let e = Expr::parse(expression)?;
            let mut leaf = |n: &str| -> Result<Vec<Range>> {
                let s = self.named(owner, n)?;
                if let Definition::Expression { .. } = s.definition {
                    return self.ranges_of(&s.owner, &s.definition, depth + 1);
                }
                let s = self.fresh(s)?;
                let p = s.resolution.and_then(|r| r.subc_path).ok_or_else(|| anyhow!("subcorpus '{n}' has no file"))?;
                ranges::read_subc(Path::new(&p))
            };
            return e.eval(&mut leaf, &|op, a, b| match op {
                '&' => ranges::intersect(&a, &b),
                '|' => ranges::union(&a, &b),
                _ => ranges::difference(&a, &b),
            });
        }
        if self.engine == "cqp" {
            let mut all = Vec::new();
            for q in cwb::parts(def)? {
                all.extend(cwb::dump_ranges(self.corpus, &q)?);
            }
            return Ok(ranges::normalize(all));
        }
        let pool = manatee_pool::global().ok_or_else(|| anyhow!("native Manatee is not available (FQS_MANATEE_LIB)"))?;
        let registry = self.registry()?;
        let doc = to_manatee::doc_attr(manatee_lib::fullref(&registry).as_deref());
        let dir = subc_dir(self.state, &self.corpus.id);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut all = Vec::new();
        for (i, part) in to_manatee::parts(def, (&doc.0, &doc.1))?.iter().enumerate() {
            let tmp = dir.join(format!(".part-{}-{i}.subc", std::process::id()));
            let req = json!({ "op": "subc_create", "registry": registry, "path": tmp, "structure": part.structure, "query": part.query });
            let res = pool.call(&self.corpus.id, req).map_err(|e| anyhow!(Refusal::bad(format!("{e:#}"))))?;
            if res.get("written").and_then(Value::as_bool) == Some(true) {
                let r = ranges::read_subc(&tmp);
                let _ = std::fs::remove_file(&tmp);
                all.extend(r?);
            }
        }
        Ok(ranges::normalize(all))
    }
}

/// Remove the files of a resolution that is no longer used (a re-resolved or deleted subcorpus).
/// A Manatee worker that still has a `.subc` open keeps reading it (the file stays until closed).
pub fn remove_files(corpus: &CorpusEntry, r: &Resolution) {
    let Some(p) = r.subc_path.as_deref().map(Path::new) else { return };
    if r.engine == "cqp" {
        cwb::remove(&crate::cqp_base_command(corpus).1, p);
    } else {
        let _ = std::fs::remove_file(p);
    }
}

/// pando's `POST /subcorpus {"definition"}` on the corpus's warm library.
fn pando_subcorpus(state: &HttpAppState, corpus: &CorpusEntry, definition: &str) -> Result<Value> {
    let hcm = state.pando_hcm.clone().ok_or_else(|| anyhow!(Refusal::new(503, "subcorpora on pando corpora need FQS's pando library (FLEXICORP_PANDO_LIB)")))?;
    let index_dir = crate::resolve_pando_index_dir(corpus)?;
    let opts = state.limits.engine_options_for(&corpus.settings);
    let guard = HotGuard::acquire(hcm, &corpus.id, &index_dir, false, Some(&opts))?;
    let (status, v) = guard.request("POST", "/subcorpus", "", &json!({ "definition": definition }).to_string())?;
    if status == 404 {
        refuse!(501, "this pando library has no subcorpora (POST /subcorpus): update libflexicorp_pando");
    }
    if status >= 400 {
        let msg = v.get("error").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| v.to_string());
        refuse!(400, "pando: {msg}");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_ids_and_stamps() {
        let d = std::env::temp_dir().join(format!("fqs-subc-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("data")).unwrap();
        std::fs::write(d.join("corpus.info"), "size=3\nbuild_id=b2\nindex_id=z9\n").unwrap();
        assert_eq!(pando_index_id(&d), "z9");
        std::fs::write(d.join("corpus.info"), "size=3\nbuild_id=b2\n").unwrap();
        assert_eq!(pando_index_id(&d), "b2");
        let reg = d.join("reg");
        std::fs::write(&reg, format!("PATH \"{}\"\n", d.join("data").display())).unwrap();
        let s1 = manatee_stamp(&reg);
        assert_eq!(s1, manatee_stamp(&reg), "stable");
        std::fs::write(d.join("data/word.lex"), "x").unwrap();
        assert_ne!(s1, manatee_stamp(&reg), "a data file changes it");
        let _ = std::fs::remove_dir_all(&d);
    }
}
