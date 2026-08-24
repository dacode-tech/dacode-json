/* Whole-workload shims over simdjson, mirroring vendor/shim.c.
 *
 * Uses the On Demand API, which is simdjson's recommended and fastest
 * interface — it is lazy, so a query that reads one field does not pay for
 * the whole document. That makes it the strongest available opponent for
 * the tier-3 pool, and the fair one to quote.
 *
 * Parsers are kept in thread-local storage so buffer reuse matches what the
 * Rust side does with a reusable Workspace.
 */

#include "simdjson/simdjson.h"
#include <cstring>
#include <string>

using namespace simdjson;

namespace {
/* Reused across calls, exactly like the Rust Workspace. */
thread_local ondemand::parser od_parser;
thread_local dom::parser dom_parser;
} // namespace

extern "C" {

/* ---- parse only ------------------------------------------------------ */

/* DOM parse. `dat` must have SIMDJSON_PADDING spare bytes. */
size_t vj_sj_parse_dom(const char *dat, size_t len, size_t cap) {
    dom::element doc;
    auto err = dom_parser.parse(dat, len, false).get(doc);
    (void)cap;
    if (err) return 0;
    return len;
}

/* On Demand: iterate the whole document so the work is comparable to a
 * full parse rather than a header check. */
size_t vj_sj_parse_ondemand(const char *dat, size_t len, size_t cap) {
    padded_string_view view(dat, len, cap);
    ondemand::document doc;
    if (od_parser.iterate(view).get(doc)) return 0;
    size_t n = 0;
    ondemand::array arr;
    if (doc.get_array().get(arr)) return 0;
    for (auto rec : arr) {
        ondemand::object obj;
        if (rec.get_object().get(obj)) continue;
        for (auto field : obj) { (void)field; n++; }
    }
    return n;
}

/* ---- parse + query --------------------------------------------------- */

long long vj_sj_sum_field(const char *dat, size_t len, size_t cap, const char *key) {
    padded_string_view view(dat, len, cap);
    ondemand::document doc;
    if (od_parser.iterate(view).get(doc)) return 0;
    long long sum = 0;
    ondemand::array arr;
    if (doc.get_array().get(arr)) return 0;
    for (auto rec : arr) {
        int64_t v;
        if (!rec[key].get_int64().get(v)) sum += v;
    }
    return sum;
}

/* One field from the first record. On Demand can stop early here, which is
 * the whole point of a lazy parser. */
long long vj_sj_first_field(const char *dat, size_t len, size_t cap, const char *key) {
    padded_string_view view(dat, len, cap);
    ondemand::document doc;
    if (od_parser.iterate(view).get(doc)) return -1;
    ondemand::array arr;
    if (doc.get_array().get(arr)) return -1;
    for (auto rec : arr) {
        std::string_view sv;
        if (!rec[key].get_string().get(sv)) return (long long)sv.size();
        return -1;
    }
    return -1;
}

unsigned long long vj_sj_extract_all(const char *dat, size_t len, size_t cap) {
    padded_string_view view(dat, len, cap);
    ondemand::document doc;
    if (od_parser.iterate(view).get(doc)) return 0;
    unsigned long long acc = 0;
    ondemand::array arr;
    if (doc.get_array().get(arr)) return 0;
    for (auto rec : arr) {
        ondemand::object obj;
        if (rec.get_object().get(obj)) continue;
        for (auto field : obj) {
            std::string_view k;
            if (field.unescaped_key().get(k)) continue;
            acc += k.size();
            ondemand::value v = field.value();
            switch (v.type().value_unsafe()) {
                case ondemand::json_type::string: {
                    std::string_view sv;
                    if (!v.get_string().get(sv)) acc += sv.size();
                    break;
                }
                case ondemand::json_type::number: {
                    int64_t n;
                    if (!v.get_int64().get(n)) acc += (unsigned long long)n;
                    break;
                }
                case ondemand::json_type::boolean: {
                    bool b;
                    if (!v.get_bool().get(b)) acc += b ? 1u : 0u;
                    break;
                }
                case ondemand::json_type::array: {
                    ondemand::array inner;
                    if (!v.get_array().get(inner)) {
                        size_t c = 0;
                        if (!inner.count_elements().get(c)) acc += c;
                    }
                    break;
                }
                default: break;
            }
        }
    }
    return acc;
}

/* ---- validate -------------------------------------------------------- */

int vj_sj_validate(const char *dat, size_t len, size_t cap) {
    padded_string_view view(dat, len, cap);
    ondemand::document doc;
    if (od_parser.iterate(view).get(doc)) return 0;
    /* On Demand validates lazily, so the document must be walked for this
     * to mean anything. */
    return doc.raw_json().error() == SUCCESS ? 1 : 0;
}

/* ---- info ------------------------------------------------------------ */

const char *vj_sj_version(void) { return SIMDJSON_VERSION; }

/* `name()` returns a std::string by value in some simdjson versions, so
 * `.c_str()` on it would dangle. Copy into a function-local static. */
const char *vj_sj_implementation(void) {
    static std::string cached = simdjson::get_active_implementation()->name();
    return cached.c_str();
}
size_t vj_sj_padding(void) { return SIMDJSON_PADDING; }

} // extern "C"
