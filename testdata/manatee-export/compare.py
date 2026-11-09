#!/usr/bin/env python3
"""Compare a Manatee corpus with pando corpora made from it: the same queries (Manatee CQL) counted by
FQS's native Manatee and by pando on each index (e.g. one built from the original vertical, one from
`fqs manatee-export`). Rows that differ are marked <<.

    python3 compare.py --fqs http://127.0.0.1:8787 --corpus ortofon_m \\
        --pando /usr/local/bin/pando --index vert=/data/idx-vert --index export=/data/idx-export \\
        --query '[lemma="zítra"]' --query '<doc/>' ...   (or --queries FILE, one per line)

Without --query/--queries only a few generic ones are run ([], <s/>, <doc/>, …). For a real test,
give queries that touch every attribute and structure of the corpus, and its odd corners (empty
values, nested or unclosed structures, multivalue attributes, sequences).
FQS is asked anonymously; a corpus that needs a role takes --token (an FQS token).
"""
import argparse
import json
import subprocess
import urllib.error
import urllib.request

GENERIC = ['[]', '<s/>', '<doc/>', '<p/>', '[word=".*a"]', '[lemma="a.*"]']


def manatee(fqs, corpus, q, token):
    body = json.dumps({"corpus": corpus, "query": q, "language": "manatee", "start": 0, "size": 1}).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    try:
        with urllib.request.urlopen(urllib.request.Request(fqs.rstrip("/") + "/query", data=body, headers=headers), timeout=600) as r:
            d = json.load(r)
    except urllib.error.HTTPError as e:
        return "HTTP %d" % e.code
    res = d.get("raw", {}).get("done", {}).get("result", {})
    return res.get("total", d.get("error") or "?")


def pando(binary, index, q):
    out = subprocess.run([binary, "--api", "--cql", "manatee", "--total", "--limit", "1", index, q], capture_output=True, text=True)
    try:
        d = json.loads(out.stdout)
    except ValueError:
        return (out.stderr or out.stdout).strip()[:60]
    if not d.get("ok"):
        e = d.get("error")
        return (e.get("message") if isinstance(e, dict) else str(e))[:60]
    return d["result"]["page"]["total"]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--fqs", default="http://127.0.0.1:8787")
    ap.add_argument("--corpus", required=True, help="the Manatee corpus's FQS id")
    ap.add_argument("--pando", default="pando")
    ap.add_argument("--index", action="append", default=[], metavar="NAME=DIR", help="a pando corpus (repeatable)")
    ap.add_argument("--query", action="append", default=[])
    ap.add_argument("--queries", metavar="FILE")
    ap.add_argument("--token")
    a = ap.parse_args()
    queries = list(a.query)
    if a.queries:
        queries += [l.strip() for l in open(a.queries, encoding="utf-8") if l.strip() and not l.startswith("#")]
    queries = queries or GENERIC
    idx = [i.split("=", 1) for i in a.index]
    w = max(len(q) for q in queries) + 2
    print("query".ljust(w) + "manatee".rjust(12) + "".join(n.rjust(max(12, len(n) + 2)) for n, _ in idx))
    differ = 0
    for q in queries:
        m = manatee(a.fqs, a.corpus, q, a.token)
        ps = [pando(a.pando, d, q) for _, d in idx]
        same = all(p == m for p in ps)
        differ += not same
        print(q.ljust(w) + str(m).rjust(12) + "".join(str(p).rjust(max(12, len(n) + 2)) for (n, _), p in zip(idx, ps)) + ("" if same else "  <<"))
    print(f"\n{len(queries) - differ} of {len(queries)} the same")


if __name__ == "__main__":
    main()
