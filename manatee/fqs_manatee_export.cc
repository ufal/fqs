// libfqs_manatee: a compiled Manatee corpus written out as pando's JSONL v2 (pando/dev/PANDO-JSONL-V2.md),
// for `fqs manatee-export`: a pando corpus made from a Manatee corpus whose vertical is gone.
//
// What is written: a header (the positional attributes in ATTRLIST order, the first one as `form`;
// the structures of STRUCTLIST with their attributes; multivalue fields; zero-width and nested
// structures); one compact token line per position ({"type":"token","v":[…]}, read sequentially);
// each structure range as a single-shot `region` right after its last token (inner ones first),
// zero-width ranges (beg == end) right before the token they stand at. Values are recoded to UTF-8
// from the registry's ENCODING; a multivalue attribute's MULTISEP becomes `|`, as pando reads them.
// Dynamic attributes (DYNAMIC in the registry) are derived from others and left out unless asked for.
//
// Licence: links manatee-open (GPL-2); this library is GPL-2 as well, and optional for FQS.

#include "fqs_manatee.hh"

#include <algorithm>
#include <cerrno>
#include <cctype>
#include <ctime>
#include <iconv.h>
#include <map>
#include <queue>
#include <stdexcept>

using fqsm::json_str;
using fqsm::set_err;

namespace {

// Manatee's ENCODING ("utf-8", "iso8859-2", "cp1250", …) as iconv knows it; "" = UTF-8 already.
std::string iconv_name(std::string enc) {
    std::transform(enc.begin(), enc.end(), enc.begin(), [](unsigned char c) { return std::tolower(c); });
    if (enc.empty() || enc == "utf-8" || enc == "utf8") return "";
    if (enc.rfind("iso8859-", 0) == 0) return "ISO-8859-" + enc.substr(8);
    if (enc.rfind("iso8859_", 0) == 0) return "ISO-8859-" + enc.substr(8);
    if (enc == "latin1") return "ISO-8859-1";
    if (enc == "latin2") return "ISO-8859-2";
    if (enc.rfind("cp", 0) == 0) return "CP" + enc.substr(2);
    return enc;
}

// Recodes Manatee's strings to UTF-8 (a no-op for UTF-8 corpora).
class Recoder {
    iconv_t cd_ = reinterpret_cast<iconv_t>(-1);
    std::string buf_;

public:
    explicit Recoder(const std::string &encoding) {
        std::string from = iconv_name(encoding);
        if (from.empty()) return;
        cd_ = iconv_open("UTF-8", from.c_str());
        if (cd_ == reinterpret_cast<iconv_t>(-1)) throw std::runtime_error("cannot recode from the registry's ENCODING \"" + encoding + "\" to UTF-8");
    }
    ~Recoder() {
        if (active()) iconv_close(cd_);
    }
    Recoder(const Recoder &) = delete;
    Recoder &operator=(const Recoder &) = delete;
    bool active() const { return cd_ != reinterpret_cast<iconv_t>(-1); }

