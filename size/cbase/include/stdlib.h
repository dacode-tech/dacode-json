#ifndef _FREESTANDING_STDLIB_H
#define _FREESTANDING_STDLIB_H
#include <stddef.h>
void *malloc(size_t);
void *realloc(void *, size_t);
void free(void *);
double strtod(const char *, char **);
void abort(void);
#endif
