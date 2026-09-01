#ifndef _FREESTANDING_LOCALE_H
#define _FREESTANDING_LOCALE_H
#define LC_NUMERIC 4
char *setlocale(int, const char *);
struct lconv { char *decimal_point; };
struct lconv *localeconv(void);
#endif
