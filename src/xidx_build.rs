//! Does a corpus's xidx belong to its pando index? flexencoder writes one id per run into both
//! (`xidx/build.id`, `build_id=` in the index's corpus.info; flexencoder_build_id.hpp). The xidx's byte
//! offsets are those of the index's positions only when the ids are the same; an xidx rewritten by a
//! reindex whose index was not swapped in (or the other way round) is not used: its hits then get
//! synthetic fragments, as a corpus without XML. Not checked when neither records an id (older builds).
//! The adapter makes the same check before it cuts a fragment (flexicorp_json.h, xidx_matches_index).

use std::path::Path;

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim_end().to_string()
}

/// The xidx's run id, "" when not recorded.
pub fn xidx_build_id(xidx_dir: &Path) -> String {
    std::fs::read_to_string(xidx_dir.join("build.id")).map(|t| first_line(&t)).unwrap_or_default()
}

/// The index's run id (`build_id=` in corpus.info, or in current/ of a published root), "" when not recorded.
pub fn index_build_id(index_dir: &Path) -> String {
    for info in [index_dir.join("corpus.info"), index_dir.join("current").join("corpus.info")] {
        if let Ok(text) = std::fs::read_to_string(&info) {
            return text
                .lines()
                .find_map(|l| l.strip_prefix("build_id="))
                .map(|v| v.trim_end().to_string())
                .unwrap_or_default();
        }
    }
    String::new()
}

/// The xidx and the index are from the same flexencoder run (or neither says).
pub fn xidx_matches_index(xidx_dir: &Path, index_dir: Option<&Path>) -> bool {
    let Some(index_dir) = index_dir else { return true };
    let (x, i) = (xidx_build_id(xidx_dir), index_build_id(index_dir));
    (x.is_empty() && i.is_empty()) || x == i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_compared() {
        let d = std::env::temp_dir().join(format!("fqs-xidx-build-{}", std::process::id()));
        let (x, i) = (d.join("xidx"), d.join("pando"));
        std::fs::create_dir_all(&x).unwrap();
        std::fs::create_dir_all(i.join("current")).unwrap();
        assert!(xidx_matches_index(&x, Some(&i)), "neither recorded");
        std::fs::write(x.join("build.id"), "b1\n").unwrap();
        assert!(!xidx_matches_index(&x, Some(&i)), "the index has none");
        std::fs::write(i.join("current").join("corpus.info"), "size=3\nbuild_id=b1\n").unwrap();
        assert!(xidx_matches_index(&x, Some(&i)), "a published root's current/");
        std::fs::write(i.join("corpus.info"), "size=3\nbuild_id=b2\nindex_id=z\n").unwrap();
        assert!(!xidx_matches_index(&x, Some(&i)), "another run");
        assert!(xidx_matches_index(&x, None), "no index to compare with");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
