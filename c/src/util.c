#include "util.h"

#include <mmsystem.h>
#include <psapi.h>

// ---------------------------------------------------------------------------
// Icons. Geometry is in a 64x64 space, same as the Rust build's icon.rs.

// (center x, radius, opacity) for each circle, back to front.
static const float CIRCLES[3][3] = {{21.0f, 9.0f, 0.16f}, {29.0f, 10.2f, 0.38f}, {38.5f, 11.5f, 1.0f}};
#define CIRCLE_Y 32.0f
#define TILE_RADIUS 15.0f
#define SUPERSAMPLE 4

typedef struct {
    float r, g, b, a; // premultiplied
} Color;

static Color over(Color dst, float r, float g, float b, float alpha) {
    float k = 1 - alpha;
    return (Color){r * alpha + dst.r * k, g * alpha + dst.g * k, b * alpha + dst.b * k, alpha + dst.a * k};
}

static BOOL in_rounded_rect(float u, float v, float inset) {
    float half = 32 - inset, r = TILE_RADIUS - inset;
    float dx = MAX((u > 32 ? u - 32 : 32 - u) - (half - r), 0), dy = MAX((v > 32 ? v - 32 : 32 - v) - (half - r), 0);
    return dx * dx + dy * dy <= r * r;
}

static Color circles(Color c, float u, float v, float r, float g, float b) {
    for (int i = 0; i < 3; i++) {
        float dx = u - CIRCLES[i][0], dy = v - CIRCLE_Y;
        if (dx * dx + dy * dy <= CIRCLES[i][1] * CIRCLES[i][1]) c = over(c, r, g, b, CIRCLES[i][2]);
    }
    return c;
}

typedef struct {
    BOOL tile;
    float r, g, b, opacity;
} Scene;

static Color sample(const Scene *s, float u, float v) {
    if (!s->tile) {
        Color c = circles((Color){0}, u, v, s->r, s->g, s->b);
        c.r *= s->opacity, c.g *= s->opacity, c.b *= s->opacity, c.a *= s->opacity;
        return c;
    }
    if (!in_rounded_rect(u, v, 0)) return (Color){0};
    float t = CLAMP((u + v) / 128, 0, 1);
    // Indigo gradient from #6a5ae0 (top left) to #1e1a4a (bottom right).
    Color c = over((Color){0}, (0x6a + (0x1e - 0x6a) * t) / 255, (0x5a + (0x1a - 0x5a) * t) / 255,
                   (0xe0 + (0x4a - 0xe0) * t) / 255, 1);
    // Faint inner edge highlight, like light catching the tile's rim.
    if (!in_rounded_rect(u, v, 1)) c = over(c, 1, 1, 1, 0.14f);
    return circles(c, u, v, 1, 1, 1);
}

static HICON render(int size, const Scene *s, float origin, float span) {
    BITMAPINFO bmi = {{sizeof(BITMAPINFOHEADER), size, -size, 1, 32, BI_RGB}};
    BYTE *bits = NULL;
    HBITMAP color = CreateDIBSection(NULL, &bmi, DIB_RGB_COLORS, (void **)&bits, NULL, 0);
    if (!color) return LoadIconW(NULL, IDI_APPLICATION);
    const float n = SUPERSAMPLE * SUPERSAMPLE;
    for (int y = 0; y < size; y++)
        for (int x = 0; x < size; x++) {
            Color acc = {0};
            for (int sy = 0; sy < SUPERSAMPLE; sy++)
                for (int sx = 0; sx < SUPERSAMPLE; sx++) {
                    float fx = x + (sx + 0.5f) / SUPERSAMPLE, fy = y + (sy + 0.5f) / SUPERSAMPLE;
                    Color c = sample(s, origin + fx / size * span, origin + fy / size * span);
                    acc.r += c.r, acc.g += c.g, acc.b += c.b, acc.a += c.a;
                }
            float a = acc.a / n;
            BYTE *p = bits + (y * size + x) * 4;
            // DIB is BGRA with straight alpha.
            p[0] = a > 0 ? (BYTE)CLAMP(m_round(acc.b / n / a * 255), 0, 255) : 0;
            p[1] = a > 0 ? (BYTE)CLAMP(m_round(acc.g / n / a * 255), 0, 255) : 0;
            p[2] = a > 0 ? (BYTE)CLAMP(m_round(acc.r / n / a * 255), 0, 255) : 0;
            p[3] = (BYTE)m_round(a * 255);
        }
    HBITMAP mask = CreateBitmap(size, size, 1, 1, NULL);
    ICONINFO info = {TRUE, 0, 0, mask, color};
    HICON icon = CreateIconIndirect(&info);
    DeleteObject(color);
    DeleteObject(mask);
    return icon;
}

HICON icon_tile(int size) {
    Scene s = {TRUE};
    return render(MAX(size, 16), &s, 0, 64);
}

HICON icon_glyph(int size, COLORREF rgb, float opacity) {
    Scene s = {FALSE, GetRValue(rgb) / 255.0f, GetGValue(rgb) / 255.0f, GetBValue(rgb) / 255.0f, opacity};
    // Cropped tight around the mark.
    return render(MAX(size, 16), &s, 8, 48);
}

