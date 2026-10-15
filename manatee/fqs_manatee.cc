// libfqs_manatee: a C interface over manatee-open for FQS (native Manatee, no Python, no KonText).
// FQS loads it at runtime (FQS_MANATEE_LIB), as it loads libflexicorp_pando; see dev/MANATEE-NATIVE.md.
//
// Every function catches C++ exceptions and reports them as an error string: nothing crosses into
// Rust. Strings returned are malloc'ed JSON, freed with fqsm_free. One corpus handle is not meant
// for parallel calls: FQS holds a lock per corpus.
//
// Licence: links manatee-open (GPL-2); this library is GPL-2 as well, and optional for FQS.

#include "corpus.hh"
#include "cqpeval.hh"

#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

namespace {

char *dup(const std::string &s) {
    char *p = static_cast<char *>(std::malloc(s.size() + 1));
    if (p) std::memcpy(p, s.c_str(), s.size() + 1);
    return p;
}

void set_err(char **err, const std::string &msg) {
    if (err) *err = dup(msg);
}

void json_str(std::string &out, const char *s) {
    out += '"';
    for (const unsigned char *c = reinterpret_cast<const unsigned char *>(s ? s : ""); *c; ++c) {
        switch (*c) {
            case '"': out += "\\\""; break;
            case '\\': out += "\\\\"; break;
            case '\n': out += "\\n"; break;
            case '\r': out += "\\r"; break;
            case '\t': out += "\\t"; break;
            default:
                if (*c < 0x20) {
                    char buf[8];
                    std::snprintf(buf, sizeof buf, "\\u%04x", *c);
                    out += buf;
                } else {
                    out += static_cast<char>(*c);
                }
        }
    }
    out += '"';
}

// the tokens [from, to) of attribute `pa`, space-separated
std::string span(PosAttr *pa, Position from, Position to) {
    std::string s;
    for (Position p = from; p < to; ++p) {
        if (!s.empty()) s += ' ';
        s += pa->pos2str(p);
    }
    return s;
}

// a query as the Python wrapper evaluates it: with a closing ';'
RangeStream *eval(Corpus *c, const char *cql) {
    std::string q(cql ? cql : "");
    while (!q.empty() && (q.back() == ' ' || q.back() == ';' || q.back() == '\n')) q.pop_back();
    q += ';';
    return c->filter_query(eval_cqpquery(q.c_str(), c));
}

}  // namespace

extern "C" {

const char *fqsm_version() { return "fqs-manatee 0.1"; }

// Open a corpus: a registry file path (or a name in MANATEE_REGISTRY). NULL and *err on failure.
void *fqsm_open(const char *registry, char **err) {
    try {
        return new Corpus(registry ? registry : "");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error opening the corpus");
    }
    return nullptr;
}

void fqsm_close(void *h) {
    try {
        delete static_cast<Corpus *>(h);
    } catch (...) {
    }
}

long long fqsm_size(void *h) {
    try {
        return h ? static_cast<long long>(static_cast<Corpus *>(h)->size()) : -1;
    } catch (...) {
        return -1;
    }
}

// Run a query: {"total": N, "hits": [{"pos", "end", "left", "kwic", "right"}]} for hits
// [start, start+size), with `window` tokens of context of attribute `attr` (default the default
// attribute). The total is exact (the whole result is walked).
char *fqsm_query(void *h, const char *cql, long long start, long long size, int window, const char *attr, char **err) {
    try {
        Corpus *c = static_cast<Corpus *>(h);
        PosAttr *pa = (attr && *attr) ? c->get_attr(attr) : c->get_default_attr();
        const Position csize = c->size();
        RangeStream *rs = eval(c, cql);
        std::string hits;
        long long total = 0;
        while (!rs->end()) {
            if (total >= start && total < start + size) {
                const Position b = rs->peek_beg(), e = rs->peek_end();
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
            }
            ++total;
            rs->next();
        }
        delete rs;
        return dup("{\"total\":" + std::to_string(total) + ",\"hits\":[" + hits + "]}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error in the query");
    }
    return nullptr;
}

// Frequencies of a query's hits by a Manatee criterion ("word 0", "lemma/i 0", "doc.id 0", …):
// {"rows": [{"value", "count"}]}, the most frequent first, at most `limit` (0: all).
char *fqsm_freq(void *h, const char *cql, const char *crit, long long limit, char **err) {
    try {
        Corpus *c = static_cast<Corpus *>(h);
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

void fqsm_free(char *s) { std::free(s); }

}  // extern "C"
