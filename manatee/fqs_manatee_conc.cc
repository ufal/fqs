// libfqs_manatee: stored concordances (fqs-manatee 0.9), the building block of Manatee sessions in
// FQS (dev/MANATEE-SESSIONS.md). KonText keeps a concordance (Manatee's `Concordance`) and works on
// it: sort, filters, samples, shuffles, a page of lines, frequencies, collocations. This file gives
// FQS's workers the same calls, each doing what KonText's conclib does with that object, so a chain
// of KonText's operations run here ends in the same concordance as on KonText's own Manatee.
//
// A concordance handle belongs to the corpus handle it was made with (the worker keeps both open
// and frees a concordance before its corpus). Every function catches C++ exceptions and reports
// them as an error string. Results are malloc'ed JSON, freed with fqsm_free.
//
// Licence: links manatee-open (GPL-2); this library is GPL-2 as well, and optional for FQS.

#include "fqs_manatee.hh"
#include "concget.hh"
#include "concstat.hh"

#include <algorithm>
#include <cstdint>
#include <random>
#include <stdexcept>

using fqsm::dup;
using fqsm::eval;
using fqsm::json_str;
using fqsm::set_err;
using fqsm::use;

namespace {

Concordance *as_conc(void *conc) {
    if (!conc) throw std::runtime_error("no concordance");
    return static_cast<Concordance *>(conc);
}

// "-5<0 5>0 1 [lemma=\"x\"]" → lctx rctx rank query (KonText's p / n / P / N operations)
void pn_filter(Concordance *conc, const char *options, bool positive, bool exclude_kwic) {
    std::string o(options ? options : "");
    std::vector<std::string> parts;
    size_t i = 0;
    for (int k = 0; k < 3; ++k) {
        while (i < o.size() && (o[i] == ' ' || o[i] == '\t')) ++i;
        size_t j = i;
        while (j < o.size() && o[j] != ' ' && o[j] != '\t') ++j;
        if (j == i) throw std::runtime_error("Unknown filter \"" + o + "\"");
        parts.push_back(o.substr(i, j - i));
        i = j;
    }
    while (i < o.size() && (o[i] == ' ' || o[i] == '\t')) ++i;
    const std::string query = o.substr(i);
    if (query.empty()) throw std::runtime_error("Unknown filter \"" + o + "\"");
    const int collnum = conc->numofcolls() + 1;
    conc->set_collocation(collnum, query + ";", parts[0].c_str(), parts[1].c_str(), std::atoi(parts[2].c_str()), exclude_kwic);
    conc->delete_pnfilter(collnum, positive);
}

}  // namespace

