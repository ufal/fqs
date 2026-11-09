// libfqs_manatee: what its source files share (fqs_manatee.cc: corpora, queries, the check;
// fqs_manatee_aligned.cc: parallel corpora). See fqs_manatee.cc and dev/MANATEE-NATIVE.md.
//
// Licence: links manatee-open (GPL-2); this library is GPL-2 as well, and optional for FQS.

#ifndef FQS_MANATEE_HH
#define FQS_MANATEE_HH

#include "corpus.hh"
#include "cqpeval.hh"

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

namespace fqsm {

// What a handle of this library is: the corpus, and the folder of the registry file it was opened
// from. Aligned corpora (the registry's ALIGNED) are opened by Manatee by name, through
// MANATEE_REGISTRY: that folder goes first there whenever the corpus is used (see `use`).
struct Handle {
    Corpus *corp;
    std::string regdir;
};

inline char *dup(const std::string &s) {
    char *p = static_cast<char *>(std::malloc(s.size() + 1));
    if (p) std::memcpy(p, s.c_str(), s.size() + 1);
    return p;
}

inline void set_err(char **err, const std::string &msg) {
    if (err) *err = dup(msg);
}

inline void json_str(std::string &out, const char *s) {
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

inline std::string json_str(const std::string &s) {
    std::string out;
    json_str(out, s.c_str());
    return out;
}

// a comma-separated list, empty items left out
inline std::vector<std::string> split_list(const std::string &list) {
    std::vector<std::string> out;
    for (size_t from = 0; from <= list.size();) {
        size_t comma = list.find(',', from);
        std::string n = list.substr(from, comma == std::string::npos ? std::string::npos : comma - from);
        from = comma == std::string::npos ? list.size() + 1 : comma + 1;
        while (!n.empty() && (n.back() == ' ' || n.back() == '\t')) n.pop_back();
        while (!n.empty() && (n[0] == ' ' || n[0] == '\t')) n.erase(0, 1);
        if (!n.empty()) out.push_back(n);
    }
    return out;
}

// The corpus of a handle, with its registry folder first in MANATEE_REGISTRY (then whatever the
// process had), so that Manatee finds its aligned corpora next to it (`within CORPUS: …`,
// Corpus::get_aligned). The workers are single-threaded: setting the variable per call is safe.
inline Corpus *use(void *h) {
    static bool saved = false;
    static std::string original;
    if (!saved) {
        const char *o = std::getenv("MANATEE_REGISTRY");
        original = o ? o : "";
        saved = true;
    }
    Handle *hd = static_cast<Handle *>(h);
    if (!hd->regdir.empty()) {
        std::string v = hd->regdir + (original.empty() ? "" : ":" + original);
        setenv("MANATEE_REGISTRY", v.c_str(), 1);
    }
    return hd->corp;
}

// a query as the Python wrapper evaluates it: with a closing ';'
inline RangeStream *eval(Corpus *c, const char *cql) {
    std::string q(cql ? cql : "");
    while (!q.empty() && (q.back() == ' ' || q.back() == ';' || q.back() == '\n')) q.pop_back();
    q += ';';
    return c->filter_query(eval_cqpquery(q.c_str(), c));
}

// Walk a query's result: `fn(beg, end, rs)` for hits [start, start+size) (`rs` at the hit: its
// labels, e.g. the matches in aligned corpora); the total (exact: every hit is walked), or with
// `count_all` false the hits up to the end of the page only (then the total is exact only when it
// is below start+size: the result ended inside the page).
// `begs` (optional): every hit's start, in corpus order (for the ARF).
template <class Fn>
long long walk(Corpus *c, const char *cql, long long start, long long size, Fn fn, bool count_all = true,
               std::vector<Position> *begs = nullptr) {
    RangeStream *rs = eval(c, cql);
    long long total = 0;
    try {
        while (!rs->end()) {
            if (begs) begs->push_back(rs->peek_beg());
            if (total >= start && total < start + size) fn(rs->peek_beg(), rs->peek_end(), rs);
            ++total;
            if (!count_all && total >= start + size) break;
            rs->next();
        }
    } catch (...) {
        delete rs;
        throw;
    }
    delete rs;
    return total;
}

// The per-token attribute values of a hit's context: `pas` (NULL: an attribute this corpus lacks,
// read as ""), tokens [from, to) as [[v1, v2, …], …].
inline void tokens(std::string &out, const std::vector<PosAttr *> &pas, Position from, Position to) {
    out += '[';
    for (Position p = from; p < to; ++p) {
        if (p > from) out += ',';
        out += '[';
        for (size_t i = 0; i < pas.size(); ++i) {
            if (i) out += ',';
            json_str(out, pas[i] ? pas[i]->pos2str(p) : "");
        }
        out += ']';
    }
    out += ']';
}

// The problem of a corpus whose token text does not agree with its index ("" when it reads right):
// fqsm_check's test, for a corpus or an aligned one. `checked` counts the words tested.
std::string read_problem(Corpus *c, int &checked);

}  // namespace fqsm

// The aligned part of fqsm_kwic_aligned (fqs_manatee_aligned.cc): for one page of hits, per hit
// the segment(s) of each aligned corpus.
namespace fqsm {
class AlignedPage;
AlignedPage *aligned_page_new(Corpus *c, const std::string &which, const std::vector<std::string> &attr_names);
void aligned_page_hit(AlignedPage *a, std::string &out, Position beg, RangeStream *rs);
std::string aligned_page_names(AlignedPage *a);
void aligned_page_free(AlignedPage *a);
// The aligned corpora (the registry's ALIGNED) checked as fqsm_check checks the corpus: a JSON
// array [{"corpus", "ok", "segments"?, "warning"?, "error"?}]; `problem` set when one does not read
// as its registry describes (a corpus that cannot be opened is no problem: it is reported).
std::string aligned_check(Corpus *c, int &checked, std::string &problem);
}  // namespace fqsm

// KonText's KWIC lines (fqs_manatee_view.cc): Manatee's KWICLines over one hit at a time. `spec`:
// left, right, attrs, ctxattrs, structs, refs separated by \x1f, as KonText's kwiclib passes them.
namespace fqsm {
class KwicView;
KwicView *kwic_view_new(Corpus *c, const char *spec);
void kwic_view_hit(KwicView *v, std::string &out, Position beg, Position end, RangeStream *rs);
void kwic_view_free(KwicView *v);
// Manatee's ARF over the hits' starts (corpus order) in a corpus of `size` positions
double arf(const std::vector<Position> &begs, Position size);
}  // namespace fqsm

#endif
