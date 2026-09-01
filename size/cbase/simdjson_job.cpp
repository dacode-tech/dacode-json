/* simdjson doing the same job, via the DOM API.
 *
 * Expected to fail: simdjson is C++17 and reaches for `<string>`,
 * `<vector>`, `<memory>` and friends, which do not exist on a target with
 * no C++ standard library. That failure is a finding — record what it
 * says rather than working around it.
 */

#include "simdjson.h"

extern "C" {
extern const unsigned char *volatile INPUT_P;
extern const size_t INPUT_LEN;
void finish(int64_t total);

void _start() {
    int64_t total = 0;
    simdjson::dom::parser parser;
    simdjson::dom::element doc;
    auto err = parser
                   .parse(reinterpret_cast<const char *>(
                              const_cast<const unsigned char *>(INPUT_P)),
                          INPUT_LEN)
                   .get(doc);
    if (!err) {
        for (auto rec : doc.get_array()) {
            int64_t v;
            if (!rec["score"].get_int64().get(v)) total += v;
        }
    }
    finish(total);
}
}
