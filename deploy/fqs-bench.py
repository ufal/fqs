#!/usr/bin/env python3
"""Speed comparison through an FQS: the same queries on several corpora (typically one corpus compiled
for different engines: pando, native Manatee, CWB), as a user of FQS sees them:

    python3 fqs/deploy/fqs-bench.py [URL] --corpora ud_manatee,kontext:ud_manatee,ud_pando
        [--kontext-url http://127.0.0.1:8080] [--secret-file FILE] [--queries FILE] [--only Q1,Q4]
        [--repeat 2] [--manatee-word word] [--json]

`kontext:NAME` is corpus NAME through KonText's own interface (query_submit, then view and
get_conc_cache_status), timed from here: what a KonText user waits for, Manatee via KonText. KonText
keeps concordances in its cache; every run adds spaces after the query's first "[" so that each run
computes (KonText trims only the ends; Manatee ignores the spaces).

Per query and corpus: first (FQS's time to the first page, the first time this query runs),
count (to the exact total: pando counts in the background, polled via /status; Manatee counts before
it answers), again (the best of --repeat reruns: file cache, and pando's result cache), and the hits.
All times are as a client waits for them (a tunnel adds its latency to every column alike). Pando
keeps results in memory: restart FQS before a run, or "first" is a cache hit too; "again" for pando is
that cache.

The queries: the "fair" set of the kontext-pando benchmarks (Q1–Q21), or --queries FILE with lines
"Qn<TAB>query". Pando calls the token form `form`, Manatee corpora mostly `word`: --manatee-word
renames `form=` in queries to Manatee corpora (default word; "" leaves them). Not cold in the OS sense:
the operating system's file cache is not flushed (on a Mac: `sudo purge` before a run for that).
"""
import argparse
import importlib.util
import json
import os
import re
import sys
import time
import urllib.parse
import urllib.request

_spec = importlib.util.spec_from_file_location("fqs_smoke", os.path.join(os.path.dirname(os.path.abspath(__file__)), "fqs-smoke.py"))
smoke = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(smoke)

FAIR = [
    ("Q1", '[upos="NOUN"]'),
    ("Q4", '[lemma="be"]'),
    ("Q5", '[form="the"]'),
    ("Q6", '[form="gezelligheid"]'),
    ("Q7", '"the"'),
    ("Q8", '[upos!="PUNCT"]'),
    ("Q9", '[upos="NOUN" & lemma="time"]'),
    ("Q10", '[upos="ADJ" | upos="NOUN"]'),
    ("Q11", '[lemma="un.*"]'),
    ("Q12", '[upos="DET"] [upos="NOUN"]'),
    ("Q13", '[upos="DET"] [upos="ADJ"] [upos="NOUN"]'),
    ("Q14", '[form="the"] [upos="NOUN"]'),
    ("Q15", '[upos="PRON"] [upos="VERB"] [upos="ADV"]'),
    ("Q19", '[upos="NOUN"] [upos="VERB"] within s'),
    ("Q21", '[upos="ADJ"]'),
]


def job_of(payload):
    """The pando count job ({id, finished, total, elapsed_ms, …}) in an answer, if any."""
    for p, v, parent in smoke.find(payload, "state"):
        if "job" in p and isinstance(parent, dict) and "id" in parent:
            return parent
    return None


def run(fqs, cid, engine, query, timeout, again=False):
    """{first, count, hits, exact, error}: one query, waiting for the exact total."""
    body = {"corpus": cid, "query": query, "language": "auto", "start": 0, "size": 20, "window": 5, "request_role": fqs.role}
    t0 = time.time()
    st, p, rt = fqs.call("POST", "/query", body, corpus=cid, timeout=timeout)
    err = smoke.error_of(st, p)
    if err:
        return {"error": err}
    first = rt   # as a client waits for it, like the KonText columns
    hits, exact = smoke.hits_of(p)
    count = first
    job = job_of(p) if engine == "pando" else None
    if job and not job.get("finished"):
        t_end = time.time() + timeout
        while time.time() < t_end:
            time.sleep(0.05)
            qs = urllib.parse.urlencode({"corpus": cid, "job": job["id"]})
            st, s, _ = fqs.call("GET", "/status?" + qs, corpus=cid, timeout=30)
            j = job_of(s) or (s if isinstance(s, dict) and "finished" in s else None)
            if j and j.get("finished"):
                job = j
                break
        hits, exact = job.get("total"), bool(job.get("finished"))
        count = (time.time() - t0) * 1000
    return {"first": first, "count": count, "hits": hits, "exact": exact, "round_trip": rt}


