#ifndef _FREESTANDING_MATH_H
#define _FREESTANDING_MATH_H
/* Clang's own <__float_infinity_nan.h> already defines INFINITY and NAN
 * for a freestanding target, so only fill in what is missing. */
#include <__float_infinity_nan.h>
#ifndef HUGE_VAL
#define HUGE_VAL __builtin_huge_val()
#endif
double floor(double);
double pow(double, double);
double fabs(double);
#endif
