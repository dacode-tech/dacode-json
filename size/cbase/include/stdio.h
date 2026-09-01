#ifndef _FREESTANDING_STDIO_H
#define _FREESTANDING_STDIO_H
#include <stddef.h>
#include <stdarg.h>
typedef struct _FILE FILE;
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
FILE *fopen(const char *, const char *);
int fclose(FILE *);
size_t fread(void *, size_t, size_t, FILE *);
size_t fwrite(const void *, size_t, size_t, FILE *);
int fseek(FILE *, long, int);
long ftell(FILE *);
int snprintf(char *, size_t, const char *, ...);
int sprintf(char *, const char *, ...);
#endif
