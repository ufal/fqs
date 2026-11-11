//! A cap on how much of one hit is sent back. A pando region query (`r:<text lang="x">`) has a whole
//! text as its match: the engine lists every token of it and the xidx cuts its whole XML three times
//! over (`fragment`, `context_xml`, `context_data`), some megabytes per hit; a page of them is
//! tens of megabytes, more than the caller can take. Hits longer than the cap keep their first
//! tokens (the XML fragment is cut from the same span, so it stays well-formed) and say so in
//! `truncated`; `match_start` / `match_end` stay the real span.
//!
//! FQS_HIT_MAX_TOKENS (default 500; 0 = no cap).

use serde_json::{json, Value};

pub const DEFAULT_MAX_TOKENS: usize = 500;

pub fn max_tokens_from_env() -> usize {
    std::env::var("FQS_HIT_MAX_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_TOKENS)
}

/// The last token position to cut a fragment to for a match `a..=b`: `b`, or the cap's end.
pub fn capped_end(a: i64, b: i64, max: usize) -> i64 {
    if max == 0 || b < a || (b - a + 1) as usize <= max {
        b
    } else {
        a + max as i64 - 1
    }
}

/// Cut the `tokens` list of every hit (and both sides of aligned pairs) in the engine's answer.
/// Returns the number of hits that were cut.
pub fn cap_result(engine: &mut Value, max: usize) -> usize {
    if max == 0 {
        return 0;
    }
    let Some(result) = engine.get_mut("result") else { return 0 };
    let mut n = 0;
    let mut cap_hit = |h: &mut Value| {
        let span = match (h.get("match_start").and_then(Value::as_i64), h.get("match_end").and_then(Value::as_i64)) {
            (Some(a), Some(b)) if b >= a => (b - a + 1) as usize,
            _ => 0,
        };
        let listed = h.get("tokens").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
        if span <= max && listed <= max {
            return;
        }
        if let Some(t) = h.get_mut("tokens").and_then(Value::as_array_mut) {
            t.truncate(max);
        }
        if let Some(o) = h.as_object_mut() {
            o.insert("truncated".into(), json!({"tokens": span.max(listed), "kept": max}));
        }
        n += 1;
    };
    if let Some(hits) = result.get_mut("hits").and_then(Value::as_array_mut) {
        hits.iter_mut().for_each(&mut cap_hit);
    }
    if let Some(pairs) = result.get_mut("pairs").and_then(Value::as_array_mut) {
        for p in pairs {
            for side in ["source", "target"] {
                if let Some(h) = p.get_mut(side) {
                    cap_hit(h);
                }
            }
        }
    }
    n
}

/// `compact_regions`: every hit becomes a small record, whatever the size of its match: `doc_id`, the
/// span, `named_regions` (the region's attributes), the number of tokens, no token list, no XML.
pub fn compact_result(engine: &mut Value) {
    let Some(result) = engine.get_mut("result") else { return };
    let compact = |h: &mut Value| {
        let Some(o) = h.as_object_mut() else { return };
        let listed = o.get("tokens").and_then(Value::as_array).map(Vec::len);
        let span = match (o.get("match_start").and_then(Value::as_i64), o.get("match_end").and_then(Value::as_i64)) {
            (Some(a), Some(b)) if b >= a => Some((b - a + 1) as usize),
            _ => None,
        };
        const KEEP: &[&str] = &["doc_id", "doc_xml", "match_start", "match_end", "named_regions", "region", "attrs"];
        o.retain(|k, _| KEEP.contains(&k.as_str()));
        o.insert("token_count".into(), json!(span.or(listed).unwrap_or(0)));
        o.insert("compact".into(), json!(true));
    };
    if let Some(hits) = result.get_mut("hits").and_then(Value::as_array_mut) {
        hits.iter_mut().for_each(compact);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_end_keeps_short_spans() {
        assert_eq!(capped_end(10, 20, 500), 20);
        assert_eq!(capped_end(0, 499, 500), 499);
        assert_eq!(capped_end(0, 500, 500), 499);
        assert_eq!(capped_end(100, 9000, 500), 599);
        assert_eq!(capped_end(5, 9000, 0), 9000);
    }

    #[test]
    fn long_hits_lose_tokens_short_ones_do_not() {
        let toks = |n: usize| (0..n).map(|i| json!({"form": i})).collect::<Vec<_>>();
        let mut e = json!({"result": {"hits": [
            {"match_start": 0, "match_end": 9, "tokens": toks(10)},
            {"match_start": 100, "match_end": 1599, "tokens": toks(1500)}
        ]}});
        assert_eq!(cap_result(&mut e, 500), 1);
        let hits = e["result"]["hits"].as_array().unwrap();
        assert_eq!(hits[0]["tokens"].as_array().unwrap().len(), 10);
        assert!(hits[0].get("truncated").is_none());
        assert_eq!(hits[1]["tokens"].as_array().unwrap().len(), 500);
        assert_eq!(hits[1]["truncated"], json!({"tokens": 1500, "kept": 500}));
        assert_eq!(hits[1]["match_end"], 1599);
    }

    #[test]
    fn compact_keeps_the_record_and_drops_the_text() {
        let mut e = json!({"result": {"hits": [{"doc_id": "a/b.xml", "match_start": 10, "match_end": 1009,
            "named_regions": {"r": {"lang": "Bororo"}}, "tokens": [1, 2], "fragment": "<s/>", "context_xml": "<s/>", "groups": [1]}]}});
        compact_result(&mut e);
        let h = &e["result"]["hits"][0];
        assert_eq!(h["token_count"], 1000);
        assert_eq!(h["doc_id"], "a/b.xml");
        assert_eq!(h["named_regions"]["r"]["lang"], "Bororo");
        for k in ["tokens", "fragment", "context_xml", "groups"] {
            assert!(h.get(k).is_none(), "{k}");
        }
    }

    #[test]
    fn no_cap_leaves_everything() {
        let mut e = json!({"result": {"hits": [{"match_start": 0, "match_end": 5000, "tokens": [1, 2, 3]}]}});
        assert_eq!(cap_result(&mut e, 0), 0);
        assert!(e["result"]["hits"][0].get("truncated").is_none());
    }
}