    const char *operator()(const char *s) {
        if (!active() || !s) return s ? s : "";
        size_t inleft = std::strlen(s);
        buf_.assign(inleft * 4 + 4, '\0');
        char *in = const_cast<char *>(s);
        char *out = &buf_[0];
        size_t outleft = buf_.size() - 1;
        iconv(cd_, nullptr, nullptr, nullptr, nullptr);
        while (inleft > 0) {
            if (iconv(cd_, &in, &inleft, &out, &outleft) == static_cast<size_t>(-1)) {
                if (errno == EILSEQ || errno == EINVAL) {   // a byte the encoding does not have: U+FFFD
                    ++in;
                    --inleft;
                    if (outleft >= 3) {
                        *out++ = '\xEF';
                        *out++ = '\xBF';
                        *out++ = '\xBD';
                        outleft -= 3;
                    }
                } else {
                    break;
                }
            }
        }
        *out = '\0';
        return buf_.c_str();
    }
};

bool truthy(const std::string &v) {
    return !v.empty() && (v[0] == 'y' || v[0] == 'Y' || v[0] == '1' || v[0] == 't' || v[0] == 'T');
}

// An option of the registry, "" when it is not there (Manatee throws for an unknown attribute).
std::string opt(Corpus *c, const std::string &path) {
    try {
        return c->get_conf(path);
    } catch (...) {
        return "";
    }
}

// A value with its MULTISEP turned into `|` (pando's multivalue separator).
std::string multi(const char *v, const std::string &sep) {
    std::string s(v);
    if (sep.empty() || sep == "|") return s;
    std::string out;
    for (size_t from = 0;;) {
        size_t at = s.find(sep, from);
        out += s.substr(from, at == std::string::npos ? std::string::npos : at - from);
        if (at == std::string::npos) break;
        out += '|';
        from = at + sep.size();
    }
    return out;
}

struct Column {
    std::string name;      // as written: the first one is `form`
    PosAttr *pa;
    TextIterator *it;
    bool multivalue;
    std::string sep;
};

struct StructAttr {
    std::string name;
    PosAttr *pa;
    bool multivalue;
    std::string sep;
};

struct Struct {
    std::string name;
    Structure *st;
    NumOfPos n;            // ranges
    NumOfPos next = 0;     // the first range not started yet
    std::vector<StructAttr> attrs;
    long long zerowidth = 0;
    bool nested = false;
    long long written = 0;
    // open ranges: (end, range number), the one ending first on top
    std::priority_queue<std::pair<Position, NumOfPos>, std::vector<std::pair<Position, NumOfPos> >, std::greater<std::pair<Position, NumOfPos> > > open;
};

void region(std::string &out, Struct &s, NumOfPos num, Position start, Position end_incl, Recoder &rc) {
    out += "{\"type\":\"region\",\"struct\":";
    json_str(out, s.name.c_str());
    out += ",\"start_pos\":" + std::to_string(start) + ",\"end_pos\":" + std::to_string(end_incl) + ",\"attrs\":{";
    for (size_t i = 0; i < s.attrs.size(); ++i) {
        if (i) out += ',';
        json_str(out, s.attrs[i].name.c_str());
        out += ':';
        const char *v = rc(s.attrs[i].pa->pos2str(num));
        if (s.attrs[i].multivalue)
            json_str(out, multi(v, s.attrs[i].sep).c_str());
        else
            json_str(out, v);
    }
    out += "}}\n";
    ++s.written;
}

std::string json_list(const std::vector<std::string> &v) {
    std::string out = "[";
    for (size_t i = 0; i < v.size(); ++i) {
        if (i) out += ',';
        json_str(out, v[i].c_str());
    }
    return out + "]";
}

}  // namespace

