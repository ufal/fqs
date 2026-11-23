//! HTML for `fqs report`: one self-contained page (inline CSS, bars as divs),
//! rendered from the report JSON. No scripts, no external files.

use serde_json::Value;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn txt(v: &Value) -> String {
    match v {
        Value::Null => "–".into(),
        Value::String(s) => esc(s),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => format!("{:.1}", f),
            _ => n.to_string(),
        },
        other => esc(&other.to_string()),
    }
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(Value::as_array).map(|a| a.as_slice()).unwrap_or(&[])
}

/// `cols`: (header, key, is_code). Empty tables say so.
fn table(rows: &[Value], cols: &[(&str, &str, bool)]) -> String {
    if rows.is_empty() {
        return "<p class=none>nothing to show</p>".into();
    }
    let mut h = String::from("<table><tr>");
    for (name, _, _) in cols {
        h.push_str(&format!("<th>{}</th>", esc(name)));
    }
    h.push_str("</tr>");
    for r in rows {
        h.push_str("<tr>");
        for (_, k, code) in cols {
            let c = txt(&r[*k]);
            h.push_str(&if *code { format!("<td><code>{c}</code></td>") } else { format!("<td>{c}</td>") });
        }
        h.push_str("</tr>");
    }
    h.push_str("</table>");
    h
}

fn bars(rows: &[(String, f64)], unit: &str) -> String {
    let max = rows.iter().map(|r| r.1).fold(0.0, f64::max).max(1.0);
    let mut h = String::from("<div class=bars>");
    for (l, v) in rows {
        h.push_str(&format!(
            "<div class=bar><span class=bl>{}</span><span class=bb><i style=\"width:{:.1}%\"></i></span><span class=bv>{}{}</span></div>",
            esc(l),
            100.0 * v / max,
            *v as u64,
            unit
        ));
    }
    h.push_str("</div>");
    h
}

