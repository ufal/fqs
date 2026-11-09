#!/usr/bin/env python3
"""Stored subcorpora on native Manatee through an FQS, against a reference computed from subc.vert
itself (not from Manatee). The FQS runs without a JWT secret (the request's `user` / `request_role`
are believed), with native Manatee (FQS_MANATEE_LIB, fqs-manatee 0.7+) and the corpus `subctest`:

    MANATEE_SRC=/path/to/manatee-open sh testdata/manatee-subcorpora/build.sh
    fqs corpora upsert-json --db DB --json '[{"id": "subctest", "label": "subcorpus test",
        "project_root": "'$PWD'/testdata/manatee-subcorpora", "preferred_backend": "manatee",
        "settings": {"manatee_registry": "'$PWD'/testdata/manatee-subcorpora/out/registry/subctest"}}]'
    FQS_MANATEE_LIB=$PWD/manatee/libfqs_manatee.dylib fqs serve --db DB --port 18799 &
    python3 testdata/manatee-subcorpora/check-fqs.py [http://127.0.0.1:18799 [CORPUS [ATTR]]]

The same checks on a pando index of subc.vert (`pando-index --format vertical subc.vert DIR`, a
catalogue row with preferred_backend pando and settings.index_dir DIR):

    python3 testdata/manatee-subcorpora/check-fqs.py http://127.0.0.1:18799 subcpando form

With MANATEE_SRC set, the corpus is also rebuilt once (a "reindex") to check that a subcorpus is
resolved again on its next use. The subcorpora it makes are deleted first and at the end.
Checks: sizes of every kind of definition (regions with = / != / several values / literal values
with pattern characters, documents, a token query, an expression); hits under `use` and `within`
(a hit counts only when all its tokens are inside); `/info` sizes; visibility (private, groups,
public) and the publish rule (default: admins); PATCH; DELETE refused while an expression uses it.
Exit status 1 on any difference.
"""
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request

URL = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18799").rstrip("/")
HERE = os.path.dirname(os.path.abspath(__file__))
C = sys.argv[2] if len(sys.argv) > 2 else "subctest"
ATTR = sys.argv[3] if len(sys.argv) > 3 else "word"   # the word attribute (pando's vertical reader: form)
FAILS = []


def call(method, path, body=None, **params):
    url = URL + path + ("?" + urllib.parse.urlencode(params) if params else "")
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method, headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        text = e.read().decode("utf-8", "replace")
        try:
            return e.code, json.loads(text)
        except ValueError:
            return e.code, {"error": text}


def check(what, got, want):
    if got != want:
        FAILS.append(what)
        print(f"FAIL {what}: got {got!r}, want {want!r}")
    else:
        print(f"ok   {what}")


# --- the reference: documents and tokens of subc.vert -------------------------------------------
docs, words = [], []
for line in open(os.path.join(HERE, "subc.vert"), encoding="utf-8"):
    line = line.rstrip("\n")
    m = re.match(r"<doc (.*)>$", line)
    if m:
        docs.append({"attrs": dict(re.findall(r'(\w+)="([^"]*)"', m.group(1))), "beg": len(words)})
    elif line == "</doc>":
        docs[-1]["end"] = len(words)
    elif not line.startswith("<"):
        words.append(line)


def doc_positions(pred):
    return {p for d in docs if pred(d["attrs"]) for p in range(d["beg"], d["end"])}


REF = {
    "fiction": doc_positions(lambda a: a["genre"] == "fiction"),
    "y1990": doc_positions(lambda a: a["year"] == "1990"),
    "fic_or_poetry": doc_positions(lambda a: a["genre"] in ("fiction", "poetry")),
    "notnews": doc_positions(lambda a: a["genre"] != "news"),
    "night": doc_positions(lambda a: a["title"] == "Night 1.2"),
    "budget": doc_positions(lambda a: a["title"] == "Budget (draft)"),
    "docs": doc_positions(lambda a: a["id"] in ("d2", "d4")),
    "thes": {p for p, w in enumerate(words) if w == "the"},
    "alldocs": set(range(len(words))),
}
REF["expr"] = (REF["fiction"] | REF["docs"]) - REF["y1990"]
REF["both"] = REF["y1990"] & REF["notnews"]

