"""Reference answers for the aligned test corpora from Manatee's Python module (as KonText reads them):
per query the total and per hit its position and, per aligned corpus, the aligned segment (KonText's
way: the corpora in the query's `within CORPUS:` parts by their labels, the others by add_aligned).

    MANATEE_REGISTRY=OUT/registry python3 reference.py         (prints JSON; see build.sh)
"""
import json
import manatee

QUERIES = [
    ("par_cs", '[lemma="pes"]', ["par_en"]),
    ("par_cs", '[lemma="pes"] within par_en:[word="sleeps"]', ["par_en"]),
    ("par_cs", '[lemma="pes"] within! par_en:[word="sleeps"]', ["par_en"]),
    ("par_en", '[word="cat"]', ["par_cs"]),
    ("parx_cs", '[lemma="kočka"]', ["parx_en"]),
    ("parx_cs", '[word="Venku"]', ["parx_en"]),
    ("parx_cs", '[lemma="kočka"] within parx_en:[word="lies"]', ["parx_en"]),
    ("parx_en", '[word="lies"]', ["parx_cs"]),
    ("parx_en", '[lemma="cat"] within parx_cs:[lemma="spát"]', ["parx_cs"]),
]


def words(corp, beg, end):
    a = corp.get_attr("word")
    return " ".join(a.pos2str(p) for p in range(beg, end))


def run(name, cql, aligned):
    corp = manatee.Corpus(name)
    conc = manatee.Concordance(corp, cql, 0, -1)
    conc.sync()
    hits = []
    for i in range(conc.size()):
        hits.append({"pos": conc.beg_at(i), "end": conc.end_at(i), "aligned": {}})
    in_query = manatee.StrVector()
    conc.get_aligned(in_query)
    in_query = list(in_query)
    for al in aligned:
        if al not in in_query:
            conc.switch_aligned(corp.get_conffile())
            conc.add_aligned(al)
        conc.switch_aligned(al)
        alc = corp.get_aligned(al) if hasattr(corp, "get_aligned") else manatee.Corpus(al)
        for i, h in enumerate(hits):
            b, e = conc.beg_at(i), conc.end_at(i)
            h["aligned"][al] = None if b < 0 else {"pos": b, "end": e, "text": words(alc, b, e), "labelled": al in in_query}
        conc.switch_aligned(corp.get_conffile())
    return {"corpus": name, "cql": cql, "total": conc.size(), "hits": hits}


print(json.dumps([run(*q) for q in QUERIES], ensure_ascii=False, indent=1))
