//! Position sets as sorted, disjoint, non-adjacent half-open ranges `[beg, end)`, the set
//! operations of subcorpus expressions (one pass over both lists), and Manatee's `.subc` file:
//! the ranges as native-endian int64 pairs `beg end beg end …`, adjacent ranges merged, exactly as
//! manatee-open's `create_subcorpus` writes it (corp/subcorp.cc).

use anyhow::{bail, Context, Result};
use std::path::Path;

pub type Range = (i64, i64);

/// Sorted, merged ranges from any list (empty ranges dropped).
pub fn normalize(mut v: Vec<Range>) -> Vec<Range> {
    v.retain(|(b, e)| e > b);
    v.sort_unstable();
    let mut out: Vec<Range> = Vec::with_capacity(v.len());
    for (b, e) in v {
        match out.last_mut() {
            Some(last) if b <= last.1 => last.1 = last.1.max(e),
            _ => out.push((b, e)),
        }
    }
    out
}

pub fn tokens(v: &[Range]) -> u64 {
    v.iter().map(|(b, e)| (e - b) as u64).sum()
}

pub fn union(a: &[Range], b: &[Range]) -> Vec<Range> {
    let mut all = Vec::with_capacity(a.len() + b.len());
    all.extend_from_slice(a);
    all.extend_from_slice(b);
    normalize(all)
}

pub fn intersect(a: &[Range], b: &[Range]) -> Vec<Range> {
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() && j < b.len() {
        let beg = a[i].0.max(b[j].0);
        let end = a[i].1.min(b[j].1);
        if beg < end {
            out.push((beg, end));
        }
        if a[i].1 < b[j].1 { i += 1 } else { j += 1 }
    }
    normalize(out)
}

pub fn difference(a: &[Range], b: &[Range]) -> Vec<Range> {
    let mut out = Vec::new();
    let mut j = 0;
    for &(mut beg, end) in a {
        while j < b.len() && b[j].1 <= beg {
            j += 1;
        }
        let mut k = j;
        while k < b.len() && b[k].0 < end {
            if b[k].0 > beg {
                out.push((beg, b[k].0));
            }
            beg = beg.max(b[k].1);
            k += 1;
        }
        if beg < end {
            out.push((beg, end));
        }
    }
    out
}

/// Ranges as Manatee's .subc bytes.
pub fn to_subc_bytes(v: &[Range]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 16);
    for (b, e) in normalize(v.to_vec()) {
        out.extend_from_slice(&b.to_ne_bytes());
        out.extend_from_slice(&e.to_ne_bytes());
    }
    out
}

pub fn from_subc_bytes(bytes: &[u8]) -> Result<Vec<Range>> {
    if bytes.len() % 16 != 0 {
        bail!("not a Manatee subcorpus file ({} bytes, not pairs of int64)", bytes.len());
    }
    let n = |c: &[u8]| i64::from_ne_bytes(c.try_into().expect("8 bytes"));
    Ok(bytes.chunks_exact(16).map(|c| (n(&c[..8]), n(&c[8..]))).collect())
}

pub fn read_subc(path: &Path) -> Result<Vec<Range>> {
    from_subc_bytes(&std::fs::read(path).with_context(|| format!("reading {}", path.display()))?)
}

/// Written next to the target and renamed: a worker never reads half a file.
pub fn write_subc(path: &Path, v: &[Range]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("subc.tmp");
    std::fs::write(&tmp, to_subc_bytes(v)).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_operations() {
        let a = normalize(vec![(10, 20), (0, 5), (5, 7), (30, 40)]);
        assert_eq!(a, vec![(0, 7), (10, 20), (30, 40)], "adjacent merged, sorted");
        let b = vec![(3, 12), (15, 35)];
        assert_eq!(union(&a, &b), vec![(0, 40)]);
        assert_eq!(intersect(&a, &b), vec![(3, 7), (10, 12), (15, 20), (30, 35)]);
        assert_eq!(difference(&a, &b), vec![(0, 3), (12, 15), (35, 40)]);
        assert_eq!(difference(&b, &a), vec![(7, 10), (20, 30)]);
        assert_eq!(tokens(&a), 7 + 10 + 10);
        assert_eq!(difference(&a, &[]), a);
        assert!(intersect(&a, &[]).is_empty());
    }

    #[test]
    fn subc_round_trip() {
        let d = std::env::temp_dir().join(format!("fqs-subc-{}", std::process::id()));
        let p = d.join("x.subc");
        write_subc(&p, &[(5, 9), (0, 2), (2, 3)]).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 32);
        assert_eq!(read_subc(&p).unwrap(), vec![(0, 3), (5, 9)]);
        assert!(from_subc_bytes(&[0; 9]).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
