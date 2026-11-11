// libfqs_manatee: word lists (fqs-manatee 0.9). KonText's word list (bgcalc/wordlist) reads a
// positional attribute's lexicon with its precompiled frequencies (frq, arf, docf), filters it by a
// regular expression, a list of wanted words and a list of unwanted ones, and keeps the words with
// at least the minimum frequency; for a structure attribute the "frequency" of a value is the size
// of its regions in tokens ("doc sizes") or their number. This is that function, on the corpus as
// FQS has it (under a subcorpus whose precompiled frequencies are missing, the frequencies are
// counted from the subcorpus: a frequency distribution of the attribute over `[]`).
//
// Licence: links manatee-open (GPL-2); this library is GPL-2 as well, and optional for FQS.

#include "fqs_manatee.hh"
#include "wordlist.hh"

#include <algorithm>
#include <cmath>
#include <map>
#include <set>

using fqsm::dup;
using fqsm::json_str;
using fqsm::set_err;
using fqsm::use;

namespace {

std::set<std::string> lines_of(const char *s) {
    std::set<std::string> out;
    std::string t(s ? s : "");
    for (size_t from = 0; from <= t.size();) {
        size_t at = t.find('\n', from);
        std::string w = t.substr(from, at == std::string::npos ? std::string::npos : at - from);
        if (!w.empty()) out.insert(w);
        if (at == std::string::npos) break;
        from = at + 1;
    }
    return out;
}

// the frequency source of a positional attribute: its compiled statistics, else (subcorpus) counts
class Freqs {
    Frequency *stat_ = nullptr;
    std::map<std::string, double> counted_;
    bool use_counted_ = false;
    PosAttr *pa_;
public:
    Freqs(Corpus *c, PosAttr *pa, const std::string &attrname, const std::string &nums) : pa_(pa) {
        try {
            stat_ = pa->get_stat(nums.c_str());
        } catch (...) {
            stat_ = nullptr;
        }
        if (!stat_) {
            if (nums != "frq" || c->search_size() == c->size())
                throw std::runtime_error("the " + nums + " statistics of '" + attrname + "' are not compiled");
            // a subcorpus without compiled frequencies: count them
            std::vector<std::string> words;
            std::vector<NumOfPos> freqs, norms;
            c->freq_dist(fqsm::eval(c, "[]"), (attrname + " 0").c_str(), 0, words, freqs, norms);
            for (size_t i = 0; i < words.size(); ++i) counted_[words[i]] = static_cast<double>(freqs[i]);
            use_counted_ = true;
        }
    }
    ~Freqs() { delete stat_; }
    double operator()(int id) {
        if (use_counted_) {
            auto it = counted_.find(pa_->id2str(id));
            return it == counted_.end() ? 0.0 : it->second;
        }
        return stat_->freq(id);
    }
};

void add_item(std::string &items, const std::string &word, double frq, bool arf) {
    if (!items.empty()) items += ',';
    items += "[";
    json_str(items, word.c_str());
    char buf[64];
    if (arf) std::snprintf(buf, sizeof buf, ",%.1f]", std::round(frq * 10) / 10);
    else std::snprintf(buf, sizeof buf, ",%lld]", static_cast<long long>(std::llround(frq)));
    items += buf;
}

}  // namespace


namespace fqsm {

// The values of positional attribute `attrname` that match `pattern` (and not the non-word expression
// `excl`) and occur at least `minfreq` times by statistic `nums` (KonText's keywords start from the
// focus corpus's word list).
std::vector<std::string> pattern_words(Corpus *c, const std::string &attrname, const std::string &pattern,
                                       const std::string &excl, const std::string &nums, double minfreq) {
    PosAttr *pa = c->get_attr(attrname);
    Freqs freq(c, pa, attrname, nums);
    std::vector<std::string> out;
    Generator<int> *gen = pa->regexp2ids(pattern.c_str(), false, excl.empty() ? nullptr : excl.c_str());
    try {
        while (!gen->end()) {
            const int id = gen->next();
            const double f = freq(id);
            if (f != 0 && f >= minfreq) out.push_back(pa->id2str(id));
        }
    } catch (...) {
        delete gen;
        throw;
    }
    delete gen;
    return out;
}

}  // namespace fqsm

