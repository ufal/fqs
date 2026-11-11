// libfqs_manatee: KonText's KWIC lines (fqs-manatee 0.8). KonText builds its concordance lines with
// Manatee's KWICLines (lib/kwiclib: context `-40#` / `40#` or `-1:s`, attributes, structures, refs)
// and turns its raw output into the page. fqsm_kwic_ex gives that raw output per hit, made by the
// same class, so a KonText page through FQS is built from exactly what KonText's own route reads.
//
// KWICLines walks a RangeStream of its own. Here it gets a stream of one hit at a time (OneHit),
// fed from the page walk in fqs_manatee.cc, so no stored concordance is needed and the walk stays
// the one that also counts.

#include "fqs_manatee.hh"
#include "concget.hh"

#include <limits>

namespace {

// A RangeStream holding the one hit it was last given (its labels too), ended once read.
class OneHit : public RangeStream {
    Position beg_ = 0, end_ = 0, final_ = 0;
    bool done_ = true;
    Labels lab_;
public:
    explicit OneHit(Position final) : final_(final) {}
    void set(Position b, Position e, RangeStream *from) {
        beg_ = b;
        end_ = e;
        done_ = false;
        lab_.clear();
        if (from) from->add_labels(lab_);
    }
    virtual bool end() const { return done_; }
    virtual bool next() { done_ = true; return false; }
    virtual Position peek_beg() const { return done_ ? final_ : beg_; }
    virtual Position peek_end() const { return done_ ? final_ : end_; }
    virtual void add_labels(Labels &lab) const {
        for (Labels::const_iterator i = lab_.begin(); i != lab_.end(); ++i) lab[i->first] = i->second;
    }
    virtual Position find_beg(Position) { return peek_beg(); }
    virtual Position find_end(Position) { return peek_end(); }
    virtual NumOfPos rest_min() const { return done_ ? 0 : 1; }
    virtual NumOfPos rest_max() const { return done_ ? 0 : 1; }
    virtual Position final() const { return final_; }
    virtual int nesting() const { return 0; }
    virtual bool epsilon() const { return false; }
};

void tokens_json(std::string &out, const Tokens &t) {
    out += '[';
    for (size_t i = 0; i < t.size(); ++i) {
        if (i) out += ',';
        fqsm::json_str(out, t[i].c_str());
    }
    out += ']';
}

// a field of the view spec: fields separated by \x1f
std::vector<std::string> spec_fields(const char *spec) {
    std::vector<std::string> out;
    std::string s(spec ? spec : "");
    size_t from = 0;
    for (;;) {
        size_t at = s.find('\x1f', from);
        out.push_back(s.substr(from, at == std::string::npos ? std::string::npos : at - from));
        if (at == std::string::npos) break;
        from = at + 1;
    }
    while (out.size() < 6) out.push_back("");
    return out;
}

}  // namespace

namespace fqsm {

class KwicView {
public:
    OneHit *hit;          // owned by kl
    KWICLines *kl;
    KwicView(Corpus *c, const char *spec) : hit(nullptr), kl(nullptr) {
        std::vector<std::string> f = spec_fields(spec);
        hit = new OneHit(std::numeric_limits<Position>::max());
        // left, right, attrs, ctxattrs, structs, refs: as KonText's kwiclib passes them
        kl = new KWICLines(c, hit, f[0].c_str(), f[1].c_str(), f[2].c_str(), f[3].c_str(), f[4].c_str(), f[5].c_str());
    }
    ~KwicView() { delete kl; }   // deletes `hit`
};

KwicView *kwic_view_new(Corpus *c, const char *spec) { return new KwicView(c, spec); }
void kwic_view_free(KwicView *v) { delete v; }

// the line KWICLines is at, as the "view" object (see kwic_view_hit)
static void line_json(KWICLines *kl, std::string &out) {
    out += "\"pos\":" + std::to_string(kl->get_pos()) + ",\"kwiclen\":" + std::to_string(kl->get_kwiclen()) +
           ",\"ctxbeg\":" + std::to_string(kl->get_ctxbeg()) + ",\"ctxend\":" + std::to_string(kl->get_ctxend()) +
           ",\"linegroup\":" + std::to_string(kl->get_linegroup()) + ",\"refs\":";
    tokens_json(out, kl->get_ref_list());
    out += ",\"left\":";
    tokens_json(out, kl->get_left());
    out += ",\"kwic\":";
    tokens_json(out, kl->get_kwic());
    out += ",\"right\":";
    tokens_json(out, kl->get_right());
}

// ,"view":{"pos","kwiclen","ctxbeg","ctxend","linegroup","refs":[…],"left":[str, class, …],"kwic":[…],"right":[…]}
void kwic_view_hit(KwicView *v, std::string &out, Position beg, Position end, RangeStream *rs) {
    v->hit->set(beg, end, rs);
    out += ",\"view\":{";
    if (v->kl->nextline()) line_json(v->kl, out);
    out += '}';
}

// Lines [from, to) of a stored concordance (in its view order: sorted, shuffled), as KonText's
// kwiclib reads them: KWICLines over conc->RS(true, from, to). Appends the hits as JSON objects
// {"pos","end","doc"?,"view":{…}} separated by commas.
void kwic_conc_lines(Corpus *c, Concordance *conc, long long from, long long to, const char *spec,
                     Structure *doc_st, PosAttr *doc_pa, AlignedPage *al, std::string &out) {
    std::vector<std::string> f = spec_fields(spec);
    KWICLines kl(c, conc->RS(true, from, to), f[0].c_str(), f[1].c_str(), f[2].c_str(), f[3].c_str(), f[4].c_str(), f[5].c_str());
    RangeStream *hits = al ? conc->RS(true, from, to) : nullptr;   // the same lines again, for the aligned segments
    bool first = true;
    while (kl.nextline()) {
        if (!first) out += ',';
        first = false;
        out += "{\"pos\":" + std::to_string(kl.get_pos()) + ",\"end\":" + std::to_string(kl.get_pos() + kl.get_kwiclen());
        if (doc_pa) {
            const NumOfPos n = doc_st->rng->num_at_pos(kl.get_pos());
            out += ",\"doc\":";
            if (n >= 0) json_str(out, doc_pa->pos2str(n)); else out += "null";
        }
        if (hits && !hits->end()) {
            out += ",\"aligned\":";
            aligned_page_hit(al, out, hits->peek_beg(), hits);
            hits->next();
        }
        out += ",\"view\":{";
        line_json(&kl, out);
        out += "}}";
    }
    delete hits;
}

// Manatee's ARF (finlib/fsstat.cc compute_ARF) over the hits' starts in corpus order.
double arf(const std::vector<Position> &begs, Position size) {
    long long freq = (long long)begs.size();
    if (freq <= 0) return 0.0;
    double v = double(size) / freq;
    double sum = 0.0;
    Position prev = begs[0];
    double first = prev;
    for (long long i = 1; i < freq; ++i) {
        Position curr = begs[i];
        double d = curr - prev;
        prev = curr;
        sum += d < v ? d / v : 1.0;
    }
    first += size - prev;
    sum += first < v ? first / v : 1.0;
    return sum;
}

}  // namespace fqsm
