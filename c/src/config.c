// Settings in %APPDATA%\bufferless\config.toml: flat `key = value` lines, the
// same file the Rust build reads and writes.
#include "config.h"

#include <knownfolders.h>
#include <shlobj.h>

#define AUDIO_BITRATE 192000

static WCHAR *known_folder(REFKNOWNFOLDERID id, const WCHAR *child) {
    WCHAR *p = NULL, out[MAX_PATH * 2];
    if (FAILED(SHGetKnownFolderPath(id, 0, NULL, &p))) p = NULL;
    path_join(out, COUNTOF(out), p ? p : L".", child);
    CoTaskMemFree(p);
    return wdup(out);
}

void config_dir(WCHAR *out, int cap) {
    WCHAR *dir = known_folder(&FOLDERID_RoamingAppData, L"bufferless");
    wcopy(out, cap, dir);
    mem_free(dir);
}

void config_default(Config *c) {
    memset(c, 0, sizeof(*c));
    c->hotkey_modifiers = MOD_ALT;
    c->hotkey_key = VK_F10;
    c->replay_seconds = 60;
    c->fps = 60;
    c->bitrate_mbps = 20;
    c->capture_cursor = TRUE;
    c->system_audio = TRUE;
    c->microphone_device = wdup(L"");
    c->microphone_volume = 100;
    c->save_folder = known_folder(&FOLDERID_Videos, L"Bufferless");
    c->save_sound = TRUE;
}

void config_free(Config *c) {
    mem_free(c->microphone_device);
    mem_free(c->save_folder);
    c->microphone_device = c->save_folder = NULL;
}

void config_copy(Config *dst, const Config *src) {
    *dst = *src;
    dst->microphone_device = wdup(src->microphone_device);
    dst->save_folder = wdup(src->save_folder);
}

static void sanitize(Config *c) {
    c->replay_seconds = CLAMP(c->replay_seconds, 5, 30 * 60);
    c->fps = CLAMP(c->fps, 10, 240);
    c->bitrate_mbps = CLAMP(c->bitrate_mbps, 1, 150);
    c->microphone_volume = MIN(c->microphone_volume, 400);
}

// A TOML basic ("...") or literal ('...') string. Returns NULL if malformed.
static WCHAR *parse_string(const WCHAR *v) {
    int n = wlen(v);
    if (n < 2 || v[0] != v[n - 1] || (v[0] != '"' && v[0] != '\'')) return NULL;
    WCHAR *out = mem_alloc(n * sizeof(WCHAR));
    int o = 0;
    for (int i = 1; i < n - 1; i++) {
        if (v[0] == '"' && v[i] == '\\' && i + 1 < n - 1) {
            WCHAR e = v[++i];
            out[o++] = e == 'n' ? '\n' : e == 't' ? '\t' : e;
        } else {
            out[o++] = v[i];
        }
    }
    return out;
}

static void set_u32(UINT32 *field, const WCHAR *v) {
    *field = wparse_u32(v, *field);
}

static void set_bool(BOOL *field, const WCHAR *v) {
    if (weqa(v, "true")) *field = TRUE;
    else if (weqa(v, "false")) *field = FALSE;
}

static void set_str(WCHAR **field, const WCHAR *v) {
    WCHAR *s = parse_string(v);
    if (s) mem_free(*field), *field = s;
}

