// 64-bit integer helpers that 32-bit x86 code calls for multiply, divide and
// shift (the C runtime normally provides them). Only built for x86, without
// /GL, and written using 32-bit operations only so they never call themselves.
#ifdef _M_IX86

#include <intrin.h>

typedef unsigned int u32;
typedef unsigned long long u64;
typedef long long i64;

// Long division on 32-bit halves. Constant shifts by 32 compile inline.
static u64 udivmod(u64 n, u64 d, u64 *rem) {
    u32 nh = (u32)(n >> 32), nl = (u32)n, dh = (u32)(d >> 32), dl = (u32)d;
    u32 qh = 0, ql = 0, rh = 0, rl = 0;
    for (int i = 63; i >= 0; i--) {
        u32 bit = i >= 32 ? (nh >> (i - 32)) & 1 : (nl >> i) & 1;
        rh = rh << 1 | rl >> 31;
        rl = rl << 1 | bit;
        if (rh > dh || (rh == dh && rl >= dl)) {
            u32 borrow = rl < dl;
            rl -= dl;
            rh -= dh + borrow;
            if (i >= 32) qh |= 1u << (i - 32);
            else ql |= 1u << i;
        }
    }
    if (rem) *rem = (u64)rh << 32 | rl;
    return (u64)qh << 32 | ql;
}

static u64 neg(u64 v) {
    u32 lo = (u32)v, hi = (u32)(v >> 32);
    lo = ~lo + 1;
    hi = ~hi + (lo == 0);
    return (u64)hi << 32 | lo;
}

static i64 sdivmod(i64 a, i64 b, i64 *rem) {
    int na = (u32)((u64)a >> 32) >> 31, nb = (u32)((u64)b >> 32) >> 31;
    u64 r, q = udivmod(na ? neg((u64)a) : (u64)a, nb ? neg((u64)b) : (u64)b, &r);
    // The remainder takes the dividend's sign, the quotient the product's.
    if (rem) *rem = (i64)(na ? neg(r) : r);
    return (i64)(na != nb ? neg(q) : q);
}

// Stack arguments, result in EDX:EAX, callee pops: exactly __stdcall. The
// names the compiler expects are jump stubs at the end of the file.
u64 __stdcall b_aulldiv(u64 a, u64 b) { return udivmod(a, b, 0); }
u64 __stdcall b_aullrem(u64 a, u64 b) { u64 r; udivmod(a, b, &r); return r; }
i64 __stdcall b_alldiv(i64 a, i64 b) { return sdivmod(a, b, 0); }
i64 __stdcall b_allrem(i64 a, i64 b) { i64 r; sdivmod(a, b, &r); return r; }
u64 __stdcall b_aulldvrm_impl(u64 a, u64 b, u64 *rem) { return udivmod(a, b, rem); }
i64 __stdcall b_alldvrm_impl(i64 a, i64 b, i64 *rem) { return sdivmod(a, b, rem); }

u64 __stdcall b_allmul(u64 a, u64 b) {
    u32 al = (u32)a, ah = (u32)(a >> 32), bl = (u32)b, bh = (u32)(b >> 32);
    u64 lo = __emulu(al, bl);
    u32 hi = (u32)(lo >> 32) + al * bh + ah * bl;
    return (u64)hi << 32 | (u32)lo;
}


// Divide-with-remainder: quotient in EDX:EAX, remainder in EBX:ECX.
#define DVRM(name, impl)                                                                                               \
    __declspec(naked) void name(void) {                                                                                \
        __asm sub esp, 8 /* remainder slot */                                                                          \
        __asm mov eax, esp                                                                                             \
        __asm push eax                                                                                                 \
        __asm push dword ptr [esp + 28] /* b hi */                                                                     \
        __asm push dword ptr [esp + 28] /* b lo */                                                                     \
        __asm push dword ptr [esp + 28] /* a hi */                                                                     \
        __asm push dword ptr [esp + 28] /* a lo */                                                                     \
        __asm call impl                                                                                                \
        __asm mov ecx, [esp]                                                                                           \
        __asm mov ebx, [esp + 4]                                                                                       \
        __asm add esp, 8                                                                                               \
        __asm ret 16                                                                                                   \
    }
DVRM(_aulldvrm, b_aulldvrm_impl)
DVRM(_alldvrm, b_alldvrm_impl)

// Shifts: value in EDX:EAX, count in CL.
__declspec(naked) void _allshl(void) {
    __asm {
        cmp cl, 64
        jae zero
        cmp cl, 32
        jae big
        shld edx, eax, cl
        shl eax, cl
        ret
    big:
        mov edx, eax
        xor eax, eax
        and cl, 31
        shl edx, cl
        ret
    zero:
        xor eax, eax
        xor edx, edx
        ret
    }
}

__declspec(naked) void _aullshr(void) {
    __asm {
        cmp cl, 64
        jae zero
        cmp cl, 32
        jae big
        shrd eax, edx, cl
        shr edx, cl
        ret
    big:
        mov eax, edx
        xor edx, edx
        and cl, 31
        shr eax, cl
        ret
    zero:
        xor eax, eax
        xor edx, edx
        ret
    }
}

__declspec(naked) void _allshr(void) {
    __asm {
        cmp cl, 64
        jae fill
        cmp cl, 32
        jae big
        shrd eax, edx, cl
        sar edx, cl
        ret
    big:
        mov eax, edx
        sar edx, 31
        and cl, 31
        sar eax, cl
        ret
    fill:
        sar edx, 31
        mov eax, edx
        ret
    }
}

// Float to 64-bit integer, truncating: value in ST(0), result in EDX:EAX.
// fisttp (SSE3) truncates without touching the FPU control word.
__declspec(naked) void _ftol2(void) {
    __asm {
        sub esp, 8
        fisttp qword ptr [esp]
        mov eax, [esp]
        mov edx, [esp + 4]
        add esp, 8
        ret
    }
}

__declspec(naked) void _ftol2_sse(void) {
    __asm jmp _ftol2
}

// The names the compiler calls, jumping to the __stdcall versions above (same
// stack layout, so their `ret 16` returns straight to the caller).
#define STUB(name, impl)     __declspec(naked) void name(void) { __asm jmp impl }
STUB(_aulldiv, b_aulldiv)
STUB(_aullrem, b_aullrem)
STUB(_alldiv, b_alldiv)
STUB(_allrem, b_allrem)
STUB(_allmul, b_allmul)

#endif