extern "C" {

// A concordance for the query `cql` (Manatee CQL), computed (sync): what KonText's PyConc does for
// its `q` / `a` operation. `default_attr` (NULL or ""): the default attribute for this query (`a`).
// NULL and *err on failure. Free with fqsm_conc_free before the corpus is closed.
void *fqsm_conc_new(void *h, const char *cql, const char *default_attr, char **err) {
    try {
        Corpus *c = use(h);
        fqsm::DefaultAttr da(c, default_attr);
        Concordance *conc = new Concordance(c, eval(c, cql), 0, 0, 0);
        conc->sync();
        return conc;
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error in the query");
    }
    return nullptr;
}

// A new concordance: a copy of `conc` with KonText's operation `op` applied (the first letter says
// which, the rest are its options, as in KonText's operation chain): s sort, p / n / P / N
// filters, r sample (reduce_lines: deterministic in Manatee), f shuffle (here with `seed`, so that a
// chain gives the same order again; KonText's own route shuffles once and caches the result),
// D (first hit per document, delete_subparts), F (delete_struct_repeats).
// `default_attr`: as for fqsm_conc_new (filter queries read bare strings with it). `conc` stays valid.
void *fqsm_conc_apply(void *h, void *conc, const char *op, unsigned seed, const char *default_attr, char **err) {
    Concordance *n = nullptr;
    try {
        Corpus *c = use(h);
        fqsm::DefaultAttr da(c, default_attr);
        const std::string o(op ? op : "");
        if (o.empty()) throw std::runtime_error("empty operation");
        n = as_conc(conc)->copy();
        n->sync();
        const char *arg = o.c_str() + 1;
        switch (o[0]) {
            case 's':
                if (arg[0] == '*') throw std::runtime_error("GDEX related operations are not supported");
                n->sort(arg);
                break;
            case 'p': pn_filter(n, arg, true, false); break;
            case 'n': pn_filter(n, arg, false, false); break;
            case 'P': pn_filter(n, arg, true, true); break;
            case 'N': pn_filter(n, arg, false, true); break;
            case 'r': n->reduce_lines(arg); break;
            case 'f': {
                std::vector<ConcIndex> order(n->size());
                for (size_t i = 0; i < order.size(); ++i) order[i] = static_cast<ConcIndex>(i);
                std::mt19937_64 rng(seed ? seed : 1);
                std::shuffle(order.begin(), order.end(), rng);
                n->set_sorted_view(order);
                break;
            }
            case 'D': n->delete_subparts(); break;
            case 'F': n->delete_struct_repeats(arg); break;
            default:
                throw std::runtime_error("The operation '" + o.substr(0, 1) + "' is not supported for this corpus");
        }
        n->sync();
        return n;
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error in the operation");
    }
    delete n;
    return nullptr;
}

void fqsm_conc_free(void *conc) {
    try {
        delete static_cast<Concordance *>(conc);
    } catch (...) {
    }
}

// {"size": lines, "fullsize": the population it was drawn from, "finished": true}
char *fqsm_conc_size(void *conc, char **err) {
    try {
        Concordance *c = as_conc(conc);
        return dup("{\"size\":" + std::to_string(static_cast<long long>(c->size())) +
                   ",\"fullsize\":" + std::to_string(static_cast<long long>(c->fullsize())) +
                   ",\"finished\":" + (c->finished() ? "true" : "false") + "}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error");
    }
    return nullptr;
}

// Manatee's ARF of the concordance (KonText: max(compute_ARF(), 0))
char *fqsm_conc_arf(void *conc, char **err) {
    try {
        double a = as_conc(conc)->compute_ARF();
        if (a < 0) a = 0;
        char buf[64];
        std::snprintf(buf, sizeof buf, "{\"arf\":%.17g}", a);
        return dup(buf);
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error");
    }
    return nullptr;
}

// Lines [start, start+size) in the concordance's view order, as fqsm_kwic_ex gives its hits: per
// hit {"pos", "end", "doc"?, "view": KWICLines' output}. `view`: the KWIC spec (left, right, attrs,
// ctxattrs, structs, refs separated by \x1f); `doc_attr` "structure.attribute" (or empty).
// {"size": lines in the concordance, "hits": [...]}
char *fqsm_conc_page(void *h, void *conc, long long start, long long size, const char *view, const char *doc_attr,
                     char **err) {
    try {
        Corpus *c = use(h);
        Concordance *cc = as_conc(conc);
        Structure *doc_st = nullptr;
        PosAttr *doc_pa = nullptr;
        if (doc_attr && *doc_attr) {
            std::string d = doc_attr;
            size_t dot = d.find('.');
            if (dot == std::string::npos) throw std::runtime_error("doc_attr must be structure.attribute: " + d);
            doc_st = c->get_struct(d.substr(0, dot));
            doc_pa = doc_st->get_attr(d.substr(dot + 1));
        }
        const long long total = cc->size();
        long long from = std::max(0LL, start), to = std::min(total, start + std::max(0LL, size));
        std::string hits;
        if (from < to) fqsm::kwic_conc_lines(c, cc, from, to, view, doc_st, doc_pa, hits);
        return dup("{\"size\":" + std::to_string(total) + ",\"hits\":[" + hits + "]}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error reading the concordance");
    }
    return nullptr;
}

// A frequency distribution of the concordance by a Manatee criterion (KonText's xfreq_dist input:
// Corpus::freq_dist over the concordance's stream): {"rows": [{"value", "count", "norm"}]}, in
// Manatee's order; `limit`: Manatee's (a row must have more than `limit`; KonText passes flimit-1…).
char *fqsm_conc_freq(void *h, void *conc, const char *crit, long long limit, char **err) {
    try {
        Corpus *c = use(h);
        Concordance *cc = as_conc(conc);
        std::vector<std::string> words;
        std::vector<NumOfPos> freqs, norms;
        c->freq_dist(cc->RS(), crit, limit, words, freqs, norms);   // takes the stream
        std::string rows;
        for (size_t i = 0; i < words.size(); ++i) {
            if (i) rows += ',';
            rows += "{\"value\":";
            json_str(rows, words[i].c_str());
            rows += ",\"count\":" + std::to_string(freqs[i]) + ",\"norm\":" + std::to_string(i < norms.size() ? norms[i] : 0) + '}';
        }
        return dup("{\"rows\":[" + rows + "]}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error in the frequencies");
    }
    return nullptr;
}

// The size in positions of every value of a structure attribute ("text.id"): {"values": {value:
// positions}} (KonText's get_attr_values_sizes: the norms of a frequency by text type).
char *fqsm_attr_value_sizes(void *h, const char *full_attr, char **err) {
    try {
        Corpus *c = use(h);
        std::string d = full_attr ? full_attr : "";
        size_t sp = d.find_first_of(" \t");
        if (sp != std::string::npos) d.erase(sp);
        size_t dot = d.find('.');
        if (dot == std::string::npos) throw std::runtime_error("not a structure attribute: " + d);
        Structure *st = c->get_struct(d.substr(0, dot));
        PosAttr *pa = st->get_attr(d.substr(dot + 1));
        std::string out;
        const NumOfPos n = st->size();
        // a map from id to total: ids are dense in [0, id_range)
        const int range = pa->id_range();
        std::vector<long long> by_id(range > 0 ? range : 0, 0);
        std::vector<char> seen(range > 0 ? range : 0, 0);
        for (NumOfPos i = 0; i < n; ++i) {
            const int id = pa->pos2id(i);
            if (id < 0 || id >= range) continue;
            by_id[id] += st->rng->end_at(i) - st->rng->beg_at(i);
            seen[id] = 1;
        }
        for (int id = 0; id < range; ++id) {
            if (!seen[id]) continue;
            if (!out.empty()) out += ',';
            json_str(out, pa->id2str(id));
            out += ':' + std::to_string(by_id[id]);
        }
        return dup("{\"values\":{" + out + "}}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error");
    }
    return nullptr;
}

// Collocations of the concordance (KonText's PyConc.collocs → Manatee's CollocItems): {"items":
// [{"str", "freq" (get_cnt), "cfreq" (get_freq), "bgr": {code: value}}]}, in Manatee's order (by
// `sortfn`), at most `max_items` (0: all). `bgrfns`: the measure codes asked for (e.g. "mt").
char *fqsm_conc_coll(void *h, void *conc, const char *attr, const char *sortfn, long long minfreq, long long minbgr,
                     int fromw, int tow, int max_items, const char *bgrfns, char **err) {
    try {
        use(h);
        Concordance *cc = as_conc(conc);
        const std::string codes(bgrfns ? bgrfns : "");
        CollocItems colls(cc, attr ? attr : "", (sortfn && *sortfn) ? sortfn[0] : 'm', minfreq, minbgr, fromw, tow, max_items);
        std::string items;
        int i = 0;
        while (!colls.eos()) {
            if (0 < max_items && max_items < i) break;
            if (!items.empty()) items += ',';
            items += "{\"str\":";
            json_str(items, colls.get_item());
            items += ",\"freq\":" + std::to_string(colls.get_cnt()) + ",\"cfreq\":" + std::to_string(colls.get_freq()) + ",\"bgr\":{";
            for (size_t k = 0; k < codes.size(); ++k) {
                if (k) items += ',';
                char buf[64];
                std::snprintf(buf, sizeof buf, "%.17g", colls.get_bgr(codes[k]));
                items += std::string("\"") + codes[k] + "\":" + buf;
            }
            items += "}}";
            colls.next();
            ++i;
        }
        return dup("{\"items\":[" + items + "]}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error in the collocations");
    }
    return nullptr;
}

}  // extern "C"