DEFS = {
    "fiction": {"kind": "regions", "struct": "doc", "conditions": [{"attr": "genre", "value": "fiction"}]},
    "y1990": {"kind": "regions", "struct": "doc", "conditions": [{"attr": "year", "value": "1990"}]},
    "fic_or_poetry": {"kind": "regions", "struct": "doc", "conditions": [{"attr": "genre", "value": ["fiction", "poetry"]}]},
    "notnews": {"kind": "regions", "struct": "doc", "conditions": [{"attr": "genre", "op": "!=", "value": "news"}]},
    "night": {"kind": "regions", "struct": "doc", "conditions": [{"attr": "title", "value": "Night 1.2"}]},
    "budget": {"kind": "regions", "struct": "doc", "conditions": [{"attr": "title", "value": "Budget (draft)"}]},
    "docs": {"kind": "documents", "ids": ["d2", "d4"], "struct": "doc"},
    "thes": {"kind": "query", "query": f'[{ATTR}="the"]', "language": "manatee-cql"},
    "alldocs": {"kind": "regions", "struct": "doc"},
    "expr": {"kind": "expression", "expression": "(fiction | docs) - y1990"},
    "both": {"kind": "expression", "expression": "y1990 & notnews"},
}


def hits(seq, inside):
    """starts of the hits of a word sequence whose tokens all lie in `inside`"""
    n = len(seq)
    return [p for p in range(len(words) - n + 1) if words[p:p + n] == seq and all(q in inside for q in range(p, p + n))]


def cleanup():
    _, ls = call("GET", f"/corpora/{C}/subcorpora", request_role="admin", user="root")
    for s in ls.get("subcorpora", []):
        if s["definition"]["kind"] == "expression":
            call("DELETE", f"/corpora/{C}/subcorpora/{s['name']}", request_role="admin", user="root", owner=s["owner"])
    _, ls = call("GET", f"/corpora/{C}/subcorpora", request_role="admin", user="root")
    for s in ls.get("subcorpora", []):
        call("DELETE", f"/corpora/{C}/subcorpora/{s['name']}", request_role="admin", user="root", owner=s["owner"])


def run_query(cql, **extra):
    st, a = call("POST", "/query", {"corpus": C, "query": cql, "size": 500, **extra})
    if st != 200:
        return st, None, a
    r = a["raw"]["done"]["result"]
    total = r["total"] if "total" in r else r["page"]["total"]
    return st, (total, [h["match_start"] for h in r["hits"]]), a


cleanup()
alice = {"user": "alice", "request_role": "user"}

# --- creation: every kind, sizes against the reference ------------------------------------------
for name, d in DEFS.items():
    st, row = call("POST", f"/corpora/{C}/subcorpora", {"name": name, "definition": d, **alice})
    check(f"create {name}: status", st, 201)
    check(f"create {name}: tokens", row.get("tokens"), len(REF[name]))
st, row = call("GET", f"/corpora/{C}/subcorpora/docs", **alice)
check("documents: the attribute filled in", (row["definition"].get("struct"), row["definition"].get("attr")), ("doc", "id"))
st, _ = call("POST", f"/corpora/{C}/subcorpora", {"name": "fiction", "definition": DEFS["fiction"], **alice})
check("same name again: 409", st, 409)
st, _ = call("POST", f"/corpora/{C}/subcorpora", {"name": "none", "definition": {"kind": "regions", "struct": "doc", "conditions": [{"attr": "genre", "value": "nothing"}]}, **alice})
check("selects nothing: 400", st, 400)
st, _ = call("POST", f"/corpora/{C}/subcorpora", {"name": "bad-name", "definition": DEFS["fiction"], **alice})
check("a name with '-': 400", st, 400)
st, _ = call("POST", f"/corpora/{C}/subcorpora", {"name": "x", "definition": DEFS["fiction"]})
check("no owner (anonymous): 403", st, 403)
st, _ = call("POST", f"/corpora/{C}/subcorpora", {"name": "self", "definition": {"kind": "expression", "expression": "self | fiction"}, **alice})
check("an expression using itself: 400", st, 400)
st, _ = call("POST", f"/corpora/{C}/subcorpora", {"name": "missing", "definition": {"kind": "expression", "expression": "fiction & nope"}, **alice})
check("an expression naming an unknown subcorpus: 400", st, 400)

