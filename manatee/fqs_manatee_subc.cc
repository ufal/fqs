// libfqs_manatee: subcorpora (fqs-manatee 0.7). FQS's stored subcorpora (src/subcorpus/) on native
// Manatee: a `.subc` file in Manatee's own format, written by Manatee's create_subcorpus (the call
// KonText's subcorpus form makes), and a corpus opened as such a subcorpus (Manatee's SubCorpus:
// queries see only its ranges, sizes are its size), used by every other function of this library.
//
// Licence: links manatee-open (GPL-2); this library is GPL-2 as well, and optional for FQS.

#include "fqs_manatee.hh"
#include "subcorp.hh"

#include <string>

using fqsm::dup;
using fqsm::set_err;
using fqsm::use;

extern "C" {

// Write the subcorpus file `path`:
// - `structure` set: the regions of that structure whose attributes match `query`, a structure
//   attribute condition as KonText writes it for text types (`genre="fiction" & year="19.*"`);
// - `structure` empty: the positions of the hits of the token query `query` (Manatee CQL).
// {"written": true} or {"written": false} when nothing is selected (then no file is written).
char *fqsm_subc_create(void *h, const char *path, const char *structure, const char *query, char **err) {
    try {
        Corpus *c = use(h);
        bool written;
        if (structure && *structure) {
            written = create_subcorpus(path, c, structure, query ? query : "");
        } else {
            written = create_subcorpus(path, fqsm::eval(c, query), NULL);
        }
        return dup(std::string("{\"written\":") + (written ? "true" : "false") + "}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error writing the subcorpus");
    }
    return nullptr;
}

// The corpus of handle `h` opened as the subcorpus `path` (a .subc file): a new handle, closed with
// fqsm_close, independent of `h`. NULL and *err on failure.
void *fqsm_open_subc(void *h, const char *path, char **err) {
    try {
        fqsm::Handle *hd = static_cast<fqsm::Handle *>(h);
        Corpus *c = use(h);
        SubCorpus *sc = new SubCorpus(c, path ? path : "");
        return new fqsm::Handle{sc, hd->regdir};
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error opening the subcorpus");
    }
    return nullptr;
}

// {"size": the corpus's positions, "search_size": the positions a query sees (a subcorpus: its size)}
char *fqsm_sizes(void *h, char **err) {
    try {
        Corpus *c = use(h);
        return dup("{\"size\":" + std::to_string(static_cast<long long>(c->size())) +
                   ",\"search_size\":" + std::to_string(static_cast<long long>(c->search_size())) + "}");
    } catch (std::exception &e) {
        set_err(err, e.what());
    } catch (...) {
        set_err(err, "unknown error");
    }
    return nullptr;
}

}  // extern "C"
