/* yyjson doing the same job: sum the "score" field of every record. */

#include "yyjson.h"

extern const unsigned char *volatile INPUT_P;
extern const size_t INPUT_LEN;
void finish(int64_t total);

void _start(void) {
    int64_t total = 0;
    yyjson_doc *doc =
        yyjson_read((const char *)INPUT_P, INPUT_LEN, 0);
    if (doc) {
        yyjson_val *root = yyjson_doc_get_root(doc);
        size_t idx, max;
        yyjson_val *rec;
        yyjson_arr_foreach(root, idx, max, rec) {
            yyjson_val *v = yyjson_obj_get(rec, "score");
            if (v) total += yyjson_get_sint(v);
        }
        yyjson_doc_free(doc);
    }
    finish(total);
}
