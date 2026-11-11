// libfqs_manatee: keywords (fqs-manatee 0.9): the words of a focus corpus that are more frequent in
// it than in a reference corpus, Manatee's Keyword as KonText calls it (bgcalc/keywords). The words
// considered are those of the focus corpus's word list for the pattern, as KonText takes them first.
//
// Licence: links manatee-open (GPL-2); this library is GPL-2 as well, and optional for FQS.

#include "fqs_manatee.hh"
#include "keyword.hh"

#include <cmath>

using fqsm::dup;
using fqsm::json_str;
using fqsm::set_err;
using fqsm::use;

extern "C" {

// Keywords of `attr` in the corpus of handle `h` against the one of handle `ref` (the same or another
// corpus): the words matching `pattern` (minus the registry's non-words unless `include_nonwords`) with
// at least `wl_minfreq` occurrences are compared; `simple_n` the smoothing constant, `max_items` how many,
// `minfreq` / `maxfreq` limits of the frequency in the focus corpus (0: none).
// {"items": [{"str", "score", "freqs": [frq1, frq2, rel1, rel2]}], "totals": [words, frq1, frq2]}
char *fqsm_keywords(void *h, void *ref, const char *attr, const char *pattern, int include_nonwords, const char *nums,
                    double wl_minfreq, double simple_n, int max_items, int minfreq, int maxfreq, char **err) {
    try {
        Corpus *c = use(h);
        Corpus *rc = use(ref);
        use(h);   // (the registry folders: the focus corpus's first again)
        const std::string name = attr ? attr : "";
        const std::string excl = include_nonwords ? "" : c->get_conf("NONWORDRE");
        std::vector<std::string> found = fqsm::pattern_words(c, name, pattern ? pattern : "", excl, (nums && *nums) ? nums : "frq", wl_minfreq);
        if (found.empty()) return dup("{\"items\":[],\"totals\":[0,0,0]}");
        std::unordered_set<std::string> whitelist(found.begin(), found.end()), blacklist;
        std::vector<std::string> addfreqs, pos_filters, neg_filters;
        Keyword kw(c, rc, c->get_attr(name), rc->get_attr(name), static_cast<float>(simple_n), static_cast<unsigned>(max_items),
                   minfreq, maxfreq, blacklist, whitelist, "frq", addfreqs, pos_filters, neg_filters, nullptr);
        std::string items;
        while (const kwitem *k = kw.next()) {
            if (!items.empty()) items += ',';
            char buf[160];
            std::snprintf(buf, sizeof buf, ",\"score\":%.9g,\"freqs\":[%.17g,%.17g,%.17g,%.17g]}", static_cast<double>(k->score), k->freqs[0], k->freqs[1], k->freqs[2], k->freqs[3]);
            items += "{\"str\":" + json_str(k->str) + buf;
        }
        return dup("{\"items\":[" + items + "],\"totals\":[" + std::to_string(static_cast<long long>(kw.totalcount)) + "," +
                   std::to_string(static_cast<long long>(kw.totalfreq1)) + "," + std::to_string(static_cast<long long>(kw.totalfreq2)) + "]}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (CorpInfoNotFound *e) {
        set_err(err, "Invalid regular expression for the keywords");
    } catch (...) {
        set_err(err, "unknown error in the keywords");
    }
    return nullptr;
}

}  // extern "C"