class Kontext:
    """Manatee through KonText's HTTP interface (anonymous; a local or a tunnelled KonText)."""

    def __init__(self, url):
        self.url, self.runs = url.rstrip("/"), 0

    def call(self, method, path, body=None, timeout=600):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.url + path, data=data, method=method, headers={"Content-Type": "application/json", "Accept": "application/json"})
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.load(r)

    def run(self, corp, query, timeout):
        self.runs += 1
        # a new cache key per run: KonText trims a query's ends, so the spaces go after the first "["
        pad = " " * max(0, self.runs)
        q = query.replace("[", "[" + pad, 1) if "[" in query else query + pad
        body = {"type": "concQueryArgs", "maincorp": corp, "usesubcorp": None, "viewmode": "kwic", "pagesize": 20,
                "attrs": ["word"], "attr_vmode": "visible-kwic", "base_viewattr": "word", "ctxattrs": [], "structs": [],
                "refs": [], "fromp": 0, "shuffle": 0, "text_types": {}, "async": True,
                "queries": [{"qtype": "advanced", "corpname": corp, "query": q, "pcq_pos_neg": "pos", "include_empty": False, "default_attr": "word"}]}
        t0 = time.time()
        try:
            d = self.call("POST", "/query_submit?format=json", body, timeout)
            msgs = [m for m in d.get("messages") or [] if m and m[0] == "error"]
            if msgs or not d.get("Q"):
                return {"error": str(msgs[0][1] if msgs else "no result")[:100]}
            qid = urllib.parse.quote(d["Q"][0])
            v = self.call("GET", f"/view?corpname={corp}&q={qid}&format=json&pagesize=20", timeout=timeout)
            first = (time.time() - t0) * 1000
            done, size = bool(v.get("finished")), v.get("concsize")
            while not done and time.time() - t0 < timeout:
                time.sleep(0.05)
                st = self.call("GET", f"/get_conc_cache_status?corpname={corp}&q={qid}&format=json", timeout=timeout)
                done, size = bool(st.get("finished")), st.get("concsize", size)
            count = (time.time() - t0) * 1000
        except Exception as e:
            return {"error": f"KonText: {e}"[:100]}
        return {"first": first, "count": count, "hits": size, "exact": done, "round_trip": count}


def fmt_ms(v):
    if v is None:
        return "--"
    return f"{v / 1000:.3f}" if v >= 1000 else f"{v:.1f}ms" if v < 10 else f"{v:.0f}ms"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("url", nargs="?", default="http://127.0.0.1:8787")
    ap.add_argument("--corpora", required=True, help="comma-separated corpus ids, compared side by side")
    ap.add_argument("--secret-file")
    ap.add_argument("--kontext-url", default="http://127.0.0.1:8080", help="for kontext:NAME columns")
    ap.add_argument("--queries", help="a file of 'Qn<TAB>query' lines (default: the fair set)")
    ap.add_argument("--only", help="comma-separated query ids")
    ap.add_argument("--repeat", type=int, default=2)
    ap.add_argument("--manatee-word", default="word")
    ap.add_argument("--timeout", type=int, default=600)
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()
    secret = ""
    if a.secret_file:
        for line in open(a.secret_file, encoding="utf-8"):
            if line.startswith("FQS_SECRET="):
                secret = line.split("=", 1)[1].strip().strip("\"'")
    fqs = smoke.Fqs(a.url, secret, "admin" if secret else "visitor")

    queries = FAIR
    if a.queries:
        queries = [tuple(l.rstrip("\n").split("\t", 1)) for l in open(a.queries, encoding="utf-8") if "\t" in l and not l.startswith("#")]
    if a.only:
        keep = {x.strip() for x in a.only.split(",")}
        queries = [q for q in queries if q[0] in keep]

    st, cat, _ = fqs.call("GET", "/corpora?include_noncurrent=true&request_role=" + fqs.role)
    engines = {r.get("id"): r.get("preferred_backend") for r in (cat.get("corpora", []) if isinstance(cat, dict) else [])}
    ids = [c.strip() for c in a.corpora.split(",") if c.strip()]
    kontext = Kontext(a.kontext_url)
    for c in ids:
        if c.startswith("kontext:"):
            engines[c] = "kontext"
    missing = [c for c in ids if c not in engines]
    if missing:
        sys.exit(f"not in the catalogue of {fqs.url}: {', '.join(missing)}")

    results = []
    if not a.json:
        print(f"FQS {fqs.url}; times: first page / exact total / again (best of {a.repeat}); seconds unless ms\n")
        head = f"{'Q':4s} {'query':42s}" + "".join(f" | {c[:30]:^34s}" for c in ids)
        print(head)
        print(f"{'':4s} {'':42s}" + "".join(f" | {'first':>7s} {'count':>7s} {'again':>7s} {'hits':>10s}" for _ in ids))
        print("-" * len(head))
    for qid, query in queries:
        row = {"id": qid, "query": query, "corpora": {}}
        cells = []
        for cid in ids:
            eng = engines[cid]
            q = query
            if eng in ("manatee", "kontext"):
                if a.manatee_word:
                    q = re.sub(r"\bform(\s*!?=)", a.manatee_word + r"\1", q)
                q = re.sub(r"\bwithin\s+(\w+)\s*$", r"within <\1/>", q)   # Manatee: within <s/>
            once = (lambda again=False: kontext.run(cid.split(":", 1)[1], q, a.timeout)) if eng == "kontext" else (lambda again=False: run(fqs, cid, eng, q, a.timeout, again))
            r = once()
            again = []
            if not r.get("error"):
                for _ in range(max(0, a.repeat)):
                    rr = once(again=True)
                    if rr.get("count") is not None:
                        again.append(rr["count"])
            r["again"] = min(again) if again else None
            r["engine"], r["query_sent"] = eng, q
            row["corpora"][cid] = r
            if r.get("error"):
                cells.append(f" | {r['error'][:34]:34s}")
            else:
                hits = "--" if r.get("hits") is None else f"{r['hits']:,}" + ("" if r.get("exact") else "+")
                cells.append(f" | {fmt_ms(r['first']):>7s} {fmt_ms(r['count']):>7s} {fmt_ms(r['again']):>7s} {hits:>10s}")
        results.append(row)
        if not a.json:
            print(f"{qid:4s} {query[:42]:42s}" + "".join(cells), flush=True)
    if a.json:
        print(json.dumps({"url": fqs.url, "corpora": {c: engines[c] for c in ids}, "queries": results}, indent=1))


if __name__ == "__main__":
    main()
