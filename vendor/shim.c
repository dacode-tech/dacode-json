/* Whole-workload shims over yyjson.
 *
 * Almost all of yyjson's API is `static inline` in yyjson.h, so those
 * functions have no linkable symbols. Rust FFI therefore needs C wrappers.
 *
 * Rather than expose fine-grained accessors and pay an FFI call per field,
 * each function here performs one complete benchmark workload — exactly the
 * workload the Rust side performs — so the comparison measures the parsers
 * and not the calling convention. yyjson gets the friendlier deal in every
 * case: it is doing its work entirely inside C with no boundary crossings.
 */

#include "yyjson/yyjson.h"
#include <string.h>

/* ---- parse only ------------------------------------------------------ */

/* Full DOM build and teardown. Returns the value count, or 0 on failure. */
size_t vj_yy_parse(const char *dat, size_t len) {
    yyjson_doc *doc = yyjson_read(dat, len, 0);
    if (!doc) return 0;
    size_t n = yyjson_doc_get_val_count(doc);
    yyjson_doc_free(doc);
    return n;
}

/* Same, with YYJSON_READ_INSITU. Mutates `dat`, which must have
 * YYJSON_PADDING_SIZE spare bytes. This is yyjson's fastest read path. */
size_t vj_yy_parse_insitu(char *dat, size_t len) {
    yyjson_doc *doc = yyjson_read_opts(dat, len, YYJSON_READ_INSITU, NULL, NULL);
    if (!doc) return 0;
    size_t n = yyjson_doc_get_val_count(doc);
    yyjson_doc_free(doc);
    return n;
}

/* Reuse a single allocator across parses, the closest analogue to the Rust
 * port's reusable Workspace. */
typedef struct {
    yyjson_alc alc;
    void      *buf;
    size_t     size;
} vj_pool;

vj_pool *vj_pool_new(size_t size) {
    vj_pool *p = (vj_pool *)malloc(sizeof(vj_pool));
    if (!p) return NULL;
    p->buf = malloc(size);
    if (!p->buf) { free(p); return NULL; }
    p->size = size;
    yyjson_alc_pool_init(&p->alc, p->buf, size);
    return p;
}

void vj_pool_free(vj_pool *p) {
    if (!p) return;
    free(p->buf);
    free(p);
}

size_t vj_yy_parse_pool(vj_pool *p, const char *dat, size_t len) {
    if (!p) return 0;
    yyjson_alc_pool_init(&p->alc, p->buf, p->size);
    yyjson_doc *doc = yyjson_read_opts((char *)dat, len, 0, &p->alc, NULL);
    if (!doc) return 0;
    size_t n = yyjson_doc_get_val_count(doc);
    /* No free needed: the pool owns the memory and is reset next call. */
    return n;
}

/* ---- parse + query --------------------------------------------------- */

/* Parse an array of objects and sum one integer field across all of them.
 * Mirrors `query_sum_field` on the Rust side. */
long long vj_yy_sum_field(const char *dat, size_t len, const char *key) {
    yyjson_doc *doc = yyjson_read(dat, len, 0);
    if (!doc) return 0;
    long long sum = 0;
    yyjson_val *root = yyjson_doc_get_root(doc);
    size_t idx, max;
    yyjson_val *item;
    yyjson_arr_foreach(root, idx, max, item) {
        yyjson_val *v = yyjson_obj_get(item, key);
        if (v) sum += yyjson_get_sint(v);
    }
    yyjson_doc_free(doc);
    return sum;
}

/* Parse, then read one field from the first element. Mirrors
 * `query_first_only`: dominated by parse cost for an eager DOM. */
long long vj_yy_first_field(const char *dat, size_t len, const char *key) {
    yyjson_doc *doc = yyjson_read(dat, len, 0);
    if (!doc) return -1;
    long long out = -1;
    yyjson_val *root = yyjson_doc_get_root(doc);
    yyjson_val *first = yyjson_arr_get(root, 0);
    if (first) {
        yyjson_val *v = yyjson_obj_get(first, key);
        if (v) out = (long long)yyjson_get_len(v);
    }
    yyjson_doc_free(doc);
    return out;
}

/* Parse and touch every field of every record, accumulating so nothing can
 * be optimised away. Mirrors `query_extract_all`. */
unsigned long long vj_yy_extract_all(const char *dat, size_t len) {
    yyjson_doc *doc = yyjson_read(dat, len, 0);
    if (!doc) return 0;
    unsigned long long acc = 0;
    yyjson_val *root = yyjson_doc_get_root(doc);
    size_t idx, max;
    yyjson_val *rec;
    yyjson_arr_foreach(root, idx, max, rec) {
        size_t kidx, kmax;
        yyjson_val *k, *v;
        yyjson_obj_foreach(rec, kidx, kmax, k, v) {
            acc += yyjson_get_len(k);
            switch (yyjson_get_type(v)) {
                case YYJSON_TYPE_STR:  acc += yyjson_get_len(v); break;
                case YYJSON_TYPE_NUM:  acc += (unsigned long long)yyjson_get_sint(v); break;
                case YYJSON_TYPE_BOOL: acc += yyjson_get_bool(v) ? 1u : 0u; break;
                case YYJSON_TYPE_ARR:  acc += yyjson_arr_size(v); break;
                default: break;
            }
        }
    }
    yyjson_doc_free(doc);
    return acc;
}

/* ---- object / array navigation, one call per query ------------------- */

/* Parse the document and look up one key, returning the raw value length.
 * Mirrors the per-call navigation API of Vela's tiers. */
long long vj_yy_object_get_len(const char *dat, size_t len, const char *key) {
    yyjson_doc *doc = yyjson_read(dat, len, 0);
    if (!doc) return -1;
    long long out = -1;
    yyjson_val *v = yyjson_obj_get(yyjson_doc_get_root(doc), key);
    if (v) out = (long long)yyjson_get_sint(v);
    yyjson_doc_free(doc);
    return out;
}

size_t vj_yy_array_count(const char *dat, size_t len) {
    yyjson_doc *doc = yyjson_read(dat, len, 0);
    if (!doc) return 0;
    size_t n = yyjson_arr_size(yyjson_doc_get_root(doc));
    yyjson_doc_free(doc);
    return n;
}

/* ---- validate -------------------------------------------------------- */

int vj_yy_validate(const char *dat, size_t len) {
    yyjson_doc *doc = yyjson_read(dat, len, 0);
    if (!doc) return 0;
    yyjson_doc_free(doc);
    return 1;
}

/* ---- serialize ------------------------------------------------------- */

/* Parse then re-serialize, returning the output length. Mirrors the
 * round-trip the Rust serializer benchmark performs. */
size_t vj_yy_roundtrip(const char *dat, size_t len) {
    yyjson_doc *doc = yyjson_read(dat, len, 0);
    if (!doc) return 0;
    size_t out_len = 0;
    char *out = yyjson_write(doc, 0, &out_len);
    yyjson_doc_free(doc);
    if (out) free(out);
    return out_len;
}

/* ---- version --------------------------------------------------------- */

const char *vj_yy_version(void) { return YYJSON_VERSION_STRING; }
