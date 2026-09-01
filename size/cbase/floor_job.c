/* The C floor: the shim and nothing else, so the C contenders have the
 * same thing subtracted from them that the Rust ones do. */

#include <stddef.h>
#include <stdint.h>

extern const unsigned char *volatile INPUT_P;
extern const size_t INPUT_LEN;
void finish(int64_t total);

void _start(void) {
    finish((int64_t)INPUT_LEN + (INPUT_P ? 0 : 1));
}