static void parse(Config *c, WCHAR *text) {
    for (WCHAR *line = text; line && *line;) {
        WCHAR *next = line;
        while (*next && *next != '\n') next++;
        if (*next) *next++ = 0;
        WCHAR *eq = line;
        while (*eq && *eq != '=') eq++;
        if (*eq && line[0] != '#') {
            *eq = 0;
            WCHAR *key = line, *v = eq + 1;
            wtrim(key);
            int n = wlen(v);
            if (n && v[n - 1] == '\r') v[n - 1] = 0;
            wtrim(v);
            if (weqa(key, "hotkey_modifiers")) set_u32(&c->hotkey_modifiers, v);
            else if (weqa(key, "hotkey_key")) set_u32(&c->hotkey_key, v);
            else if (weqa(key, "replay_seconds")) set_u32(&c->replay_seconds, v);
            else if (weqa(key, "monitor")) set_u32(&c->monitor, v);
            else if (weqa(key, "resolution")) set_u32(&c->resolution, v);
            else if (weqa(key, "fps")) set_u32(&c->fps, v);
            else if (weqa(key, "bitrate_mbps")) set_u32(&c->bitrate_mbps, v);
            else if (weqa(key, "capture_cursor")) set_bool(&c->capture_cursor, v);
            else if (weqa(key, "system_audio")) set_bool(&c->system_audio, v);
            else if (weqa(key, "microphone")) set_bool(&c->microphone, v);
            else if (weqa(key, "microphone_device")) set_str(&c->microphone_device, v);
            else if (weqa(key, "microphone_volume")) set_u32(&c->microphone_volume, v);
            else if (weqa(key, "save_folder")) set_str(&c->save_folder, v);
            else if (weqa(key, "save_sound")) set_bool(&c->save_sound, v);
            else if (weqa(key, "start_with_windows")) set_bool(&c->start_with_windows, v);
        }
        line = next;
    }
}

static void config_path(WCHAR *out, int cap) {
    WCHAR dir[MAX_PATH * 2];
    config_dir(dir, COUNTOF(dir));
    path_join(out, cap, dir, L"config.toml");
}

void config_load(Config *c) {
    config_default(c);
    WCHAR path[MAX_PATH * 2];
    config_path(path, COUNTOF(path));
    DWORD len;
    char *data = file_read_all(path, &len);
    if (data) {
        WCHAR *text = from_utf8(data, (int)len);
        parse(c, text);
        mem_free(text);
        mem_free(data);
    }
    sanitize(c);
}

// "...", escaping backslashes and quotes.
static WCHAR *quote(const WCHAR *s) {
    WCHAR *out = mem_alloc((wlen(s) * 2 + 3) * sizeof(WCHAR));
    int o = 0;
    out[o++] = '"';
    for (; *s; s++) {
        if (*s == '\\' || *s == '"') out[o++] = '\\';
        out[o++] = *s;
    }
    out[o] = '"';
    return out;
}

BOOL config_save(const Config *c) {
    WCHAR dir[MAX_PATH * 2], path[MAX_PATH * 2];
    config_dir(dir, COUNTOF(dir));
    dir_create_all(dir);
    config_path(path, COUNTOF(path));
    WCHAR *mic = quote(c->microphone_device), *folder = quote(c->save_folder);
    const WCHAR *b[2] = {L"false", L"true"};
    WCHAR *text = wfmt_alloc("hotkey_modifiers = %u\nhotkey_key = %u\nreplay_seconds = %u\nmonitor = %u\n"
                             "resolution = %u\nfps = %u\nbitrate_mbps = %u\ncapture_cursor = %s\n"
                             "system_audio = %s\nmicrophone = %s\nmicrophone_device = %s\n"
                             "microphone_volume = %u\nsave_folder = %s\nsave_sound = %s\nstart_with_windows = %s\n",
                             c->hotkey_modifiers, c->hotkey_key, c->replay_seconds, c->monitor, c->resolution, c->fps,
                             c->bitrate_mbps, b[!!c->capture_cursor], b[!!c->system_audio], b[!!c->microphone], mic,
                             c->microphone_volume, folder, b[!!c->save_sound], b[!!c->start_with_windows]);
    int len;
    char *utf8 = to_utf8(text, &len);
    BOOL ok = file_write_all(path, utf8, len);
    mem_free(utf8), mem_free(text), mem_free(mic), mem_free(folder);
    return ok;
}

BOOL config_capture_differs(const Config *a, const Config *b) {
    return a->monitor != b->monitor || a->resolution != b->resolution || a->fps != b->fps ||
           a->bitrate_mbps != b->bitrate_mbps || a->capture_cursor != b->capture_cursor ||
           a->system_audio != b->system_audio || a->microphone != b->microphone ||
           !weq(a->microphone_device, b->microphone_device) || a->microphone_volume != b->microphone_volume;
}

UINT32 config_memory_mb(const Config *c) {
    UINT64 per_sec = (UINT64)c->bitrate_mbps * 1000000 / 8 + AUDIO_BITRATE / 8;
    return (UINT32)(per_sec * (c->replay_seconds + 3) / 1000000);
}
