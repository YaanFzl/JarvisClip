// What the compiler expects the C runtime to provide. Compiled without /GL:
// MSVC can't do whole-program optimization on its own library helpers.
#include <intrin.h>
#include <stddef.h>
#include <string.h> // declarations only, for #pragma function


int _fltused = 0; // referenced by any code that uses floating point

#pragma function(memset, memcpy, memcmp)

void *memset(void *dst, int c, size_t n) {
    __stosb((unsigned char *)dst, (unsigned char)c, n);
    return dst;
}

void *memcpy(void *dst, const void *src, size_t n) {
    __movsb((unsigned char *)dst, (const unsigned char *)src, n);
    return dst;
}

void *memmove(void *dst, const void *src, size_t n) {
    unsigned char *d = dst;
    const unsigned char *s = src;
    if (d <= s || d >= s + n) {
        __movsb(d, s, n);
    } else {
        // Overlapping with dst after src: copy backwards. volatile keeps the
        // compiler from turning this loop back into a memmove call.
        volatile unsigned char *vd = d;
        while (n--) vd[n] = s[n];
    }
    return dst;
}

int memcmp(const void *a, const void *b, size_t n) {
    const unsigned char *x = a, *y = b;
    for (size_t i = 0; i < n; i++)
        if (x[i] != y[i]) return x[i] - y[i];
    return 0;
}