# --- queries: use and within ---------------------------------------------------------------------
for name in ["fiction", "expr", "thes", "notnews", "both"]:
    for seq in (["the"], ["the", "dog"], ["the", "the"]):
        cql = " ".join(f'[{ATTR}="{w}"]' for w in seq)
        want = hits(seq, REF[name])
        st, got, a = run_query(cql, subcorpus=name, **alice)
        check(f"use {name}: {cql}", got, (len(want), want))
        st, got_w, a_w = run_query(cql, subcorpus=name, subcorpus_mode="within", **alice)
        check(f"within {name}: same hits", got_w, (len(want), want))
    check(f"use {name}: answer's subcorpus", a.get("subcorpus"), {"name": name, "mode": "use", "tokens": len(REF[name]), "owner": "alice"})
    check(f"within {name}: answer's subcorpus mode", a_w.get("subcorpus", {}).get("mode"), "within")
st, got, a = run_query(f'[{ATTR}="the"]')
check("no subcorpus: whole corpus, no subcorpus field", (got[0], "subcorpus" in a), (len(hits(["the"], REF["alldocs"])), False))
st, _, _ = run_query(f'[{ATTR}="the"]', subcorpus="fiction", subcorpus_mode="into", **alice)
check("bad mode: 400", st, 400)

st, info = call("GET", "/info", corpus=C, subcorpus="fiction", user="alice")
check("/info use: size", (st, info.get("result", {}).get("size")), (200, len(REF["fiction"])))
st, info = call("GET", "/info", corpus=C, subcorpus="fiction", subcorpus_mode="within", user="alice")
check("/info within: subcorpus tokens", info.get("subcorpus", {}).get("tokens"), len(REF["fiction"]))

# --- visibility and publishing (default rule: admins) --------------------------------------------
bob = {"user": "bob", "request_role": "user"}
st, _, _ = run_query(f'[{ATTR}="the"]', subcorpus="fiction", **bob)
check("private: bob cannot use alice's", st, 404)
st, _ = call("PATCH", f"/corpora/{C}/subcorpora/fiction", {"visibility": "public", **alice})
check("publish as a user under 'admins': 403", st, 403)
st, row = call("PATCH", f"/corpora/{C}/subcorpora/fiction", {"visibility": "groups", "groups": ["acad"], "owner": "alice", "user": "root", "request_role": "admin"})
check("an admin publishes to a group", (st, row.get("visibility")), (200, "groups"))
st, _, _ = run_query(f'[{ATTR}="the"]', subcorpus="fiction", **bob)
check("groups: bob is not in it (no token, no groups)", st, 404)
st, row = call("PATCH", f"/corpora/{C}/subcorpora/fiction", {"visibility": "public", "owner": "alice", "user": "root", "request_role": "admin"})
st, got, a = run_query(f'[{ATTR}="the"]', subcorpus="fiction", **bob)
check("public: bob uses it", (st, a.get("subcorpus", {}).get("owner")), (200, "alice"))
st, _ = call("PATCH", f"/corpora/{C}/subcorpora/fiction", {"visibility": "private", **alice})
check("the owner withdraws it (always allowed)", st, 200)
st, _ = call("DELETE", f"/corpora/{C}/subcorpora/fiction", **bob)
check("bob cannot delete alice's", st, 404)
st, ls = call("GET", f"/corpora/{C}/subcorpora", **bob)
check("bob's list is empty", ls.get("subcorpora"), [])
st, ls = call("GET", f"/corpora/{C}/subcorpora", **alice)
check("alice's list", sorted(s["name"] for s in ls["subcorpora"]), sorted(DEFS))
st, _ = call("DELETE", f"/corpora/{C}/subcorpora/docs", **alice)
check("delete while 'expr' uses it: 409", st, 409)

# --- a reindex: resolved again on the next use ----------------------------------------------------
if os.environ.get("MANATEE_SRC") and C == "subctest":
    _, before = call("GET", f"/corpora/{C}/subcorpora/expr", **alice)
    subprocess.run(["sh", os.path.join(HERE, "build.sh")], check=True, capture_output=True)
    st, got, a = run_query(f'[{ATTR}="the"]', subcorpus="expr", **alice)
    want = hits(["the"], REF["expr"])
    check("after a reindex: same hits", got, (len(want), want))
    _, after = call("GET", f"/corpora/{C}/subcorpora/expr", **alice)
    check("after a reindex: resolved again", before["resolved"]["against"] != after["resolved"]["against"], True)
else:
    print("skip reindex check (set MANATEE_SRC)")

cleanup()
print(f"\n{len(FAILS)} failed" if FAILS else "\nall passed")
sys.exit(1 if FAILS else 0)
