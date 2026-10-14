#include "rt.h"

// ---------------------------------------------------------------------------
// Memory

void *mem_alloc(SIZE_T size) {
    return HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, size ? size : 1);
}

void *mem_realloc(void *p, SIZE_T size) {
    if (!p) return mem_alloc(size);
    return HeapReAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, p, size ? size : 1);
}

void mem_free(void *p) {
    if (p) HeapFree(GetProcessHeap(), 0, p);
}

void buf_push(Buf *b, const void *data, SIZE_T len) {
    if (b->len + len > b->cap) {
        SIZE_T cap = MAX(b->cap * 2, b->len + len);
        b->data = mem_realloc(b->data, MAX(cap, 64));
        b->cap = MAX(cap, 64);
    }
    if (data) memcpy(b->data + b->len, data, len);
    else memset(b->data + b->len, 0, len);
    b->len += len;
}
void buf_zeros(Buf *b, SIZE_T n) { buf_push(b, NULL, n); }
void buf_u8(Buf *b, BYTE v) { buf_push(b, &v, 1); }
void buf_u16be(Buf *b, UINT16 v) {
    BYTE x[2] = {(BYTE)(v >> 8), (BYTE)v};
    buf_push(b, x, 2);
}
void buf_u32be(Buf *b, UINT32 v) {
    BYTE x[4] = {(BYTE)(v >> 24), (BYTE)(v >> 16), (BYTE)(v >> 8), (BYTE)v};
    buf_push(b, x, 4);
}
void buf_u64be(Buf *b, UINT64 v) {
    buf_u32be(b, (UINT32)(v >> 32));
    buf_u32be(b, (UINT32)v);
}
void buf_free(Buf *b) {
    mem_free(b->data);
    b->data = NULL;
    b->len = b->cap = 0;
}

// ---------------------------------------------------------------------------
// Strings

int wlen(const WCHAR *s) {
    int n = 0;
    while (s && s[n]) n++;
    return n;
}

WCHAR *wdup(const WCHAR *s) {
    int n = wlen(s);
    WCHAR *d = mem_alloc((n + 1) * sizeof(WCHAR));
    memcpy(d, s, n * sizeof(WCHAR));
    return d;
}

BOOL weq(const WCHAR *a, const WCHAR *b) {
    a = a ? a : L"";
    b = b ? b : L"";
    while (*a && *a == *b) a++, b++;
    return *a == *b;
}

BOOL weqa(const WCHAR *a, const char *b) {
    while (*a && *a == (WCHAR)(unsigned char)*b) a++, b++;
    return *a == (WCHAR)(unsigned char)*b;
}

BOOL wcontains(const WCHAR *h, const WCHAR *n) {
    int nl = wlen(n);
    for (; h && *h; h++) {
        int i = 0;
        while (i < nl && h[i] == n[i]) i++;
        if (i == nl) return TRUE;
    }
    return FALSE;
}

void wcopy(WCHAR *dst, int cap, const WCHAR *src) {
    int i = 0;
    for (; src && src[i] && i < cap - 1; i++) dst[i] = src[i];
    dst[i] = 0;
}

UINT32 wparse_u32(const WCHAR *s, UINT32 fallback) {
    UINT64 v = 0;
    if (!s || !*s) return fallback;
    for (; *s; s++) {
        if (*s < '0' || *s > '9') return fallback;
        v = v * 10 + (*s - '0');
        if (v > 0xffffffff) return fallback;
    }
    return (UINT32)v;
}

void wtrim(WCHAR *s) {
    int n = wlen(s), start = 0;
    while (n > 0 && (s[n - 1] == ' ' || s[n - 1] == '\t')) n--;
    while (start < n && (s[start] == ' ' || s[start] == '\t')) start++;
    memmove(s, s + start, (n - start) * sizeof(WCHAR));
    s[n - start] = 0;
}

// ---------------------------------------------------------------------------
// Formatting

typedef struct {
    WCHAR *out;
    int cap, len;
} Fmt;

static void put(Fmt *f, WCHAR c) {
    if (f->len < f->cap - 1) f->out[f->len] = c;
    f->len++;
}

static void put_u64(Fmt *f, UINT64 v, int width, BOOL zero, int base) {
    WCHAR digits[24];
    int n = 0;
    do {
        int d = (int)(v % base);
        digits[n++] = (WCHAR)(d < 10 ? '0' + d : 'a' + d - 10);
        v /= base;
    } while (v);
    for (; width > n; width--) put(f, zero ? '0' : ' ');
    while (n) put(f, digits[--n]);
}

