//! `fqs report`: a usage overview built from the activity log (like the
//! log-file analysers of old). Reads the live log and its rotated copies
//! (`FILE.1.gz` …), aggregates the `query` events and writes `report.json` and
//! a self-contained `report.html`.
//!
//! Counting rules, so the numbers mean something:
//!   * a *call* is one logged query event; a *search* is a call that does not
//!     repeat the same corpus + pattern within `--repeat-secs` (a search
//!     and its `freq` follow-up, or the cached second run, are one search);
//!   * a *burst* is 5+ calls from one identity with under a second between
//!     them (a script or test run): kept out of the typical-query rankings and
//!     shown on its own.
//!
//! The user is what the log holds (hashed by default); see the notes the report
//! prints about what that identity is.

use crate::query_shape::{analyse, Analysis, Kind};
use anyhow::{anyhow, Context, Result};
use clap::Args;
use flate2::read::GzDecoder;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

#[derive(Args, Debug)]
pub struct ReportArgs {
    /// Activity log (`serve --activity-log`); rotated `FILE.N.gz` are read too
    #[arg(long, env = "FQS_ACTIVITY_LOG", default_value = "/var/log/fqs/activity.jsonl")]
    pub activity_log: PathBuf,
    /// Period: `30d`, `12h`, or `all`
    #[arg(long, default_value = "30d")]
    pub since: String,
    /// Output folder (report.html + report.json)
    #[arg(long, default_value = "fqs-report")]
    pub out: PathBuf,
    /// Only this corpus
    #[arg(long)]
    pub corpus: Option<String>,
    /// Same corpus + pattern within this many seconds counts as one search
    #[arg(long, default_value_t = 10.0)]
    pub repeat_secs: f64,
    /// Rows in the ranking tables
    #[arg(long, default_value_t = 15)]
    pub top: usize,
}

struct Q {
    t: f64,
    day: String,
    hour: usize,
    corpus: String,
    endpoint: String,
    query: String,
    status: u64,
    ms: Option<u64>,
    queued_ms: u64,
    user: String,
    warm: Option<bool>,
    open_ms: Option<u64>,
    hits: Option<u64>,
    error: String,
    denied: bool,
    limit: bool,
    busy: bool,
    timed_out: bool,
    a: Analysis,
    repeat: bool,
    burst: bool,
}

pub fn run(args: &ReportArgs) -> Result<()> {
    let (events, files) = read_events(&args.activity_log)?;
    let report = build(args, events, files)?;
    fs::create_dir_all(&args.out).with_context(|| args.out.display().to_string())?;
    fs::write(args.out.join("report.json"), serde_json::to_string_pretty(&report)?)?;
    fs::write(args.out.join("report.html"), crate::report_html::render(&report))?;
    let t = &report["totals"];
    println!(
        "{} calls, {} searches, {} errors, {} identities → {}",
        t["calls"], t["searches"], t["errors"], t["identities"], args.out.join("report.html").display()
    );
    for n in report["notes"].as_array().into_iter().flatten() {
        println!("note: {}", n.as_str().unwrap_or(""));
    }
    Ok(())
}

/// The log and its rotated copies, query events only.
fn read_events(path: &Path) -> Result<(Vec<Value>, Vec<String>)> {
    let mut files = Vec::new();
    let mut texts = Vec::new();
    if path.exists() {
        texts.push(fs::read_to_string(path).with_context(|| path.display().to_string())?);
        files.push(path.display().to_string());
    }
    let base = path.display().to_string();
    for n in 1..=200 {
        let gz = PathBuf::from(format!("{base}.{n}.gz"));
        let plain = PathBuf::from(format!("{base}.{n}"));
        if gz.exists() {
            let mut s = String::new();
            GzDecoder::new(fs::File::open(&gz)?).read_to_string(&mut s).with_context(|| gz.display().to_string())?;
            texts.push(s);
            files.push(gz.display().to_string());
        } else if plain.exists() {
            texts.push(fs::read_to_string(&plain)?);
            files.push(plain.display().to_string());
        } else {
            break;
        }
    }
    if files.is_empty() {
        return Err(anyhow!(
            "{}: no activity log. Start `fqs serve` with --activity-log FILE (or FQS_ACTIVITY_LOG) to record one",
            path.display()
        ));
    }
    let mut out = Vec::new();
    for t in &texts {
        for line in t.lines() {
            if !line.contains("\"query\"") {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if v.get("event").and_then(Value::as_str) == Some("query") {
                    out.push(v);
                }
            }
        }
    }
    Ok((out, files))
}

