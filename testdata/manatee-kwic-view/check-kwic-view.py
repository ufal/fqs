#!/usr/bin/env python3
"""FQS's `kwic_view` (KonText's KWIC lines) against Manatee's own KWICLines, as KonText reads them.

The reference is Manatee's Python module in a KonText container (`docker exec`), on a corpus there;
FQS is asked for the same lines on its twin of that corpus (native Manatee: libfqs_manatee's
KWICLines; pando: pando's port, src/api/kwic_view.cpp). Every line field must be equal: pos,
kwiclen, ctxbeg, ctxend, refs, left / kwic / right (strings and classes).

    check-kwic-view.py --container kontext-kontext-1 --reference ud_manatee_kt \
        --fqs http://127.0.0.1:8787 --corpus ud_manatee [--corpus ud_pando --strip-ref-suffix .conllu]
        [--registry /var/lib/manatee/registry]   (MANATEE_REGISTRY in the container)

`--strip-ref-suffix`: a twin built from other files (pando's UD index keeps `.conllu` in text ids).
"""
import argparse, json, subprocess, sys, urllib.request

SPECS = [
    {"left": "-40#", "right": "40#", "attrs": "word", "ctxattrs": "word", "structs": "", "refs": "=text.id"},
    {"left": "-40#", "right": "40#", "attrs": "word,lemma", "ctxattrs": "word,lemma", "structs": "s", "refs": "=text.id"},
    {"left": "-1:s", "right": "1:s", "attrs": "word", "ctxattrs": "word", "structs": "s,text.id", "refs": "#,s,text.id"},
    {"left": "-5", "right": "5", "attrs": "word,upos", "ctxattrs": "word", "structs": "text,s", "refs": "text.id"},
    {"left": "-60#", "right": "60#", "attrs": "word", "ctxattrs": "word", "structs": "s", "refs": ""},
]
QUERIES = ['[lemma="dog"]', '[upos="ADJ"] [lemma="dog"]', '"the" []{0,2} "dog"', '[lemma="dog.*"] within <s/>']
START, SIZE = 3, 6

REFERENCE = r'''
import json, sys, manatee
corp = manatee.Corpus(sys.argv[1]); out = []
for q, spec, start, size in json.loads(sys.argv[2]):
    conc = manatee.Concordance(corp, q, 0, -1); conc.sync()
    kl = manatee.KWICLines(corp, conc.RS(True, start, start + size), spec["left"], spec["right"], spec["attrs"],
                           spec["ctxattrs"], spec["structs"], spec["refs"])
    lines = []
    while kl.nextline():
        lines.append(dict(pos=kl.get_pos(), kwiclen=kl.get_kwiclen(), ctxbeg=kl.get_ctxbeg(), ctxend=kl.get_ctxend(),
                          refs=list(kl.get_ref_list()), left=list(kl.get_left()), kwic=list(kl.get_kwic()),
                          right=list(kl.get_right())))
    out.append(lines)
print(json.dumps(out))
'''


def fqs_lines(base, corpus, q, spec):
    body = dict(corpus=corpus, query=q, language="manatee-cql", start=START, size=SIZE, window=5, kwic_view=spec)
    r = json.load(urllib.request.urlopen(urllib.request.Request(
        base.rstrip("/") + "/query", data=json.dumps(body).encode(), headers={"content-type": "application/json"}), timeout=120))
    res = r["raw"]["done"]["result"] if "done" in r["raw"] else r["raw"]["result"]
    return [h.get("kwic_view") for h in res["hits"]]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--container", default="kontext-kontext-1")
    ap.add_argument("--reference", default="ud_manatee_kt")
    ap.add_argument("--registry", default="/var/lib/manatee/registry", help="MANATEE_REGISTRY in the container")
    ap.add_argument("--fqs", default="http://127.0.0.1:8787")
    ap.add_argument("--corpus", action="append", required=True)
    ap.add_argument("--strip-ref-suffix", default="")
    a = ap.parse_args()
    cases = [(q, s) for q in QUERIES for s in SPECS]
    ref = json.loads(subprocess.run(
        ["docker", "exec", "-e", "MANATEE_REGISTRY=" + a.registry, a.container, "python3", "-c", REFERENCE, a.reference,
         json.dumps([(q, s, START, SIZE) for q, s in cases])], check=True, capture_output=True, text=True).stdout)
    def attr_delim(line):
        # Manatee's attribute delimiter: '/' upstream, U+001F in ÚČNK's manatee-open (KonText's); KonText
        # reads either (the chunk's first character), so they count as the same here
        for k in ("left", "kwic", "right"):
            v = line.get(k) or []
            line[k] = [("/" + x[1:] if v[i + 1:i + 2] == ["attr"] and x[:1] == "\x1f" else x) if i % 2 == 0 else x
                       for i, x in enumerate(v)]
        return line
    ref = [[attr_delim(l) for l in lines] for lines in ref]
    bad = 0
    for corpus in a.corpus:
        for (q, spec), want in zip(cases, ref):
            got = fqs_lines(a.fqs, corpus, q, spec)
            for i, (w, g) in enumerate(zip(want, got)):
                g = attr_delim(dict(g or {}))
                g.pop("linegroup", None)
                if a.strip_ref_suffix:
                    g["refs"] = [r.replace(a.strip_ref_suffix, "") for r in g.get("refs", [])]
                    for k in ("left", "kwic", "right"):
                        g[k] = [x.replace(a.strip_ref_suffix, "") for x in g.get(k, [])]
                if w != g:
                    bad += 1
                    diff = {k: (w.get(k), g.get(k)) for k in w if w.get(k) != g.get(k)}
                    print(f"FAIL {corpus} {q} {spec['left']}/{spec['structs']} line {i}: {json.dumps(diff, ensure_ascii=False)[:300]}")
            if len(want) != len(got):
                bad += 1
                print(f"FAIL {corpus} {q}: {len(want)} lines vs {len(got)}")
    n = len(cases) * len(a.corpus)
    print(f"{n - bad} of {n} cases" if bad else f"all {n} cases equal ({SIZE} lines each)")
    sys.exit(1 if bad else 0)


main()
