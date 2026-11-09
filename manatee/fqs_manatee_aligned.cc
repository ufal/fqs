// libfqs_manatee, parallel corpora (fqs-manatee 0.5; dev/MANATEE-NATIVE.md, "Parallel corpora").
//
// A Manatee parallel corpus is one corpus per language, each naming the others in its registry
// (ALIGNED "a,b"), segmented by the same structure (ALIGNSTRUCT, default "align"). Segment n of one
// corresponds to segment n of the other, or, when the registry has ALIGNDEF, as the level file
// PATH/align.<other> says (written by mkalign: 1:1, 1:n, n:1, n:0, 0:n).
//
// Two uses, as in KonText:
// - the filter: `[...] within CORPUS: [...]` (and `within ! CORPUS:`) is Manatee's own CQL; it only
//   needs the aligned corpus to be found, by name, in MANATEE_REGISTRY (fqsm::use puts the
//   registry folder first). The aligned match is in the result as a label ((index + 1) * 100).
// - the aligned segments of a page of hits (fqsm_kwic_aligned): what Concordance::add_aligned
//   computes per line (concord/concedit.cc), done here for the page only, so a page still needs
//   no stored concordance. Bounds are checked where Manatee does not: a segment number past the end
//   of the aligned structure (counts that differ, without ALIGNDEF) gives "no counterpart" instead of
//   reading past the end.

#include "fqs_manatee.hh"

#include "levels.hh"

#include <memory>
#include <stdexcept>