extern "C" {

// Write the corpus as pando JSONL v2 to `path` ("-" = stdout). `dynamic` != 0: dynamic attributes
// too. `limit` > 0: only the first `limit` positions (a sample; regions cut at the limit).
// Answers {"tokens", "positional", "form_from" (the attribute written as `form`, null when it is
// `form`), "skipped_dynamic", "structures": {name: {"regions", "zerowidth",
// "nested"}}, "encoding", "recoded", "seconds"}, or NULL and *err.
char *fqsm_export_jsonl(void *h, const char *path, int dynamic, long long limit, char **err) {
    FILE *f = nullptr;
    std::vector<Column> cols;
    std::vector<char> iobuf(1 << 20);   // outlives f (fclose on the error path too)
    try {
        Corpus *c = fqsm::use(h);
        const std::time_t t0 = std::time(nullptr);
        const std::string encoding = opt(c, "ENCODING");
        Recoder rc(encoding);
        Position size = c->size();
        if (limit > 0 && limit < size) size = static_cast<Position>(limit);

        // positional attributes, ATTRLIST order. pando's first one is `form`: an attribute of that name
        // goes first (TEITOK-built corpora have both `word` and `form`), else the first one is renamed.
        std::vector<std::string> names, positional, skipped, multivalue;
        for (const std::string &name : fqsm::split_list(opt(c, "ATTRLIST"))) {
            if (!dynamic && !opt(c, name + ".DYNAMIC").empty())
                skipped.push_back(name);
            else
                names.push_back(name);
        }
        std::vector<std::string>::iterator form = std::find(names.begin(), names.end(), std::string("form"));
        if (form != names.end()) std::rotate(names.begin(), form, form + 1);
        std::string renamed;   // the attribute written as `form`, when it had another name
        for (const std::string &name : names) {
            std::string as = name;
            if (positional.empty() && name != "form") {
                renamed = name;
                as = "form";
            }
            Column col{as, c->get_attr(name), nullptr, truthy(opt(c, name + ".MULTIVALUE")), opt(c, name + ".MULTISEP")};
            if (col.multivalue) multivalue.push_back(col.name);
            positional.push_back(col.name);
            cols.push_back(col);
        }
        if (cols.empty()) throw std::runtime_error("the registry has no positional attributes (ATTRLIST)");

        // structures and their attributes (STRUCTATTRLIST, as decodevert reads them)
        std::vector<Struct> structs;
        std::map<std::string, std::vector<std::string>> sattrs;
        for (const std::string &sa : fqsm::split_list(opt(c, "STRUCTATTRLIST"))) {
            size_t dot = sa.find('.');
            if (dot != std::string::npos) sattrs[sa.substr(0, dot)].push_back(sa.substr(dot + 1));
        }
        std::vector<std::string> structural, zw_names, nested_names;
        std::string region_attrs = "{";
        for (const std::string &name : fqsm::split_list(opt(c, "STRUCTLIST"))) {
            Struct s;
            s.name = name;
            s.st = c->get_struct(name);
            s.n = s.st->rng->size();
            for (const std::string &a : sattrs[name]) {
                StructAttr at{a, s.st->get_attr(a), truthy(opt(c, name + "." + a + ".MULTIVALUE")), opt(c, name + "." + a + ".MULTISEP")};
                if (at.multivalue) multivalue.push_back(name + "_" + a);
                s.attrs.push_back(at);
            }
            for (NumOfPos i = 0; i < s.n; ++i) {   // what the header declares: zero-width and nested ranges
                const Position b = s.st->rng->beg_at(i), e = s.st->rng->end_at(i);
                if (b >= size) break;
                if (b == e) ++s.zerowidth;
                if (!s.nested && s.st->rng->nesting_at(i) > 0) s.nested = true;
            }
            if (s.zerowidth) zw_names.push_back(name);
            if (s.nested) nested_names.push_back(name);
            structural.push_back(name);
            std::vector<std::string> an;
            for (const StructAttr &a : s.attrs) an.push_back(a.name);
            if (region_attrs.size() > 1) region_attrs += ',';
            json_str(region_attrs, name.c_str());
            region_attrs += ':' + json_list(an);
            structs.push_back(std::move(s));
        }
        region_attrs += '}';

        std::string p = path ? path : "-";
        f = p == "-" ? stdout : std::fopen(p.c_str(), "wb");
        if (!f) throw std::runtime_error("cannot write " + p + ": " + std::strerror(errno));
        if (f != stdout) std::setvbuf(f, iobuf.data(), _IOFBF, iobuf.size());

        std::string out = "{\"type\":\"header\",\"version\":2,\"positional\":" + json_list(positional) + ",\"structural\":" + json_list(structural) +
                          ",\"region_attrs\":" + region_attrs;
        if (!nested_names.empty()) out += ",\"nested\":" + json_list(nested_names);
        if (!zw_names.empty()) out += ",\"zerowidth\":" + json_list(zw_names);
        if (!multivalue.empty()) out += ",\"multivalue\":" + json_list(multivalue);
        out += ",\"description\":";
        json_str(out, ("from the Manatee corpus " + std::string(c->get_confpath()) + " (fqs manatee-export)").c_str());
        out += "}\n";

        for (Column &col : cols) col.it = col.pa->textat(0);

        std::vector<std::pair<Struct *, NumOfPos>> ending;
        for (Position pos = 0; pos <= size; ++pos) {
            // ranges starting here: zero-width ones are written now (before the token), the others opened
            for (Struct &s : structs) {
                while (s.next < s.n && s.st->rng->beg_at(s.next) <= pos) {
                    const Position b = s.st->rng->beg_at(s.next), e = s.st->rng->end_at(s.next);
                    if (b < pos) {   // only when a range starts before the previous one (not sorted): keep it
                        if (e > b) s.open.push({std::min(e, size), s.next});
                    } else if (e <= b) {
                        region(out, s, s.next, b, b - 1, rc);
                    } else {
                        s.open.push({std::min(e, size), s.next});
                    }
                    ++s.next;
                }
            }
            if (pos == size) break;
            out += "{\"type\":\"token\",\"v\":[";
            for (size_t i = 0; i < cols.size(); ++i) {
                if (i) out += ',';
                const char *v = rc(cols[i].it->next());
                if (cols[i].multivalue)
                    json_str(out, multi(v, cols[i].sep).c_str());
                else
                    json_str(out, v);
            }
            out += "]}\n";
            // ranges whose last token this is, inner ones (started later) first
            ending.clear();
            for (Struct &s : structs) {
                while (!s.open.empty() && s.open.top().first - 1 <= pos) {
                    ending.push_back({&s, s.open.top().second});
                    s.open.pop();
                }
            }
            std::sort(ending.begin(), ending.end(), [](const std::pair<Struct *, NumOfPos> &a, const std::pair<Struct *, NumOfPos> &b) {
                return a.first->st->rng->beg_at(a.second) > b.first->st->rng->beg_at(b.second);
            });
            for (std::pair<Struct *, NumOfPos> &e : ending) region(out, *e.first, e.second, e.first->st->rng->beg_at(e.second), pos, rc);
            if (out.size() > (1 << 20)) {
                if (std::fwrite(out.data(), 1, out.size(), f) != out.size()) throw std::runtime_error("write error on " + p);
                out.clear();
            }
        }
        if (!out.empty() && std::fwrite(out.data(), 1, out.size(), f) != out.size()) throw std::runtime_error("write error on " + p);
        for (Column &col : cols) {
            delete col.it;
            col.it = nullptr;
        }
        if (f == stdout)
            std::fflush(f);
        else if (std::fclose(f) != 0)
            throw std::runtime_error("write error on " + p);
        f = nullptr;

        std::string res = "{\"tokens\":" + std::to_string(size) + ",\"positional\":" + json_list(positional) + ",\"form_from\":" +
                          (renamed.empty() ? std::string("null") : json_str(renamed)) + ",\"skipped_dynamic\":" + json_list(skipped) +
                          ",\"structures\":{";
        for (size_t i = 0; i < structs.size(); ++i) {
            if (i) res += ',';
            json_str(res, structs[i].name.c_str());
            res += ":{\"regions\":" + std::to_string(structs[i].written) + ",\"zerowidth\":" + std::to_string(structs[i].zerowidth) +
                   ",\"nested\":" + (structs[i].nested ? "true" : "false") + "}";
        }
        res += "},\"encoding\":";
        json_str(res, encoding.c_str());
        res += ",\"recoded\":" + std::string(rc.active() ? "true" : "false") + ",\"seconds\":" + std::to_string(std::time(nullptr) - t0) + "}";
        return fqsm::dup(res);
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error exporting the corpus");
    }
    for (Column &col : cols) delete col.it;
    if (f && f != stdout) std::fclose(f);
    return nullptr;
}

}  // extern "C"
