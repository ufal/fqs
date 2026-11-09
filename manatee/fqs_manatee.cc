// libfqs_manatee: a C interface over manatee-open for FQS (native Manatee, no Python, no KonText).
// FQS loads it at runtime (FQS_MANATEE_LIB), as it loads libflexicorp_pando; see dev/MANATEE-NATIVE.md.
//
// Every function catches C++ exceptions and reports them as an error string: nothing crosses into
// Rust. Strings returned are malloc'ed JSON, freed with fqsm_free. One corpus handle is not meant
// for parallel calls: FQS holds a lock per corpus. Parallel corpora: fqs_manatee_aligned.cc.
//
// Licence: links manatee-open (GPL-2); this library is GPL-2 as well, and optional for FQS.

#include "fqs_manatee.hh"

#include <algorithm>
#include <stdexcept>

using fqsm::dup;
using fqsm::eval;
using fqsm::json_str;
using fqsm::set_err;
using fqsm::use;
using fqsm::walk;

namespace {

// the tokens [from, to) of attribute `pa`, space-separated
std::string span(PosAttr *pa, Position from, Position to) {
    std::string s;
    for (Position p = from; p < to; ++p) {
        if (!s.empty()) s += ' ';
        s += pa->pos2str(p);
    }
    return s;
}

}  // namespace

namespace {

// An option of an attribute in the registry, "" when not set (Manatee throws for unknown paths).
std::string attr_opt(Corpus *c, const std::string &path) {
    try {
        return c->get_conf(path);
    } catch (...) {
        return "";
    }
}

// Whether `value` is one of the parts of the multivalue `text` (MULTISEP `sep`; an empty separator
// splits into bytes, as Manatee does: then any substring counts).
bool has_part(const std::string &text, const std::string &value, const std::string &sep) {
    if (text == value) return true;
    if (sep.empty()) return text.find(value) != std::string::npos;
    for (size_t from = 0; from <= text.size();) {
        size_t at = text.find(sep, from);
        if (text.compare(from, at == std::string::npos ? std::string::npos : at - from, value) == 0) return true;
        if (at == std::string::npos) break;
        from = at + sep.size();
    }
    return false;
}

}  // namespace

std::string fqsm::read_problem(Corpus *c, int &checked) {
    const Position last = c->size() - 1;
    for (const std::string &name : fqsm::split_list(c->get_conf("ATTRLIST"))) {
        PosAttr *pa = c->get_attr(name);
        const int range = pa->id_range();
        if (range <= 0) continue;
        // A MULTIVALUE attribute indexes each part of a value on its own ("Gender=Neut" at a token
        // whose text is "Case=Acc|Gender=Neut|…"): there the indexed value must be one of the parts.
        const std::string mv = attr_opt(c, name + ".MULTIVALUE");
        const bool multivalue = !mv.empty() && (mv[0] == 'y' || mv[0] == 'Y' || mv[0] == '1' || mv[0] == 't' || mv[0] == 'T');
        const std::string sep = multivalue ? attr_opt(c, name + ".MULTISEP") : "";
        const int ids[] = {0, range / 3, range / 2, (2 * range) / 3, range - 1};
        for (int id : ids) {
            FastStream *fs = pa->id2poss(id);
            if (!fs) continue;
            const Position p = fs->peek();
            const bool has = p < fs->final();
            delete fs;
            if (!has) continue;
            const int back = pa->pos2id(p);
            if (back != id && multivalue && back >= 0 && back < range && has_part(pa->id2str(back), pa->id2str(id), sep)) {
                ++checked;
                continue;
            }
            if (back != id) {
                std::string got = (back >= 0 && back < range) ? pa->id2str(back) : "an id out of range";
                return "attribute " + name + ": the index has \"" + pa->id2str(id) + "\" at position " + std::to_string(p) +
                       ", the token text reads \"" + got +
                       "\" (does the registry name the attribute TYPE the corpus was compiled with?)";
            }
            ++checked;
        }
        if (last >= 0) {
            const int id = pa->pos2id(last);
            if (id < 0 || id >= range) return "attribute " + name + ": the last position does not read (id " + std::to_string(id) + ")";
        }
    }
    return "";
}