int wfmtv(WCHAR *out, int cap, const char *fmt, va_list ap) {
    Fmt f = {out, cap, 0};
    for (; *fmt; fmt++) {
        if (*fmt != '%') {
            put(&f, (WCHAR)(unsigned char)*fmt);
            continue;
        }
        fmt++;
        BOOL zero = *fmt == '0';
        int width = 0, longs = 0;
        while (*fmt >= '0' && *fmt <= '9') width = width * 10 + (*fmt++ - '0');
        while (*fmt == 'l') longs++, fmt++;
        switch (*fmt) {
        case 's': {
            const WCHAR *s = va_arg(ap, const WCHAR *);
            for (s = s ? s : L""; *s; s++) put(&f, *s);
            break;
        }
        case 'S': { // UTF-8, treated as ASCII-compatible text
            const char *s = va_arg(ap, const char *);
            for (s = s ? s : ""; *s; s++) put(&f, (WCHAR)(unsigned char)*s);
            break;
        }
        case 'd': {
            INT64 v = longs >= 2 ? va_arg(ap, INT64) : va_arg(ap, int);
            if (v < 0) put(&f, '-'), v = -v;
            put_u64(&f, (UINT64)v, width, zero, 10);
            break;
        }
        case 'u': put_u64(&f, longs >= 2 ? va_arg(ap, UINT64) : va_arg(ap, unsigned), width, zero, 10); break;
        case 'x': put_u64(&f, longs >= 2 ? va_arg(ap, UINT64) : va_arg(ap, unsigned), width, zero, 16); break;
        case 'h': { // HRESULT as "message (0x80070057)"
            WCHAR msg[300];
            hr_text(va_arg(ap, HRESULT), msg, COUNTOF(msg));
            for (const WCHAR *s = msg; *s; s++) put(&f, *s);
            break;
        }
        case '%': put(&f, '%'); break;
        default: break;
        }
    }
    if (cap > 0) out[MIN(f.len, cap - 1)] = 0;
    return f.len;
}

int wfmt(WCHAR *out, int cap, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = wfmtv(out, cap, fmt, ap);
    va_end(ap);
    return n;
}

WCHAR *wfmt_alloc(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    WCHAR probe[1];
    int n = wfmtv(probe, 1, fmt, ap);
    va_end(ap);
    WCHAR *out = mem_alloc((n + 1) * sizeof(WCHAR));
    va_start(ap, fmt);
    wfmtv(out, n + 1, fmt, ap);
    va_end(ap);
    return out;
}

void hr_text(HRESULT hr, WCHAR *out, int cap) {
    WCHAR msg[256];
    DWORD n = FormatMessageW(FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS, NULL, hr, 0, msg,
                             COUNTOF(msg), NULL);
    while (n && (msg[n - 1] == '\r' || msg[n - 1] == '\n' || msg[n - 1] == ' ')) n--;
    msg[n] = 0;
    wfmt(out, cap, n ? "%s (0x%08x)" : "%s0x%08x", msg, (unsigned)hr);
}

char *to_utf8(const WCHAR *s, int *out_len) {
    int n = WideCharToMultiByte(CP_UTF8, 0, s, -1, NULL, 0, NULL, NULL);
    char *out = mem_alloc(n + 1);
    WideCharToMultiByte(CP_UTF8, 0, s, -1, out, n, NULL, NULL);
    if (out_len) *out_len = n > 0 ? n - 1 : 0;
    return out;
}

WCHAR *from_utf8(const char *s, int len) {
    int n = MultiByteToWideChar(CP_UTF8, 0, s, len, NULL, 0);
    WCHAR *out = mem_alloc((n + 1) * sizeof(WCHAR));
    MultiByteToWideChar(CP_UTF8, 0, s, len, out, n);
    return out;
}

// ---------------------------------------------------------------------------
// Files

void *file_read_all(const WCHAR *path, DWORD *len) {
    HANDLE f = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_EXISTING, 0, NULL);
    if (f == INVALID_HANDLE_VALUE) return NULL;
    DWORD size = GetFileSize(f, NULL), got = 0;
    BYTE *data = mem_alloc(size + 1);
    if (!ReadFile(f, data, size, &got, NULL)) got = 0;
    CloseHandle(f);
    *len = got;
    return data;
}

BOOL file_write_all(const WCHAR *path, const void *data, DWORD len) {
    HANDLE f = CreateFileW(path, GENERIC_WRITE, FILE_SHARE_READ, NULL, CREATE_ALWAYS, 0, NULL);
    if (f == INVALID_HANDLE_VALUE) return FALSE;
    DWORD done = 0;
    BOOL ok = WriteFile(f, data, len, &done, NULL) && done == len;
    CloseHandle(f);
    return ok;
}

BOOL file_exists(const WCHAR *path) {
    return GetFileAttributesW(path) != INVALID_FILE_ATTRIBUTES;
}

UINT64 file_size(const WCHAR *path) {
    WIN32_FILE_ATTRIBUTE_DATA a;
    if (!GetFileAttributesExW(path, GetFileExInfoStandard, &a)) return 0;
    return (UINT64)a.nFileSizeHigh << 32 | a.nFileSizeLow;
}