// ---------------------------------------------------------------------------
// Sounds

// Short two-note blip, synthesized once and played from memory.
void play_saved_sound(void) {
    static BYTE *wav;
    if (!wav) {
        const UINT32 rate = 44100, notes[2][2] = {{880, 70}, {1320, 90}};
        UINT32 samples = rate * 70 / 1000 + rate * 90 / 1000, data_len = samples * 2;
        BYTE *w = mem_alloc(44 + data_len);
        INT16 *pcm = (INT16 *)(w + 44);
        UINT32 o = 0;
        for (int k = 0; k < 2; k++) {
            UINT32 n = rate * notes[k][1] / 1000;
            for (UINT32 i = 0; i < n; i++) {
                float t = (float)i / rate;
                // Quick fade in/out so it doesn't click.
                float env = MIN((float)MIN(i, n - i) / (rate * 0.008f), 1.0f);
                pcm[o++] = (INT16)(m_sin(t * notes[k][0] * 6.28318531f) * env * 0.25f * 32767);
            }
        }
        UINT32 hdr[11] = {'FFIR', 36 + data_len, 'EVAW', ' tmf', 16, 1 | 1 << 16, rate, rate * 2, 2 | 16 << 16,
                          'atad', data_len};
        memcpy(w, hdr, 44);
        wav = w;
    }
    PlaySoundW((LPCWSTR)wav, NULL, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
}

void play_error_sound(void) {
    PlaySoundW(L"SystemHand", NULL, SND_ALIAS | SND_ASYNC);
}

// ---------------------------------------------------------------------------

void timestamp(WCHAR *out, int cap) {
    SYSTEMTIME t;
    GetLocalTime(&t);
    wfmt(out, cap, "%04u-%02u-%02u %02u-%02u-%02u", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond);
}

void foreground_app_name(WCHAR *out, int cap) {
    DWORD pid = 0, len;
    WCHAR path[1024];
    GetWindowThreadProcessId(GetForegroundWindow(), &pid);
    HANDLE process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
    len = COUNTOF(path);
    BOOL ok = process && QueryFullProcessImageNameW(process, 0, path, &len);
    if (process) CloseHandle(process);
    WCHAR stem[256] = L"";
    if (ok) {
        wcopy(stem, COUNTOF(stem), path_file_name(path));
        for (int i = wlen(stem); i-- > 0;)
            if (stem[i] == '.') {
                stem[i] = 0;
                break;
            }
    }
    // Our own windows (and Explorer) count as the desktop.
    if (!stem[0] || weq(stem, L"explorer") || weq(stem, L"bufferless")) {
        wcopy(out, cap, L"Desktop");
        return;
    }
    int o = 0;
    for (int i = 0; stem[i] && o < cap - 1; i++)
        if (!wcontains(L"<>:\"/\\|?*", (WCHAR[]){stem[i], 0})) out[o++] = stem[i];
    out[o] = 0;
}

void hotkey_name(UINT32 mods, UINT32 vk, WCHAR *out, int cap) {
    int n = 0;
    out[0] = 0;
    if (mods & MOD_CONTROL) n += wfmt(out + n, cap - n, "Ctrl+");
    if (mods & MOD_ALT) n += wfmt(out + n, cap - n, "Alt+");
    if (mods & MOD_SHIFT) n += wfmt(out + n, cap - n, "Shift+");
    if (mods & MOD_WIN) n += wfmt(out + n, cap - n, "Win+");
    if (vk >= 0x70 && vk <= 0x87) {
        wfmt(out + n, cap - n, "F%u", vk - 0x6f);
    } else if ((vk >= '0' && vk <= '9') || (vk >= 'A' && vk <= 'Z')) {
        wfmt(out + n, cap - n, "%s", (WCHAR[]){(WCHAR)vk, 0});
    } else {
        UINT scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
        if (!GetKeyNameTextW(scan << 16, out + n, cap - n)) wfmt(out + n, cap - n, "Key %u", vk);
    }
}

void set_autostart(BOOL enable) {
    const WCHAR *key = L"Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    if (enable) {
        WCHAR exe[MAX_PATH * 2], value[MAX_PATH * 2 + 4];
        GetModuleFileNameW(NULL, exe, COUNTOF(exe));
        int n = wfmt(value, COUNTOF(value), "\"%s\"", exe);
        RegSetKeyValueW(HKEY_CURRENT_USER, key, L"Bufferless", REG_SZ, value, (n + 1) * sizeof(WCHAR));
    } else {
        RegDeleteKeyValueW(HKEY_CURRENT_USER, key, L"Bufferless");
    }
}

BOOL taskbar_is_light(void) {
    DWORD value = 0, size = sizeof(value);
    return RegGetValueW(HKEY_CURRENT_USER, L"Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize",
                        L"SystemUsesLightTheme", RRF_RT_REG_DWORD, NULL, &value, &size) == ERROR_SUCCESS &&
           value;
}