namespace fqsm {

namespace {

// one aligned corpus as seen from the queried corpus
struct Side {
    std::string name;
    Corpus *corp;
    Structure *seg;                      // its ALIGNSTRUCT
    std::unique_ptr<MLTStream> map;      // the queried corpus's level file to it (ALIGNDEF), or none
    std::vector<PosAttr *> pas;          // the page's attributes in this corpus (NULL: missing)
    int label;                           // Manatee's label for matches in it: (index in ALIGNED + 1) * 100
};

std::vector<std::string> aligned_names(Corpus *c) {
    return split_list(c->get_conf("ALIGNED"));
}

Structure *align_struct(Corpus *c) {
    const std::string &name = c->get_conf("ALIGNSTRUCT");
    if (name.empty()) throw std::runtime_error(std::string(c->get_conffile()) + ": no ALIGNSTRUCT");
    return c->get_struct(name);
}

// [first, last] segment numbers in `side` for the segment `num` of the queried corpus; false: none.
// As Concordance::add_aligned (with the queried corpus's level file when it has ALIGNDEF).
bool map_segment(Side &side, NumOfPos num, NumOfPos &first, NumOfPos &last) {
    first = last = num;
    if (side.map) {
        MLTStream *m = side.map.get();
        m->find_org(num);
        if (m->end()) return false;
        switch (m->change_type()) {
            case MLTStream::KEEP:
                first = last = num - m->orgpos() + m->newpos();
                break;
            case MLTStream::DELETE:
                return false;
            default:   // CONCAT, MORPH, INSERT: the new range
                first = m->newpos();
                last = first + m->change_newsize() - 1;
        }
    }
    const NumOfPos n = side.seg->rng->size();
    return first >= 0 && first <= last && last < n;
}

}  // namespace

class AlignedPage {
public:
    Corpus *corp;
    Structure *seg;
    std::vector<std::unique_ptr<Side>> sides;
};

AlignedPage *aligned_page_new(Corpus *c, const std::string &which, const std::vector<std::string> &attr_names) {
    std::vector<std::string> all = aligned_names(c);
    if (all.empty()) throw std::runtime_error(std::string(c->get_conffile()) + " is not a parallel corpus (no ALIGNED in its registry)");
    std::vector<std::string> wanted = which == "*" || which.empty() ? all : split_list(which);
    std::unique_ptr<AlignedPage> page(new AlignedPage());
    page->corp = c;
    page->seg = align_struct(c);
    const bool aligndef = !c->get_conf("ALIGNDEF").empty();
    for (const std::string &name : wanted) {
        int index = -1;
        for (size_t i = 0; i < all.size(); ++i)
            if (all[i] == name) index = static_cast<int>(i);
        if (index < 0) throw std::runtime_error(name + " is not aligned with " + c->get_conffile() + " (ALIGNED: " + c->get_conf("ALIGNED") + ")");
        std::unique_ptr<Side> s(new Side());
        s->name = name;
        s->corp = c->get_aligned(name);
        s->seg = align_struct(s->corp);
        if (aligndef) s->map.reset(full_level(c->get_aligned_level(s->corp->get_conffile())));
        for (const std::string &a : attr_names) {
            PosAttr *pa = nullptr;
            try {
                pa = s->corp->get_attr(a);
            } catch (...) {
            }
            s->pas.push_back(pa);
        }
        s->label = (index + 1) * 100;
        page->sides.push_back(std::move(s));
    }
    return page.release();
}

// the hit starting at `beg`: [{"corpus", "pos", "end", "tokens", "match"?}, …]
void aligned_page_hit(AlignedPage *a, std::string &out, Position beg, RangeStream *rs) {
    Labels labels;
    if (rs) rs->add_labels(labels);
    // the segment the hit starts in (as add_aligned: the first one at or after the hit; none when the
    // hit is outside every segment)
    NumOfPos num = a->seg->rng->num_next_pos(beg);
    const bool inside = num >= 0 && num < a->seg->rng->size() && a->seg->rng->beg_at(num) <= beg;
    out += '[';
    for (size_t i = 0; i < a->sides.size(); ++i) {
        Side &s = *a->sides[i];
        if (i) out += ',';
        out += "{\"corpus\":" + json_str(s.name);
        NumOfPos first = 0, last = 0;
        if (inside && map_segment(s, num, first, last)) {
            const Position sb = s.seg->rng->beg_at(first), se = s.seg->rng->end_at(last);
            out += ",\"pos\":" + std::to_string(sb) + ",\"end\":" + std::to_string(se) + ",\"tokens\":";
            tokens(out, s.pas, sb, se);
        } else {
            out += ",\"pos\":null,\"end\":null,\"tokens\":[]";
        }
        Labels::const_iterator mb = labels.find(s.label);
        if (mb != labels.end()) {
            Labels::const_iterator me = labels.find(-s.label);
            const Position e = me != labels.end() && me->second > mb->second ? me->second : mb->second + 1;
            out += ",\"match\":{\"pos\":" + std::to_string(mb->second) + ",\"end\":" + std::to_string(e) + "}";
        }
        out += '}';
    }
    out += ']';
}

std::string aligned_page_names(AlignedPage *a) {
    std::string out = "[";
    for (size_t i = 0; i < a->sides.size(); ++i) {
        if (i) out += ',';
        out += json_str(a->sides[i]->name);
    }
    return out + "]";
}

void aligned_page_free(AlignedPage *a) {
    delete a;
}

std::string aligned_check(Corpus *c, int &checked, std::string &problem) {
    std::string out = "[";
    bool first_entry = true;
    for (const std::string &name : aligned_names(c)) {
        if (!first_entry) out += ',';
        first_entry = false;
        out += "{\"corpus\":" + json_str(name);
        try {
            Corpus *ac = c->get_aligned(name);
            std::string p = read_problem(ac, checked);
            if (!p.empty()) {
                problem = "aligned corpus " + name + ": " + p;
                out += ",\"ok\":false,\"error\":" + json_str(p) + "}";
                continue;
            }
            const NumOfPos mine = align_struct(c)->rng->size(), theirs = align_struct(ac)->rng->size();
            out += ",\"ok\":true,\"segments\":" + std::to_string(theirs);
            if (c->get_conf("ALIGNDEF").empty() && mine != theirs) {
                out += ",\"warning\":" + json_str("aligned by segment number (no ALIGNDEF), but " + std::string(c->get_conffile()) + " has " +
                                                  std::to_string(mine) + " " + c->get_conf("ALIGNSTRUCT") + " and " + name + " " +
                                                  std::to_string(theirs) + ": segments past the shorter one have no counterpart");
            } else if (!c->get_conf("ALIGNDEF").empty()) {
                c->get_aligned_level(ac->get_conffile());   // the level file opens (kept by the corpus)
            }
            out += '}';
        } catch (std::exception &e) {
            out += ",\"ok\":false,\"error\":" + json_str(e.what()) + "}";
        } catch (...) {
            out += ",\"ok\":false,\"error\":\"cannot be opened\"}";
        }
    }
    return out + "]";
}

}  // namespace fqsm