extern "C" {

const char *fqsm_version() { return "fqs-manatee 0.7.0"; }

// Open a corpus: a registry file path (or a name in MANATEE_REGISTRY). NULL and *err on failure.
// The registry file's folder is where its aligned corpora are looked for first (fqsm::use).
void *fqsm_open(const char *registry, char **err) {
    try {
        std::string reg = registry ? registry : "";
        size_t slash = reg.rfind('/');
        std::string dir = slash == std::string::npos ? "" : (slash == 0 ? "/" : reg.substr(0, slash));
        fqsm::Handle probe{nullptr, dir};
        use(&probe);
        return new fqsm::Handle{new Corpus(reg), dir};
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error opening the corpus");
    }
    return nullptr;
}

void fqsm_close(void *h) {
    try {
        fqsm::Handle *hd = static_cast<fqsm::Handle *>(h);
        if (hd) delete hd->corp;
        delete hd;
    } catch (...) {
    }
}

long long fqsm_size(void *h) {
    try {
        return h ? static_cast<long long>(use(h)->size()) : -1;
    } catch (...) {
        return -1;
    }
}


// Run a query: {"total": N, "hits": [{"pos", "end", "left", "kwic", "right"}]} for hits
// [start, start+size), with `window` tokens of context of attribute `attr` (default the default
// attribute). The total is exact (the whole result is walked).
char *fqsm_query(void *h, const char *cql, long long start, long long size, int window, const char *attr, char **err) {
    try {
        Corpus *c = use(h);
        PosAttr *pa = (attr && *attr) ? c->get_attr(attr) : c->get_default_attr();
        const Position csize = c->size();
        std::string hits;
        long long total = walk(c, cql, start, size, [&](Position b, Position e, RangeStream *) {
            const Position lb = b > window ? b - window : 0;
            const Position re = e + window < csize ? e + window : csize;
            if (!hits.empty()) hits += ',';
            hits += "{\"pos\":" + std::to_string(b) + ",\"end\":" + std::to_string(e) + ",\"left\":";
            json_str(hits, span(pa, lb, b).c_str());
            hits += ",\"kwic\":";
            json_str(hits, span(pa, b, e).c_str());
            hits += ",\"right\":";
            json_str(hits, span(pa, e, re).c_str());
            hits += '}';
        });
        return dup("{\"total\":" + std::to_string(total) + ",\"hits\":[" + hits + "]}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error in the query");
    }
    return nullptr;
}

namespace {

// fqsm_kwic_page and fqsm_kwic_aligned: `aligned` NULL for no aligned corpora.
char *kwic_impl(void *h, const char *cql, long long start, long long size, int window, const char *attrs,
                const char *doc_attr, int count_all, const char *aligned, char **err) {
    fqsm::AlignedPage *al = nullptr;
    try {
        Corpus *c = use(h);
        std::vector<std::string> names = fqsm::split_list(attrs ? attrs : "");
        std::vector<PosAttr *> pas;
        for (const std::string &n : names) pas.push_back(c->get_attr(n));
        if (pas.empty()) {
            names.push_back(c->get_conf("DEFAULTATTR"));
            pas.push_back(c->get_default_attr());
        }
        Structure *doc_st = nullptr;
        PosAttr *doc_pa = nullptr;
        if (doc_attr && *doc_attr) {
            std::string d = doc_attr;
            size_t dot = d.find('.');
            if (dot == std::string::npos) throw std::runtime_error("doc_attr must be structure.attribute: " + d);
            doc_st = c->get_struct(d.substr(0, dot));
            doc_pa = doc_st->get_attr(d.substr(dot + 1));
        }
        if (aligned) al = fqsm::aligned_page_new(c, aligned, names);
        const Position csize = c->size();
        std::string hits;
        long long total = walk(c, cql, start, size, [&](Position b, Position e, RangeStream *rs) {
            const Position lb = b > window ? b - window : 0;
            const Position re = e + window < csize ? e + window : csize;
            if (!hits.empty()) hits += ',';
            hits += "{\"pos\":" + std::to_string(b) + ",\"end\":" + std::to_string(e);
            if (doc_pa) {
                const NumOfPos n = doc_st->rng->num_at_pos(b);
                hits += ",\"doc\":";
                if (n >= 0) json_str(hits, doc_pa->pos2str(n)); else hits += "null";
            }
            hits += ",\"left\":";
            fqsm::tokens(hits, pas, lb, b);
            hits += ",\"kwic\":";
            fqsm::tokens(hits, pas, b, e);
            hits += ",\"right\":";
            fqsm::tokens(hits, pas, e, re);
            if (al) {
                hits += ",\"aligned\":";
                fqsm::aligned_page_hit(al, hits, b, rs);
            }
            hits += '}';
        }, count_all != 0);
        std::string an;
        for (size_t i = 0; i < names.size(); ++i) {
            if (i) an += ',';
            json_str(an, names[i].c_str());
        }
        std::string out = "{\"total\":" + std::to_string(total) + ",\"counted\":" + (count_all ? "true" : "false") +
                          ",\"attrs\":[" + an + "]";
        if (al) out += ",\"aligned_corpora\":" + fqsm::aligned_page_names(al);
        out += ",\"hits\":[" + hits + "]}";
        fqsm::aligned_page_free(al);
        return dup(out);
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error in the query");
    }
    fqsm::aligned_page_free(al);
    return nullptr;
}

}  // namespace

// Run a query with tokens: {"total": N, "attrs": [...], "hits": [{"pos", "end", "doc", "left":
// [[v1, v2, …], …], "kwic": [...], "right": [...]}]}: per token the values of `attrs` (comma-
// separated positional attributes; empty: the default attribute), `window` tokens of context on each
// side, and `doc` the value of the structure attribute `doc_attr` ("doc.id"; empty: none) at the hit.
// For FCS (layers, the document) and pando-shaped answers.
char *fqsm_kwic(void *h, const char *cql, long long start, long long size, int window, const char *attrs,
                const char *doc_attr, char **err) {
    return kwic_impl(h, cql, start, size, window, attrs, doc_attr, 1, nullptr, err);
}

// fqsm_kwic, but with `count_all` 0 the result is walked only up to the end of the page: for a
// query whose total FQS already knows (another page, KonText's second request). "total" is then
// the hits seen, "counted": false.
char *fqsm_kwic_page(void *h, const char *cql, long long start, long long size, int window, const char *attrs,
                     const char *doc_attr, int count_all, char **err) {
    return kwic_impl(h, cql, start, size, window, attrs, doc_attr, count_all, nullptr, err);
}

// fqsm_kwic_page with, per hit, the aligned segments of the parallel corpora `aligned` (comma-
// separated registry names from the corpus's ALIGNED; "*": all of them) (fqs-manatee 0.5):
// "aligned_corpora": [names], and per hit "aligned": [{"corpus", "pos", "end", "tokens": [[v1, …],
// …], "match"?: {"pos", "end"}}], one entry per aligned corpus in that order. The segment is what
// Manatee's add_aligned gives (KonText's aligned column): the aligned structure (ALIGNSTRUCT) the hit
// starts in, mapped by number or, with ALIGNDEF, by the level file mkalign wrote; "pos"/"end" null
// and "tokens" empty when the hit has no counterpart there. "match": the part of the segment that a
// `within CORPUS: query` condition matched (Manatee's labels), when the query has one for it. The
// tokens carry the values of `attrs` (empty: the default attribute; "" for an attribute the aligned
// corpus lacks). Without ALIGNED in the registry: an error.
char *fqsm_kwic_aligned(void *h, const char *cql, long long start, long long size, int window, const char *attrs,
                        const char *doc_attr, int count_all, const char *aligned, char **err) {
    return kwic_impl(h, cql, start, size, window, attrs, doc_attr, count_all, aligned ? aligned : "*", err);
}

// Frequencies of a query's hits by a Manatee criterion ("word 0", "lemma/i 0", "doc.id 0", …):
// {"rows": [{"value", "count"}]}, the most frequent first, at most `limit` (0: all).
char *fqsm_freq(void *h, const char *cql, const char *crit, long long limit, char **err) {
    try {
        Corpus *c = use(h);
        RangeStream *rs = eval(c, cql);
        std::vector<std::string> words;
        std::vector<NumOfPos> freqs, norms;
        c->freq_dist(rs, crit, 0, words, freqs, norms);   // takes the stream
        std::vector<size_t> order(words.size());
        for (size_t i = 0; i < order.size(); ++i) order[i] = i;
        std::sort(order.begin(), order.end(), [&](size_t a, size_t b) { return freqs[a] > freqs[b] || (freqs[a] == freqs[b] && words[a] < words[b]); });
        std::string rows;
        size_t n = 0;
        for (size_t i : order) {
            if (limit > 0 && n >= static_cast<size_t>(limit)) break;
            if (!rows.empty()) rows += ',';
            rows += "{\"value\":";
            json_str(rows, words[i].c_str());
            rows += ",\"count\":" + std::to_string(freqs[i]) + '}';
            ++n;
        }
        return dup("{\"rows\":[" + rows + "],\"groups\":" + std::to_string(words.size()) + "}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error in the frequencies");
    }
    return nullptr;
}

// Is the corpus readable as its registry describes it? For every positional attribute, a few words of
// the lexicon: where the index (id2poss) says a word occurs, the token text (pos2id) must read back the
// same word; and the last position must read. A registry that names the wrong layout (the files
// written as FD_FGD, the registry without TYPE "FD_FGD") fails here instead of answering wrong text
// or crashing on context. {"ok": true, "checked": N} or {"ok": false, "problem": "..."}.
// A parallel corpus (ALIGNED in its registry): its aligned corpora are checked the same way, and
// listed in "aligned" ([{"corpus", "ok", "segments", "warning"?, "error"?}]; fqs-manatee 0.5): one
// that does not open is reported there (queries on it fail with Manatee's message), one that reads
// wrongly fails the check.
char *fqsm_check(void *h, char **err) {
    try {
        Corpus *c = use(h);
        int checked = 0;
        std::string problem = fqsm::read_problem(c, checked);
        if (!problem.empty()) return dup("{\"ok\":false,\"problem\":" + json_str(problem) + "}");
        std::string aligned;
        if (!c->get_conf("ALIGNED").empty()) {
            aligned = fqsm::aligned_check(c, checked, problem);
            if (!problem.empty())
                return dup("{\"ok\":false,\"problem\":" + json_str(problem) + ",\"aligned\":" + aligned + "}");
        }
        return dup("{\"ok\":true,\"checked\":" + std::to_string(checked) + (aligned.empty() ? "" : ",\"aligned\":" + aligned) + "}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error checking the corpus");
    }
    return nullptr;
}

void fqsm_free(char *s) { std::free(s); }

}  // extern "C"
