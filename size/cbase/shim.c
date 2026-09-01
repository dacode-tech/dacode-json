/* The bare-metal shim for the C contenders — the counterpart of
 * `size/src/lib.rs`.
 *
 * Same input, same "publish the answer and stop", same bump allocator, so
 * that the only difference between a C binary and a Rust one is the JSON
 * library. `--gc-sections` drops everything the job does not reach, which
 * is what makes this comparable to a linked Rust binary rather than to a
 * whole-library `.o`.
 */

#include <stddef.h>
#include <stdint.h>

/* Kept byte-identical to `size::INPUT`. */
const unsigned char INPUT[] =
    "[\n"
    "{\"id\":1,\"name\":\"alpha\",\"score\":11,\"tags\":[\"a\",\"b\"],\"ok\":true,"
    "\"ratio\":1.5,\"note\":null},\n"
    "{\"id\":2,\"name\":\"bravo\",\"score\":22,\"tags\":[\"c\"],\"ok\":false,"
    "\"ratio\":2.25,\"note\":\"x\"},\n"
    "{\"id\":3,\"name\":\"charlie\",\"score\":33,\"tags\":[],\"ok\":true,"
    "\"ratio\":0.125,\"note\":null}\n"
    "]";

const unsigned char *volatile INPUT_P = INPUT;
const size_t INPUT_LEN = sizeof(INPUT) - 1;

volatile int64_t ANSWER = 0;

void finish(int64_t total) {
    ANSWER = total;
    for (;;) __asm__ volatile("wfi");
}

/* --- the same bump allocator the Rust side uses -------------------- */

#define ARENA (64 * 1024)
static unsigned char heap[ARENA];
static size_t next_off = 0;

void *malloc(size_t n) {
    size_t start = (next_off + 7u) & ~(size_t)7u;
    if (start + n > ARENA) return NULL;
    next_off = start + n;
    return heap + start;
}

void free(void *p) { (void)p; }

void *realloc(void *p, size_t n) {
    /* Never shrinks and never reuses, as the Rust bump allocator does not.
     * yyjson's reader only grows its value pool. */
    void *q = malloc(n);
    if (!q || !p) return q;
    unsigned char *a = q, *b = p;
    for (size_t i = 0; i < n; i++) a[i] = b[i];
    return q;
}

/* --- the `mem*` routines, since there is no libc ------------------- */
/*
 * `-fno-builtin` is deliberately NOT passed: letting the compiler lower
 * small fixed-size copies inline is what a real build does, and yyjson is
 * written expecting it. These are the out-of-line fallbacks.
 */

void *memcpy(void *d, const void *s, size_t n) {
    unsigned char *a = d;
    const unsigned char *b = s;
    while (n--) *a++ = *b++;
    return d;
}

void *memmove(void *d, const void *s, size_t n) {
    unsigned char *a = d;
    const unsigned char *b = s;
    if (a < b) {
        while (n--) *a++ = *b++;
    } else {
        a += n;
        b += n;
        while (n--) *--a = *--b;
    }
    return d;
}

void *memset(void *d, int c, size_t n) {
    unsigned char *a = d;
    while (n--) *a++ = (unsigned char)c;
    return d;
}

int memcmp(const void *x, const void *y, size_t n) {
    const unsigned char *a = x, *b = y;
    while (n--) {
        if (*a != *b) return (int)*a - (int)*b;
        a++;
        b++;
    }
    return 0;
}

size_t strlen(const char *s) {
    const char *p = s;
    while (*p) p++;
    return (size_t)(p - s);
}
