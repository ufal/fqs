// libfqs_manatee: what KonText reads of a corpus beside concordances (fqs-manatee 0.9): its sizes
// and the number of values / regions per attribute / structure (corpus details), and the context
// of one token with the structure attributes at it (wider context, full reference).
//
// Licence: links manatee-open (GPL-2); this library is GPL-2 as well, and optional for FQS.

#include "fqs_manatee.hh"

#include <algorithm>

using fqsm::dup;
using fqsm::json_str;
using fqsm::set_err;
using fqsm::use;

namespace {

std::string conf_or_empty(Corpus *c, const std::string &path) {
    try {
        return c->get_conf(path);
    } catch (...) {
        return "";
    }
}

// tokens [from, to) of `pa`, space-separated
std::string span(PosAttr *pa, Position from, Position to) {
    std::string s;
    for (Position p = from; p < to; ++p) {
        if (p > from) s += ' ';
        s += pa->pos2str(p);
    }
    return s;
}

}  // namespace

extern "C" {

// {"size", "search_size", "attributes": [{"name", "vocab"}], "structures": [{"name", "regions",
// "attrs": [{"name", "vocab"}]}]}: the corpus as KonText's corpus details show it. Dynamic
// attributes have no lexicon of their own to count cheaply; they are listed with vocab 0.
char *fqsm_corpus_info(void *h, char **err) {
    try {
        Corpus *c = use(h);
        std::string attrs, structs;
        for (const std::string &n : fqsm::split_list(conf_or_empty(c, "ATTRLIST"))) {
            int vocab = 0;
            try {
                vocab = c->get_attr(n)->id_range();
            } catch (...) {
            }
            if (!attrs.empty()) attrs += ',';
            attrs += "{\"name\":" + json_str(n) + ",\"vocab\":" + std::to_string(vocab) + "}";
        }
        for (const std::string &n : fqsm::split_list(conf_or_empty(c, "STRUCTLIST"))) {
            long long regions = 0;
            std::string sattrs;
            try {
                Structure *st = c->get_struct(n);
                regions = st->size();
                for (const std::string &a : fqsm::split_list(conf_or_empty(c, n + ".ATTRLIST"))) {
                    int vocab = 0;
                    try {
                        vocab = st->get_attr(a)->id_range();
                    } catch (...) {
                    }
                    if (!sattrs.empty()) sattrs += ',';
                    sattrs += "{\"name\":" + json_str(a) + ",\"vocab\":" + std::to_string(vocab) + "}";
                }
            } catch (...) {
            }
            if (!structs.empty()) structs += ',';
            structs += "{\"name\":" + json_str(n) + ",\"regions\":" + std::to_string(regions) + ",\"attrs\":[" + sattrs + "]}";
        }
        return dup("{\"size\":" + std::to_string(static_cast<long long>(c->size())) +
                   ",\"search_size\":" + std::to_string(static_cast<long long>(c->search_size())) +
                   ",\"attributes\":[" + attrs + "],\"structures\":[" + structs + "]}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error");
    }
    return nullptr;
}

// The context of the token at `pos`: {"pos", "left", "match", "right", "doc_id"?, "regions"?}: `left`
// / `right` tokens of `attr` (empty: the default attribute) before / after it, space-separated;
// with `regions` the structures that contain `pos` and their attributes there ({"text": {"id": …}}).
char *fqsm_context(void *h, long long pos, int left, int right, int regions, const char *attr, const char *doc_attr, char **err) {
    try {
        Corpus *c = use(h);
        const Position size = c->size();
        if (pos < 0 || pos >= size) throw std::runtime_error("position " + std::to_string(pos) + " is outside the corpus");
        PosAttr *pa = (attr && *attr) ? c->get_attr(attr) : c->get_default_attr();
        const Position lb = std::max<Position>(0, pos - std::max(0, left));
        const Position re = std::min<Position>(size, pos + 1 + std::max(0, right));
        std::string out = "{\"pos\":" + std::to_string(pos) + ",\"left\":" + json_str(span(pa, lb, pos)) +
                          ",\"match\":" + json_str(span(pa, pos, pos + 1)) + ",\"right\":" + json_str(span(pa, pos + 1, re));
        if (doc_attr && *doc_attr) {
            std::string d = doc_attr;
            size_t dot = d.find('.');
            if (dot != std::string::npos) {
                Structure *st = c->get_struct(d.substr(0, dot));
                NumOfPos n = st->rng->num_at_pos(pos);
                if (n >= 0) out += ",\"doc_id\":" + json_str(std::string(st->get_attr(d.substr(dot + 1))->pos2str(n)));
            }
        }
        if (regions) {
            std::string rs;
            for (const std::string &n : fqsm::split_list(conf_or_empty(c, "STRUCTLIST"))) {
                try {
                    Structure *st = c->get_struct(n);
                    NumOfPos r = st->rng->num_at_pos(pos);
                    if (r < 0) continue;
                    std::string as;
                    for (const std::string &a : fqsm::split_list(conf_or_empty(c, n + ".ATTRLIST"))) {
                        try {
                            if (!as.empty()) as += ',';
                            as += json_str(a) + ":" + json_str(std::string(st->get_attr(a)->pos2str(r)));
                        } catch (...) {
                            if (!as.empty() && as.back() == ',') as.pop_back();
                        }
                    }
                    if (!rs.empty()) rs += ',';
                    rs += json_str(n) + ":{" + as + "}";
                } catch (...) {
                }
            }
            out += ",\"regions\":{" + rs + "}";
        }
        return dup(out + "}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error in the context");
    }
    return nullptr;
}

// The regions of a structure with the values of some of its attributes (KonText's text types:
// the value lists and the size of each value read the structure's ranges and attribute lexicons):
// {"size": N, "begs": [...], "ends": [...], "attrs": {"id": ["v0", "v1", …], …}}, one value per
// region. `attrs` comma-separated; more than `max_regions` regions is an error; with `ranges` 0 the
// positions are left out ("begs" and "ends" empty).
char *fqsm_structure(void *h, const char *name, const char *attrs, long long max_regions, int ranges, char **err) {
    try {
        Corpus *c = use(h);
        Structure *st = c->get_struct(name ? name : "");
        const NumOfPos n = st->size();
        if (max_regions > 0 && n > max_regions)
            throw std::runtime_error("structure '" + std::string(name) + "' has " + std::to_string(n) + " regions, more than " + std::to_string(max_regions));
        std::string begs, ends;
        for (NumOfPos i = 0; ranges && i < n; ++i) {
            if (i) { begs += ','; ends += ','; }
            begs += std::to_string(st->rng->beg_at(i));
            ends += std::to_string(st->rng->end_at(i));
        }
        std::string out_attrs;
        for (const std::string &a : fqsm::split_list(attrs ? attrs : "")) {
            PosAttr *pa = st->get_attr(a);
            if (!out_attrs.empty()) out_attrs += ',';
            out_attrs += json_str(a) + ":[";
            for (NumOfPos i = 0; i < n; ++i) {
                if (i) out_attrs += ',';
                json_str(out_attrs, pa->pos2str(i));
            }
            out_attrs += ']';
        }
        return dup("{\"size\":" + std::to_string(static_cast<long long>(n)) + ",\"begs\":[" + begs + "],\"ends\":[" + ends + "],\"attrs\":{" + out_attrs + "}}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error reading the structure");
    }
    return nullptr;
}

// The region of structure `name` that contains position `pos` (KonText's structural context):
// {"num": N, "beg": …, "end": …}, or {"num": -1} when no region does.
char *fqsm_region_at(void *h, const char *name, long long pos, char **err) {
    try {
        Corpus *c = use(h);
        Structure *st = c->get_struct(name ? name : "");
        const NumOfPos n = st->rng->num_at_pos(pos);
        if (n < 0) return dup("{\"num\":-1}");
        return dup("{\"num\":" + std::to_string(static_cast<long long>(n)) + ",\"beg\":" + std::to_string(static_cast<long long>(st->rng->beg_at(n))) +
                   ",\"end\":" + std::to_string(static_cast<long long>(st->rng->end_at(n))) + "}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error reading the structure");
    }
    return nullptr;
}

}  // extern "C"
