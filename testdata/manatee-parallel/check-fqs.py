#!/usr/bin/env python3
"""Aligned queries through an FQS `/query` against reference.json (Manatee's Python module, as KonText
reads the same data: reference.py). The FQS needs native Manatee (FQS_MANATEE_LIB, fqs-manatee 0.5+)
and the four test corpora in its catalogue (build.sh, then `fqs corpora import` with a corplist that
lists them, or rows of your own: preferred_backend manatee, settings.manatee_registry).

    python3 testdata/manatee-parallel/check-fqs.py [http://127.0.0.1:8787]

Per query: the total, every hit's position, per aligned corpus its segment (KonText's add_aligned)
or, for a corpus the query has a `within CORPUS:` condition on, the matched part ("match"). Also an
error case (a name that is not aligned). Exit status 1 on any difference.
"""
import json
import os
import sys
import urllib.error
import urllib.request

URL = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8787").rstrip("/")
REF = json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "reference.json")))


def query(body):
    req = urllib.request.Request(URL + "/query", data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:   # query errors come as plain text
        text = e.read().decode("utf-8", "replace")
        try:
            return e.code, json.loads(text)
        except ValueError:
            return e.code, {"error": text}


def find_result(ans):
    """the manatee result object wherever /query nests it"""
    stack = [ans]
    while stack:
        x = stack.pop()
        if isinstance(x, dict):
            if "hits" in x and "total" in x:
                return x
            stack.extend(x.values())
    return {}


bad = 0
for q in REF:
    names = list(q["hits"][0]["aligned"]) if q["hits"] else ["*"]
    status, ans = query({"corpus": q["corpus"], "query": q["cql"], "language": "manatee-cql", "backend": "manatee", "size": 50, "aligned": names})
    r = find_result(ans)
    problems = []
    if status != 200:
        problems.append(f"HTTP {status}: {ans.get('error')}")
    if r.get("total") != q["total"]:
        problems.append(f"total {r.get('total')} != {q['total']}")
    if r.get("aligned_corpora") != names:
        problems.append(f"aligned_corpora {r.get('aligned_corpora')}")
    for mine, theirs in zip(r.get("hits", []), q["hits"]):
        if (mine.get("match_start"), mine.get("match_end") + 1) != (theirs["pos"], theirs["end"]):
            problems.append(f"hit {mine.get('match_start')} != {theirs['pos']}")
        for a in mine.get("aligned", []):
            t = theirs["aligned"][a["corpus"]]
            if t is None:
                if a["pos"] is not None or a["tokens"]:
                    problems.append(f"{a['corpus']}: a segment where Manatee has none")
            elif t["labelled"]:
                if a.get("match") != {"pos": t["pos"], "end": t["end"]}:
                    problems.append(f"{a['corpus']}: match {a.get('match')} != {t['pos']}..{t['end']}")
            elif (a["pos"], a["end"], a["text"]) != (t["pos"], t["end"], t["text"]):
                problems.append(f"{a['corpus']}: {a['pos']}..{a['end']} '{a['text']}' != {t['pos']}..{t['end']} '{t['text']}'")
    bad += bool(problems)
    print("ok  " if not problems else "BAD ", q["corpus"], q["cql"], r.get("total"), "; ".join(problems))

status, ans = query({"corpus": "par_cs", "query": '[lemma="pes"]', "language": "manatee-cql", "backend": "manatee", "aligned": ["par_de"]})
err = json.dumps(ans)
ok = status >= 400 and "not aligned" in err
bad += not ok
print("ok  " if ok else "BAD ", "par_cs aligned par_de (not aligned): an error", status)
status, ans = query({"corpus": "par_cs", "query": '[lemma="pes"]', "language": "manatee-cql", "backend": "manatee"})
ok = status == 200 and all("aligned" not in h for h in find_result(ans).get("hits", [])) and "aligned_corpora" not in find_result(ans)
bad += not ok
print("ok  " if ok else "BAD ", "par_cs without aligned: no aligned fields", status)
print("differences:", bad)
sys.exit(1 if bad else 0)