pub fn render(r: &Value) -> String {
    let t = &r["totals"];
    let mut h = String::new();
    h.push_str("<!doctype html><meta charset=utf-8><title>FQS usage report</title><style>");
    h.push_str(
        "body{font:15px/1.45 system-ui,sans-serif;max-width:1000px;margin:2em auto;padding:0 1em;color:#222}\
         h1{margin-bottom:0}h2{margin-top:2em;border-bottom:1px solid #ccc;padding-bottom:.2em}\
         .sub{color:#666}.cards{display:flex;gap:1em;flex-wrap:wrap;margin:1em 0}\
         .card{border:1px solid #ddd;border-radius:6px;padding:.6em 1em;min-width:120px}\
         .card b{display:block;font-size:1.6em}table{border-collapse:collapse;width:100%;margin:.5em 0}\
         th,td{text-align:left;padding:.25em .6em;border-bottom:1px solid #eee;vertical-align:top}\
         th{background:#f6f6f6}code{font-size:.85em;word-break:break-all}.none{color:#888}\
         .bar{display:flex;align-items:center;gap:.5em;font-size:.85em}.bl{width:6.5em;text-align:right;color:#555}\
         .bb{flex:1;background:#f0f0f0;height:.9em}.bb i{display:block;height:100%;background:#4a7ab5}\
         .bv{width:4em}.note{background:#fff7e0;border:1px solid #ecd9a0;padding:.4em .8em;margin:.3em 0;border-radius:4px}",
    );
    h.push_str("</style>");
    h.push_str("<h1>FQS usage report</h1>");
    h.push_str(&format!(
        "<p class=sub>{} to {} · {} days with queries · generated {}</p>",
        txt(&r["window"]["from"]),
        txt(&r["window"]["to"]),
        txt(&r["window"]["days_with_queries"]),
        txt(&r["generated"])
    ));
    for n in arr(r, "notes") {
        h.push_str(&format!("<div class=note>{}</div>", txt(n)));
    }
    h.push_str("<div class=cards>");
    for (label, key) in [("calls", "calls"), ("searches", "searches"), ("repeats", "repeats"), ("errors", "errors"), ("identities", "identities"), ("zero-hit searches", "zero_hit_searches")] {
        h.push_str(&format!("<div class=card><b>{}</b>{}</div>", txt(&t[key]), label));
    }
    h.push_str(&format!("<div class=card><b>{:.1}%</b>error rate</div></div>", t["error_rate"].as_f64().unwrap_or(0.0) * 100.0));

    h.push_str("<h2>Over time</h2>");
    let days: Vec<(String, f64)> = arr(r, "per_day")
        .iter()
        .map(|d| (d["day"].as_str().unwrap_or("").to_string(), d["searches"].as_f64().unwrap_or(0.0)))
        .collect();
    h.push_str("<p>Searches per day</p>");
    h.push_str(&bars(&days, ""));
    let hours: Vec<(String, f64)> = arr(r, "per_hour_utc").iter().enumerate().map(|(i, v)| (format!("{i:02}:00"), v.as_f64().unwrap_or(0.0))).collect();
    h.push_str("<p>Searches by hour of day (UTC)</p>");
    h.push_str(&bars(&hours, ""));

    h.push_str("<h2>Corpora and speed</h2>");
    h.push_str(&table(arr(r, "corpora"), &[("corpus", "corpus", false), ("calls", "calls", false), ("searches", "searches", false), ("errors", "errors", false), ("p50 ms", "p50_ms", false), ("p90 ms", "p90_ms", false), ("p99 ms", "p99_ms", false), ("cold opens", "cold_opens", false), ("avg open ms", "avg_open_ms", false), ("queued", "queued", false)]));

    h.push_str("<h2>Typical queries</h2><p class=sub>One row per query shape: literals in quotes are replaced by <code>?</code>. Repeats and scripted bursts are not counted.</p>");
    h.push_str(&table(arr(r, "shapes"), &[("shape", "shape", true), ("searches", "count", false), ("identities", "identities", false), ("avg ms", "avg_ms", false), ("errors", "errors", false), ("example", "example", true)]));
    h.push_str("<h3>Most repeated exact queries</h3>");
    h.push_str(&table(arr(r, "queries"), &[("query", "query", true), ("searches", "count", false)]));
    h.push_str("<h3>Kind and entry point</h3>");
    h.push_str(&table(arr(r, "kinds"), &[("kind", "kind", false), ("searches", "count", false)]));
    h.push_str(&table(arr(r, "endpoints"), &[("endpoint", "endpoint", false), ("searches", "count", false)]));

    h.push_str("<h2>Failures</h2>");
    for f in arr(r, "failures") {
        h.push_str(&format!("<h3>{} — {} ({} in bursts)</h3>", txt(&f["class"]), txt(&f["count"]), txt(&f["in_bursts"])));
        h.push_str(&table(arr(f, "examples"), &[("commonest cases", "what", true), ("times", "count", false)]));
    }
    if arr(r, "failures").is_empty() {
        h.push_str("<p class=none>no failed queries</p>");
    }
    h.push_str("<h3>Searches without hits</h3>");
    h.push_str(&table(arr(r, "zero_hits"), &[("shape", "shape", true), ("times", "count", false)]));
    h.push_str("<h3>Slowest answered queries</h3>");
    h.push_str(&table(arr(r, "slow"), &[("ms", "ms", false), ("corpus", "corpus", false), ("day", "when", false), ("query", "query", true)]));

    h.push_str("<h2>Who</h2>");
    h.push_str(&table(arr(r, "identity_kinds"), &[("identity kind", "kind", false), ("identities", "count", false)]));
    h.push_str(&table(arr(r, "identities"), &[("identity", "identity", true), ("calls", "calls", false), ("share", "share", false)]));

    h.push_str("<h2>Scripted bursts</h2>");
    h.push_str(&table(arr(r, "bursts"), &[("day", "start", false), ("identity", "identity", true), ("calls", "calls", false), ("errors", "errors", false), ("seconds", "seconds", false), ("first query", "example", true)]));

    h.push_str("<h2>Source</h2><ul>");
    for f in arr(&r["source"], "files") {
        h.push_str(&format!("<li><code>{}</code></li>", txt(f)));
    }
    h.push_str("</ul>");
    h
}
