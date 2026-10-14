// Minimal runtime for a program linked without the C runtime: memory,
// strings, formatting, files, logging and the shared clock.
#pragma once

#define WIN32_LEAN_AND_MEAN
#define COBJMACROS
#define CINTERFACE
#include <windows.h>
#include <stdarg.h>

#define SECOND 10000000LL // the shared clock counts 100ns units

#define MIN(a, b) ((a) < (b) ? (a) : (b))
#define MAX(a, b) ((a) > (b) ? (a) : (b))
#define CLAMP(v, lo, hi) MIN(MAX(v, lo), hi)
#define COUNTOF(a) (sizeof(a) / sizeof((a)[0]))
#define RELEASE(p) do { if (p) { (p)->lpVtbl->Release(p); (p) = NULL; } } while (0)

// Memory. Allocations are zeroed.
void *mem_alloc(SIZE_T size);
void *mem_realloc(void *p, SIZE_T size);
void mem_free(void *p);

// Growable byte buffer.
typedef struct {
    BYTE *data;
    SIZE_T len, cap;
} Buf;
void buf_push(Buf *b, const void *data, SIZE_T len);
void buf_u8(Buf *b, BYTE v);
void buf_u16be(Buf *b, UINT16 v);
void buf_u32be(Buf *b, UINT32 v);
void buf_u64be(Buf *b, UINT64 v);
void buf_zeros(Buf *b, SIZE_T n);
void buf_free(Buf *b);

// Wide strings.
int wlen(const WCHAR *s);
WCHAR *wdup(const WCHAR *s);
BOOL weq(const WCHAR *a, const WCHAR *b);
BOOL weqa(const WCHAR *a, const char *b); // compare with an ASCII string
BOOL wcontains(const WCHAR *haystack, const WCHAR *needle);
void wcopy(WCHAR *dst, int cap, const WCHAR *src);
UINT32 wparse_u32(const WCHAR *s, UINT32 fallback); // fallback if not all digits
void wtrim(WCHAR *s);

// printf-style formatting into wide buffers, with an ASCII format string
// (half the size of a wide one). Supports %s (wide), %S (UTF-8),
// %d %u %lld %llu %x, a zero-pad flag and width (%02u), %h (an HRESULT as
// "message (0x80070057)") and %%.
int wfmtv(WCHAR *out, int cap, const char *fmt, va_list ap);
int wfmt(WCHAR *out, int cap, const char *fmt, ...);
// Same, returning a heap string.
WCHAR *wfmt_alloc(const char *fmt, ...);

// "message (0x80070057)" for an HRESULT.
void hr_text(HRESULT hr, WCHAR *out, int cap);

// UTF-8 <-> UTF-16 (heap results).
char *to_utf8(const WCHAR *s, int *out_len);
WCHAR *from_utf8(const char *s, int len);

// Files and paths.
void *file_read_all(const WCHAR *path, DWORD *len);
BOOL file_write_all(const WCHAR *path, const void *data, DWORD len);
BOOL file_exists(const WCHAR *path);
UINT64 file_size(const WCHAR *path); // 0 if missing
BOOL dir_create_all(const WCHAR *path);
void path_join(WCHAR *out, int cap, const WCHAR *dir, const WCHAR *name);
const WCHAR *path_file_name(const WCHAR *path);

// Buffered file writer.
typedef struct {
    HANDLE file;
    BYTE *buf;
    DWORD len, cap;
    BOOL failed;
} Writer;
BOOL writer_open(Writer *w, const WCHAR *path, DWORD cap);
void writer_put(Writer *w, const void *data, SIZE_T len);
BOOL writer_close(Writer *w); // flushes; FALSE if anything failed

// Logging to %APPDATA%\bufferless\bufferless.log.
void log_init(const WCHAR *path);
void logmsg(const char *fmt, ...);

// Shared QPC clock, 100ns units (the timeline WASAPI reports positions on).
INT64 clock_now(void);

// Float helpers (the C runtime normally provides these).
float m_sin(float x);
float m_round(float x); // for non-negative values