extern "C" {

// A word list of attribute `attr` ("lemma", or "text.id" for a structure attribute).
//   pattern/excl_in: regular expression and the non-word expression to leave out ("" for none);
//   leave_out_nonwords: with no `excl_in`, leave out the words the registry's NONWORDRE matches;
//   nums: "frq" | "docf" | "arf" (positional), "doc sizes" | "doc counts" (structure attribute);
//   pfilter / nfilter: wanted / unwanted words, newline-separated ("" for none);
//   minfreq: the lowest frequency kept.
// {"items": [[word, frequency], …], "total": N} in the lexicon's order (the caller sorts), all of them.
char *fqsm_wordlist(void *h, const char *attr, const char *pattern, const char *excl_in, int leave_out_nonwords, const char *nums,
                    double minfreq, const char *pfilter, const char *nfilter, char **err) {
    try {
        Corpus *c = use(h);
        std::string excl_s = excl_in ? excl_in : "";
        if (excl_s.empty() && leave_out_nonwords) excl_s = c->get_conf("NONWORDRE");   // the registry's expression of non-words
        const char *excl = excl_s.c_str();
        const std::string name = attr ? attr : "";
        const std::string num = nums ? nums : "frq";
        const std::string pat = pattern ? pattern : "";
        const bool arf = num == "arf";
        const std::set<std::string> want = lines_of(pfilter), unwanted = lines_of(nfilter);
        std::string items;
        long long total = 0;
        const size_t dot = name.find('.');
        if (dot != std::string::npos) {   // a structure attribute: the size / number of its regions per value
            Structure *st = c->get_struct(name.substr(0, dot));
            PosAttr *pa = st->get_attr(name.substr(dot + 1));
            const int range = pa->id_range();
            std::vector<double> by_id(range > 0 ? range : 0, 0.0);
            const NumOfPos n = st->size();
            for (NumOfPos i = 0; i < n; ++i) {
                const int id = pa->pos2id(i);
                if (id < 0 || id >= range) continue;
                by_id[id] += num == "doc sizes" ? static_cast<double>(st->rng->end_at(i) - st->rng->beg_at(i)) : 1.0;
            }
            Generator<int> *gen = pat.empty() || pat == ".*" ? nullptr : pa->regexp2ids(pat.c_str(), false, nullptr);
            auto take = [&](int id) {
                const std::string v = pa->id2str(id);
                const double f = by_id[id];
                if (f < minfreq || (!want.empty() && !want.count(v)) || unwanted.count(v)) return;
                add_item(items, v, f, false);
                ++total;
            };
            if (!gen) {
                for (int id = 0; id < range; ++id) take(id);
            } else {
                while (!gen->end()) take(gen->next());
                delete gen;
            }
            return dup("{\"items\":[" + items + "],\"total\":" + std::to_string(total) + "}");
        }
        PosAttr *pa = c->get_attr(name);
        Freqs freq(c, pa, name, num);
        if (!want.empty() && (pat == ".*" || pat == ".+" || pat.empty())) {   // just the given words
            std::string pf(pfilter ? pfilter : "");
            for (const std::string &w : want) {
                const int id = pa->str2id(w.c_str());
                const double f = id == -1 ? 0.0 : freq(id);
                if (f >= minfreq && !unwanted.count(w)) {
                    add_item(items, w, f, arf);
                    ++total;
                }
            }
            return dup("{\"items\":[" + items + "],\"total\":" + std::to_string(total) + "}");
        }
        Generator<int> *gen = pa->regexp2ids(pat.c_str(), false, (excl && *excl) ? excl : nullptr);
        try {
            while (!gen->end()) {
                const int id = gen->next();
                const double f = freq(id);
                if (f == 0) continue;
                const std::string v = pa->id2str(id);
                if (f >= minfreq && (want.empty() || want.count(v)) && !unwanted.count(v)) {
                    add_item(items, v, f, arf);
                    ++total;
                }
            }
        } catch (...) {
            delete gen;
            throw;
        }
        delete gen;
        return dup("{\"items\":[" + items + "],\"total\":" + std::to_string(total) + "}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error in the word list");
    }
    return nullptr;
}

}  // extern "C"