BOOL dir_create_all(const WCHAR *path) {
    WCHAR prefix[MAX_PATH * 2];
    int n = wlen(path);
    if (n >= (int)COUNTOF(prefix)) return FALSE;
    // Create each prefix in turn; existing ones fail harmlessly.
    for (int i = 0; i <= n; i++) {
        if (i == n || path[i] == '\\' || path[i] == '/') {
            memcpy(prefix, path, i * sizeof(WCHAR));
            prefix[i] = 0;
            if (i > 0 && prefix[i - 1] != ':') CreateDirectoryW(prefix, NULL);
        }
    }
    return file_exists(path);
}

void path_join(WCHAR *out, int cap, const WCHAR *dir, const WCHAR *name) {
    wcopy(out, cap, dir);
    int n = wlen(out);
    while (n > 0 && (out[n - 1] == '\\' || out[n - 1] == '/')) out[--n] = 0;
    wfmt(out + n, cap - n, "\\%s", name);
}

const WCHAR *path_file_name(const WCHAR *path) {
    const WCHAR *name = path;
    for (; *path; path++)
        if (*path == '\\' || *path == '/') name = path + 1;
    return name;
}

BOOL writer_open(Writer *w, const WCHAR *path, DWORD cap) {
    memset(w, 0, sizeof(*w));
    w->file = CreateFileW(path, GENERIC_WRITE, FILE_SHARE_READ, NULL, CREATE_ALWAYS, 0, NULL);
    if (w->file == INVALID_HANDLE_VALUE) return FALSE;
    w->buf = mem_alloc(cap);
    w->cap = cap;
    return TRUE;
}

static void writer_flush(Writer *w) {
    DWORD done;
    if (w->len && (!WriteFile(w->file, w->buf, w->len, &done, NULL) || done != w->len)) w->failed = TRUE;
    w->len = 0;
}

void writer_put(Writer *w, const void *data, SIZE_T len) {
    const BYTE *p = data;
    while (len) {
        if (w->len == w->cap) writer_flush(w);
        DWORD n = (DWORD)MIN(len, w->cap - w->len);
        memcpy(w->buf + w->len, p, n);
        w->len += n, p += n, len -= n;
    }
}

BOOL writer_close(Writer *w) {
    writer_flush(w);
    CloseHandle(w->file);
    mem_free(w->buf);
    return !w->failed;
}

// ---------------------------------------------------------------------------
// Logging

static HANDLE g_log = INVALID_HANDLE_VALUE;
static SRWLOCK g_log_lock = SRWLOCK_INIT;

void log_init(const WCHAR *path) {
    // Keep the log small: start fresh if it grew past 1 MB. Shared writing so
    // two copies starting at once don't lock each other out of the log.
    BOOL fresh = file_size(path) > (1 << 20);
    g_log = CreateFileW(path, fresh ? GENERIC_WRITE : FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL,
                        fresh ? CREATE_ALWAYS : OPEN_ALWAYS, 0, NULL);
}

void logmsg(const char *fmt, ...) {
    if (g_log == INVALID_HANDLE_VALUE) return;
    WCHAR line[1024];
    SYSTEMTIME t;
    GetLocalTime(&t);
    int n = wfmt(line, COUNTOF(line), "%04u-%02u-%02u %02u:%02u:%02u.%03u ", t.wYear, t.wMonth, t.wDay, t.wHour,
                 t.wMinute, t.wSecond, t.wMilliseconds);
    va_list ap;
    va_start(ap, fmt);
    n += wfmtv(line + n, COUNTOF(line) - n - 1, fmt, ap);
    va_end(ap);
    n = MIN(n, (int)COUNTOF(line) - 2);
    line[n] = '\n', line[n + 1] = 0;
    int len;
    char *utf8 = to_utf8(line, &len);
    AcquireSRWLockExclusive(&g_log_lock);
    DWORD done;
    WriteFile(g_log, utf8, len, &done, NULL);
    ReleaseSRWLockExclusive(&g_log_lock);
    mem_free(utf8);
}

// ---------------------------------------------------------------------------
// Clock and math

INT64 clock_now(void) {
    static INT64 freq;
    LARGE_INTEGER c;
    if (!freq) {
        LARGE_INTEGER f;
        QueryPerformanceFrequency(&f);
        freq = f.QuadPart;
    }
    QueryPerformanceCounter(&c);
    // Split to avoid overflowing 64 bits.
    return c.QuadPart / freq * SECOND + c.QuadPart % freq * SECOND / freq;
}

float m_round(float x) {
    return (float)(int)(x + 0.5f);
}

float m_sin(float x) {
    const float pi = 3.14159265f, tau = 6.28318531f;
    // Reduce to [-pi, pi], then a Taylor series is accurate to ~1e-6.
    float k = x / tau;
    x -= tau * (float)(int)(k >= 0 ? k + 0.5f : k - 0.5f);
    x = CLAMP(x, -pi, pi);
    float x2 = x * x;
    return x * (1 - x2 / 6 * (1 - x2 / 20 * (1 - x2 / 42 * (1 - x2 / 72 * (1 - x2 / 110)))));
}