/// RFC 3339 UTC (`2026-11-20T21:02:26.470Z`) → epoch seconds.
fn epoch_of(ts: &str) -> Option<f64> {
    let n = |a: usize, l: usize| -> Option<i64> { ts.get(a..a + l)?.parse::<i64>().ok() };
    let (y, m, d, h, mi, s) = (n(0, 4)?, n(5, 2)?, n(8, 2)?, n(11, 2)?, n(14, 2)?, n(17, 2)?);
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let mut t = (days * 86400 + h * 3600 + mi * 60 + s) as f64;
    if ts.as_bytes().get(19) == Some(&b'.') {
        let frac: String = ts[20..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(f) = format!("0.{frac}").parse::<f64>() {
            t += f;
        }
    }
    Some(t)
}

fn parse_since(s: &str) -> Result<Option<f64>> {
    let s = s.trim().to_ascii_lowercase();
    if s == "all" || s.is_empty() {
        return Ok(None);
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: f64 = num.parse().map_err(|_| anyhow!("--since: '{s}' (use 30d, 12h or all)"))?;
    let secs = match unit {
        "d" => 86400.0,
        "h" => 3600.0,
        "w" => 7.0 * 86400.0,
        _ => return Err(anyhow!("--since: '{s}' (use 30d, 12h or all)")),
    };
    Ok(Some(OffsetDateTime::now_utc().unix_timestamp() as f64 - n * secs))
}

fn flag(v: &Value, k: &str) -> bool {
    v.get(k).map(|x| x.as_bool().unwrap_or(x.as_u64().unwrap_or(0) > 0)).unwrap_or(false)
}

fn to_q(v: &Value) -> Option<Q> {
    let ts = v.get("ts")?.as_str()?;
    let t = epoch_of(ts)?;
    let query = v.get("query").and_then(Value::as_str).unwrap_or("").to_string();
    Some(Q {
        t,
        day: ts.get(..10)?.to_string(),
        hour: ts.get(11..13)?.parse().ok()?,
        corpus: v.get("corpus").and_then(Value::as_str).unwrap_or("?").to_string(),
        endpoint: v.get("endpoint").and_then(Value::as_str).unwrap_or("").to_string(),
        status: v.get("status").and_then(Value::as_u64).unwrap_or(0),
        ms: v.get("elapsed_ms").and_then(Value::as_u64),
        queued_ms: v.get("queued_ms").and_then(Value::as_u64).unwrap_or(0),
        user: v.get("user").and_then(Value::as_str).unwrap_or("-").to_string(),
        warm: v.get("warm").and_then(Value::as_bool),
        open_ms: v.get("open_ms").and_then(Value::as_u64),
        hits: v.get("hits").or_else(|| v.get("total")).and_then(Value::as_u64),
        error: v.get("error").and_then(Value::as_str).unwrap_or("").to_string(),
        denied: flag(v, "denied"),
        limit: flag(v, "limit"),
        busy: flag(v, "busy"),
        timed_out: flag(v, "timed_out"),
        a: analyse(&query),
        query,
        repeat: false,
        burst: false,
    })
}

fn failure_class(q: &Q) -> Option<&'static str> {
    if q.denied {
        return Some("access denied");
    }
    if q.limit {
        return Some("rate limited");
    }
    if q.busy {
        return Some("busy / queue full");
    }
    if q.timed_out {
        return Some("timed out");
    }
    if (200..300).contains(&q.status) {
        return None;
    }
    Some(match q.status {
        400 | 422 => {
            if !q.a.brackets_ok {
                "syntax error: unbalanced brackets"
            } else if !q.a.quotes_ok {
                "syntax error: unbalanced quotes"
            } else {
                "query error"
            }
        }
        401 | 403 => "access denied",
        404 => "unknown corpus or session",
        408 => "timed out",
        429 => "rate limited",
        503 => "busy / queue full",
        500..=599 => "server error",
        _ => "other error",
    })
}

fn pct(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    Some(sorted[(((sorted.len() - 1) as f64) * p).round() as usize])
}

fn top_n(m: HashMap<String, u64>, n: usize) -> Vec<(String, u64)> {
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

fn pairs(v: Vec<(String, u64)>, key: &str) -> Vec<Value> {
    v.into_iter().map(|(k, n)| json!({ key: k, "count": n })).collect()
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// Mark repeats (same user, corpus, pattern within the window) and bursts.
fn mark(qs: &mut [Q], repeat_secs: f64) {
    let mut last: HashMap<(String, String), f64> = HashMap::new();
    for q in qs.iter_mut() {
        let base = if q.a.base.is_empty() { q.query.clone() } else { q.a.base.clone() };
        // not keyed on the user: TEITOK sends a search and its freq follow-up under different identities
        let key = (q.corpus.clone(), base);
        if let Some(&t0) = last.get(&key) {
            q.repeat = q.t - t0 <= repeat_secs;
        }
        last.insert(key, q.t);
    }
    let mut by_user: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, q) in qs.iter().enumerate() {
        by_user.entry(q.user.as_str()).or_default().push(i);
    }
    let mut hits = Vec::new();
    for idx in by_user.values() {
        let mut run: Vec<usize> = Vec::new();
        for &i in idx {
            if run.last().map_or(true, |&j| qs[i].t - qs[j].t < 1.0) {
                run.push(i);
            } else {
                if run.len() >= 5 {
                    hits.extend(run.iter().copied());
                }
                run = vec![i];
            }
        }
        if run.len() >= 5 {
            hits.extend(run.iter().copied());
        }
    }
    for i in hits {
        qs[i].burst = true;
    }
}

fn build(args: &ReportArgs, raw: Vec<Value>, files: Vec<String>) -> Result<Value> {
    let cutoff = parse_since(&args.since)?;
    let mut qs: Vec<Q> = raw.iter().filter_map(to_q).collect();
    qs.retain(|q| cutoff.map_or(true, |c| q.t >= c));
    if let Some(c) = &args.corpus {
        qs.retain(|q| q.corpus.eq_ignore_ascii_case(c));
    }
    qs.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
    mark(&mut qs, args.repeat_secs);
    let top = args.top;

    let calls = qs.len() as u64;
    let searches = qs.iter().filter(|q| !q.repeat).count() as u64;
    let errors = qs.iter().filter(|q| failure_class(q).is_some()).count() as u64;
    let burst_calls = qs.iter().filter(|q| q.burst).count() as u64;
    let identities: HashSet<&str> = qs.iter().map(|q| q.user.as_str()).collect();

    // per day / per hour (hours: UTC)
    let mut days: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
    let mut hours = [0u64; 24];
    for q in &qs {
        let d = days.entry(q.day.clone()).or_default();
        d.0 += 1;
        if !q.repeat {
            d.1 += 1;
        }
        if failure_class(q).is_some() {
            d.2 += 1;
        }
        if !q.repeat && !q.burst && q.hour < 24 {
            hours[q.hour] += 1;
        }
    }

    // per corpus
    let mut per_corpus: BTreeMap<&str, Vec<&Q>> = BTreeMap::new();
    for q in &qs {
        per_corpus.entry(q.corpus.as_str()).or_default().push(q);
    }
    let mut corpora: Vec<Value> = per_corpus
        .iter()
        .map(|(c, v)| {
            let mut ms: Vec<u64> = v.iter().filter(|q| failure_class(q).is_none()).filter_map(|q| q.ms).collect();
            ms.sort_unstable();
            let opens: Vec<u64> = v.iter().filter(|q| q.warm == Some(false)).filter_map(|q| q.open_ms).collect();
            json!({
                "corpus": c, "calls": v.len(), "searches": v.iter().filter(|q| !q.repeat).count(),
                "errors": v.iter().filter(|q| failure_class(q).is_some()).count(),
                "p50_ms": pct(&ms, 0.5), "p90_ms": pct(&ms, 0.9), "p99_ms": pct(&ms, 0.99),
                "cold_opens": v.iter().filter(|q| q.warm == Some(false)).count(),
                "avg_open_ms": if opens.is_empty() { None } else { Some(opens.iter().sum::<u64>() / opens.len() as u64) },
                "queued": v.iter().filter(|q| q.queued_ms > 0).count(),
            })
        })
        .collect();
    corpora.sort_by(|a, b| b["calls"].as_u64().cmp(&a["calls"].as_u64()));

    // typical queries: real searches only (no repeats, no scripted bursts)
    struct Sh {
        count: u64,
        users: HashSet<String>,
        example: String,
        ms: Vec<u64>,
        errors: u64,
    }
    let mut shapes: HashMap<String, Sh> = HashMap::new();
    let mut literals: HashMap<String, u64> = HashMap::new();
    let mut kinds: HashMap<&'static str, u64> = HashMap::new();
    let mut endpoints: HashMap<String, u64> = HashMap::new();
    for q in qs.iter().filter(|q| !q.repeat && !q.burst) {
        *kinds.entry(q.a.kind.name()).or_default() += 1;
        *endpoints.entry(if q.endpoint.is_empty() { "?".into() } else { q.endpoint.clone() }).or_default() += 1;
        let key = if q.a.shape.is_empty() { clip(&q.query, 120) } else { q.a.shape.clone() };
        let s = shapes.entry(key).or_insert_with(|| Sh { count: 0, users: HashSet::new(), example: q.a.base.clone(), ms: vec![], errors: 0 });
        s.count += 1;
        s.users.insert(q.user.clone());
        if let Some(m) = q.ms {
            s.ms.push(m);
        }
        if failure_class(q).is_some() {
            s.errors += 1;
        }
        if q.a.kind != Kind::Aggregation || !q.a.base.is_empty() {
            *literals.entry(q.a.base.clone()).or_default() += 1;
        }
    }
    let mut shape_rows: Vec<(&String, &Sh)> = shapes.iter().collect();
    shape_rows.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
    let shape_json: Vec<Value> = shape_rows
        .iter()
        .take(top)
        .map(|(k, s)| {
            json!({
                "shape": clip(k, 200), "count": s.count, "identities": s.users.len(),
                "example": clip(&s.example, 200),
                "avg_ms": if s.ms.is_empty() { None } else { Some(s.ms.iter().sum::<u64>() / s.ms.len() as u64) },
                "errors": s.errors,
            })
        })
        .collect();
    let lit_json = pairs(top_n(literals, top).into_iter().map(|(k, n)| (clip(&k, 200), n)).collect(), "query");

    // failures by class, then the commonest cases inside each
    let mut classes: BTreeMap<&'static str, (u64, u64, HashMap<String, u64>)> = BTreeMap::new();
    for q in &qs {
        if let Some(c) = failure_class(q) {
            let e = classes.entry(c).or_default();
            e.0 += 1;
            if q.burst {
                e.1 += 1;
            }
            let what = if !q.error.is_empty() { clip(&q.error, 160) } else { clip(&q.query, 160) };
            *e.2.entry(what).or_default() += 1;
        }
    }
    let mut failures: Vec<Value> = classes
        .into_iter()
        .map(|(c, (n, b, ex))| json!({ "class": c, "count": n, "in_bursts": b, "examples": pairs(top_n(ex, 5), "what") }))
        .collect();
    failures.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));

    // zero-hit searches, slow queries
    let mut zero: HashMap<String, u64> = HashMap::new();
    for q in qs.iter().filter(|q| !q.repeat && !q.burst && failure_class(q).is_none() && q.hits == Some(0)) {
        *zero.entry(clip(&q.a.shape, 160)).or_default() += 1;
    }
    let zero_total: u64 = zero.values().sum();
    let mut slow: Vec<&Q> = qs.iter().filter(|q| failure_class(q).is_none() && q.ms.is_some()).collect();
    slow.sort_by(|a, b| b.ms.cmp(&a.ms));
    let slow_json: Vec<Value> = slow
        .iter()
        .take(top.min(10))
        .map(|q| json!({ "ms": q.ms, "corpus": q.corpus, "when": q.day, "query": clip(&q.query, 200) }))
        .collect();

    // identities
    let mut by_user: HashMap<String, u64> = HashMap::new();
    let mut kinds_u: HashMap<&'static str, u64> = HashMap::new();
    for q in &qs {
        *by_user.entry(q.user.clone()).or_default() += 1;
    }
    for u in &identities {
        let k = if u.starts_with("ip:") {
            "ip (the calling host)"
        } else if u.starts_with("session:") {
            "session"
        } else if u.is_empty() || *u == "-" {
            "none"
        } else {
            "named or hashed"
        };
        *kinds_u.entry(k).or_default() += 1;
    }
    let users_json: Vec<Value> = top_n(by_user, top)
        .into_iter()
        .map(|(u, n)| json!({ "identity": u, "calls": n, "share": if calls > 0 { n as f64 / calls as f64 } else { 0.0 } }))
        .collect();

    // bursts
    let mut bursts: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < qs.len() {
        if qs[i].burst {
            let u = qs[i].user.clone();
            let mut j = i;
            while j + 1 < qs.len() && qs[j + 1].burst && qs[j + 1].user == u && qs[j + 1].t - qs[j].t < 1.0 {
                j += 1;
            }
            let grp = &qs[i..=j];
            bursts.push(json!({
                "start": grp[0].day, "identity": u, "calls": grp.len(),
                "errors": grp.iter().filter(|q| failure_class(q).is_some()).count(),
                "seconds": ((grp[grp.len() - 1].t - grp[0].t) * 100.0).round() / 100.0,
                "example": clip(&grp[0].query, 160),
            }));
            i = j + 1;
        } else {
            i += 1;
        }
    }

    let mut notes: Vec<String> = Vec::new();
    if calls == 0 {
        notes.push("No queries in this period.".into());
    }
    if calls > 20 && identities.len() <= 2 {
        notes.push(format!(
            "Only {} identit{} in {} calls: either there are few users, or the front-end passes the same identity for everyone.",
            identities.len(),
            if identities.len() == 1 { "y" } else { "ies" },
            calls
        ));
    }
    if kinds_u.contains_key("ip (the calling host)") {
        notes.push("Some identities are `ip:` the address of the calling server (e.g. the TEITOK host), not end users.".into());
    }
    if burst_calls > 0 {
        notes.push(format!("{burst_calls} calls came in scripted bursts (5+ within a second); they are left out of the typical-query tables."));
    }
    notes.push("Identities are as logged; hashed ones change at each server restart unless `--activity-salt` is fixed, so they only compare within one run.".into());

    let from = qs.first().map(|q| q.day.clone());
    let to = qs.last().map(|q| q.day.clone());
    Ok(json!({
        "generated": OffsetDateTime::now_utc().to_string(),
        "source": { "files": files, "corpus_filter": args.corpus, "since": args.since, "repeat_secs": args.repeat_secs },
        "window": { "from": from, "to": to, "days_with_queries": days.len() },
        "totals": {
            "calls": calls, "searches": searches, "repeats": calls - searches, "errors": errors,
            "error_rate": if calls > 0 { errors as f64 / calls as f64 } else { 0.0 },
            "burst_calls": burst_calls, "identities": identities.len(), "zero_hit_searches": zero_total,
        },
        "per_day": days.into_iter().map(|(d, (c, s, e))| json!({ "day": d, "calls": c, "searches": s, "errors": e })).collect::<Vec<_>>(),
        "per_hour_utc": hours,
        "corpora": corpora,
        "kinds": pairs(top_n(kinds.into_iter().map(|(k, n)| (k.to_string(), n)).collect(), 10), "kind"),
        "endpoints": pairs(top_n(endpoints, 10), "endpoint"),
        "shapes": shape_json,
        "queries": lit_json,
        "failures": failures,
        "zero_hits": pairs(top_n(zero, top), "shape"),
        "slow": slow_json,
        "identity_kinds": pairs(top_n(kinds_u.into_iter().map(|(k, n)| (k.to_string(), n)).collect(), 10), "kind"),
        "identities": users_json,
        "bursts": bursts,
        "notes": notes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: &str, q: &str, status: u64, ms: u64, user: &str) -> Value {
        json!({"event":"query","ts":ts,"corpus":"ud219","endpoint":"/query","query":q,"status":status,"elapsed_ms":ms,"user":user})
    }

    fn args() -> ReportArgs {
        ReportArgs { activity_log: "x".into(), since: "all".into(), out: "x".into(), corpus: None, repeat_secs: 10.0, top: 10 }
    }

    #[test]
    fn epoch_is_right() {
        assert_eq!(epoch_of("1970-01-02T00:00:00Z"), Some(86400.0));
        let a = epoch_of("2026-11-20T21:02:26.5Z").unwrap();
        let b = epoch_of("2026-11-20T21:02:26.0Z").unwrap();
        assert!((a - b - 0.5).abs() < 1e-6);
    }

    #[test]
    fn repeats_bursts_and_failures() {
        let mut raw = vec![
            ev("2026-11-19T14:15:23.7Z", r#"[upos="VERB"] > [deprel="nsubj"]"#, 200, 15, "u1"),
            ev("2026-11-19T14:15:25.9Z", r#"[upos="VERB"] > [deprel="nsubj"]"#, 200, 3, "u1"),
            ev("2026-11-19T14:16:00.0Z", r#"[upos="NOUN"] > [deprel="obj"]"#, 200, 9, "u2"),
        ];
        for i in 0..5 {
            raw.push(ev(&format!("2026-11-20T21:02:26.{i}00Z"), r#"[upos="VERB""#, 400, 0, "u1"));
        }
        let r = build(&args(), raw, vec![]).unwrap();
        assert_eq!(r["totals"]["calls"], 8);
        assert_eq!(r["totals"]["searches"], 3); // u1, u2, and the first of the burst
        assert_eq!(r["totals"]["repeats"], 5);
        assert_eq!(r["totals"]["burst_calls"], 5);
        assert_eq!(r["totals"]["errors"], 5);
        assert_eq!(r["failures"][0]["class"], "syntax error: unbalanced brackets");
        assert_eq!(r["failures"][0]["in_bursts"], 5);
        // bursts are not in the typical shapes: VERB>nsubj and NOUN>obj share one shape
        assert_eq!(r["shapes"].as_array().unwrap().len(), 1);
        assert_eq!(r["shapes"][0]["count"], 2);
        assert_eq!(r["bursts"].as_array().unwrap().len(), 1);
    }
}
