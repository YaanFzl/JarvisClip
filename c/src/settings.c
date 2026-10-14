// Settings window built from plain Win32 controls.
#include "app.h"
#include "audio.h"
#include "capture.h"
#include "util.h"

#include <commctrl.h>
#include <shobjidl.h>

#define CLASS L"BufferlessSettings"

enum {
    ID_HOTKEY = 100,
    ID_LENGTH,
    ID_MONITOR,
    ID_RESOLUTION,
    ID_FPS,
    ID_BITRATE,
    ID_MEMORY,
    ID_SYSTEM_AUDIO,
    ID_MIC,
    ID_MIC_DEVICE,
    ID_FOLDER,
    ID_BROWSE,
    ID_SOUND,
    ID_AUTOSTART,
    ID_MIC_VOLUME,
    ID_CURSOR,
};

static const UINT32 LENGTHS[] = {15, 30, 60, 90, 120, 180, 300, 600, 1200};
static const char *LENGTH_NAMES[] = {"15 seconds", "30 seconds", "1 minute",   "1.5 minutes", "2 minutes",
                                     "3 minutes",  "5 minutes",  "10 minutes", "20 minutes"};
static const UINT32 RESOLUTIONS[] = {0, 1440, 1080, 900, 720};
static const char *RESOLUTION_NAMES[] = {"Native", "1440p", "1080p", "900p", "720p"};
static const UINT32 FRAME_RATES[] = {30, 60, 90, 120, 144, 165};

static struct {
    HWND hwnd;
    Config cfg;
    MonitorInfo *monitors;
    UINT32 monitor_count;
    DeviceInfo *mics;
    UINT32 mic_count;
    HFONT font, heading_font;
    int dpi;
} st;

HWND settings_window(void) {
    return st.hwnd;
}

static HWND item(int id) {
    return GetDlgItem(st.hwnd, id);
}

static BOOL checked(int id) {
    return SendMessageW(item(id), BM_GETCHECK, 0, 0) == BST_CHECKED;
}

static UINT32 combo_index(int id) {
    LRESULT i = SendMessageW(item(id), CB_GETCURSEL, 0, 0);
    return i < 0 ? 0 : (UINT32)i;
}

static WCHAR *window_text(HWND h) {
    int n = GetWindowTextLengthW(h);
    WCHAR *s = mem_alloc((n + 1) * sizeof(WCHAR));
    GetWindowTextW(h, s, n + 1);
    return s;
}

static int S(int v) {
    return v * st.dpi / 96;
}

static HWND create(const char *cls, const char *text, DWORD style, DWORD ex, int x, int y, int w, int h, int id,
                   HFONT font) {
    WCHAR wcls[32], wtext[64];
    wfmt(wcls, COUNTOF(wcls), "%S", cls);
    wfmt(wtext, COUNTOF(wtext), "%S", text);
    HWND ctl = CreateWindowExW(ex, wcls, wtext, style | WS_CHILD | WS_VISIBLE, x, y, w, h, st.hwnd,
                               (HMENU)(INT_PTR)id, NULL, NULL);
    SendMessageW(ctl, WM_SETFONT, (WPARAM)font, 1);
    return ctl;
}

// Layout constants (scaled for DPI in build()).
static int label_x, ctl_x, ctl_w, row_h, row, width;

static void heading(const char *text, int y) {
    create("STATIC", text, 0, 0, label_x, y, ctl_w, row_h, -1, st.heading_font);
}

static void label(const char *text, int y) {
    create("STATIC", text, 0, 0, label_x, y + S(4), ctl_x - label_x, row_h, -1, st.font);
}

static HWND combo(int id, int y, int w) {
    return create("COMBOBOX", "", CBS_DROPDOWNLIST | WS_VSCROLL | WS_TABSTOP, 0, ctl_x, y, w, S(300), id, st.font);
}

static void combo_add(HWND ctl, const WCHAR *text) {
    SendMessageW(ctl, CB_ADDSTRING, 0, (LPARAM)text);
}

static void combo_add_ascii(HWND ctl, const char *text) {
    WCHAR w[64];
    wfmt(w, COUNTOF(w), "%S", text);
    combo_add(ctl, w);
}

static void checkbox(int id, const char *text, int x, int y, int w, BOOL on) {
    HWND ctl = create("BUTTON", text, BS_AUTOCHECKBOX | WS_TABSTOP, 0, x, y, w, row_h, id, st.font);
    SendMessageW(ctl, BM_SETCHECK, on ? BST_CHECKED : BST_UNCHECKED, 0);
}

static HWND number_edit(int id, UINT32 value, int y) {
    WCHAR text[16];
    wfmt(text, COUNTOF(text), "%u", value);
    HWND ctl = create("EDIT", "", ES_NUMBER | ES_AUTOHSCROLL | WS_TABSTOP, WS_EX_CLIENTEDGE, ctl_x, y, S(60), row_h, id,
                      st.font);
    SetWindowTextW(ctl, text);
    return ctl;
}

// Read the form into a Config (caller frees).
static void read_form(Config *cfg) {
    config_copy(cfg, &st.cfg);
    UINT32 hk = (UINT32)SendMessageW(item(ID_HOTKEY), HKM_GETHOTKEY, 0, 0), vk = hk & 0xff, f = (hk >> 8) & 0xff;
    if (vk) {
        cfg->hotkey_key = vk;
        cfg->hotkey_modifiers = 0;
        if (f & HOTKEYF_ALT) cfg->hotkey_modifiers |= MOD_ALT;
        if (f & HOTKEYF_CONTROL) cfg->hotkey_modifiers |= MOD_CONTROL;
        if (f & HOTKEYF_SHIFT) cfg->hotkey_modifiers |= MOD_SHIFT;
    }
    cfg->replay_seconds = LENGTHS[MIN(combo_index(ID_LENGTH), COUNTOF(LENGTHS) - 1)];
    cfg->monitor = combo_index(ID_MONITOR);
    cfg->resolution = RESOLUTIONS[MIN(combo_index(ID_RESOLUTION), COUNTOF(RESOLUTIONS) - 1)];
    cfg->fps = FRAME_RATES[MIN(combo_index(ID_FPS), COUNTOF(FRAME_RATES) - 1)];
    WCHAR *t = window_text(item(ID_BITRATE));
    cfg->bitrate_mbps = CLAMP(wparse_u32(t, cfg->bitrate_mbps), 1, 150);
    mem_free(t);
    cfg->capture_cursor = checked(ID_CURSOR);
    cfg->system_audio = checked(ID_SYSTEM_AUDIO);
    cfg->microphone = checked(ID_MIC);
    UINT32 mic = combo_index(ID_MIC_DEVICE);
    mem_free(cfg->microphone_device);
    cfg->microphone_device = wdup(mic && mic - 1 < st.mic_count ? st.mics[mic - 1].id : L"");
    t = window_text(item(ID_MIC_VOLUME));
    cfg->microphone_volume = MIN(wparse_u32(t, cfg->microphone_volume), 400);
    mem_free(t);
    t = window_text(item(ID_FOLDER));
    wtrim(t);
    if (t[0]) mem_free(cfg->save_folder), cfg->save_folder = t;
    else mem_free(t);
    cfg->save_sound = checked(ID_SOUND);
    cfg->start_with_windows = checked(ID_AUTOSTART);
}

// Refresh the RAM estimate and enable/disable mic controls.
static void update_dependent(void) {
    Config cfg;
    read_form(&cfg);
    WCHAR text[64];
    wfmt(text, COUNTOF(text), "uses ~%u MB of RAM", config_memory_mb(&cfg));
    SetWindowTextW(item(ID_MEMORY), text);
    EnableWindow(item(ID_MIC_DEVICE), cfg.microphone);
    EnableWindow(item(ID_MIC_VOLUME), cfg.microphone);
    config_free(&cfg);
}

// Create all controls. Returns the client size.
static SIZE build(void) {
    const Config *cfg = &st.cfg;
    label_x = S(20), ctl_x = S(170), ctl_w = S(270), row_h = S(24), row = S(32);
    width = ctl_x + ctl_w + S(20);
    int y = S(14);

    // --- Recording
    heading("Recording", y);
    y += row;
    label("Save clip hotkey", y);
    HWND hk = create("msctls_hotkey32", "", WS_TABSTOP, WS_EX_CLIENTEDGE, ctl_x, y, ctl_w, row_h, ID_HOTKEY, st.font);
    UINT32 f = 0;
    if (cfg->hotkey_modifiers & MOD_ALT) f |= HOTKEYF_ALT;
    if (cfg->hotkey_modifiers & MOD_CONTROL) f |= HOTKEYF_CONTROL;
    if (cfg->hotkey_modifiers & MOD_SHIFT) f |= HOTKEYF_SHIFT;
    SendMessageW(hk, HKM_SETHOTKEY, cfg->hotkey_key | f << 8, 0);
    y += row;

    label("Replay length", y);
    HWND c = combo(ID_LENGTH, y, ctl_w);
    UINT32 sel = 2;
    for (UINT32 i = 0; i < COUNTOF(LENGTHS); i++) combo_add_ascii(c, LENGTH_NAMES[i]);
    for (UINT32 i = 0; i < COUNTOF(LENGTHS); i++)
        if (LENGTHS[i] >= cfg->replay_seconds) {
            sel = i;
            break;
        }
    SendMessageW(c, CB_SETCURSEL, sel, 0);
    y += row;

    label("Display", y);
    c = combo(ID_MONITOR, y, ctl_w);
    for (UINT32 i = 0; i < st.monitor_count; i++) {
        WCHAR text[128];
        wfmt(text, COUNTOF(text), "%s - %ux%u", st.monitors[i].name, st.monitors[i].width, st.monitors[i].height);
        combo_add(c, text);
    }
    SendMessageW(c, CB_SETCURSEL, MIN(cfg->monitor, st.monitor_count ? st.monitor_count - 1 : 0), 0);
    y += row;

    label("Resolution", y);
    c = combo(ID_RESOLUTION, y, ctl_w);
    sel = 0;
    for (UINT32 i = 0; i < COUNTOF(RESOLUTIONS); i++) {
        combo_add_ascii(c, RESOLUTION_NAMES[i]);
        if (RESOLUTIONS[i] == cfg->resolution) sel = i;
    }
    SendMessageW(c, CB_SETCURSEL, sel, 0);
    y += row;

    label("Frame rate", y);
    c = combo(ID_FPS, y, ctl_w);
    sel = 1;
    for (UINT32 i = 0; i < COUNTOF(FRAME_RATES); i++) {
        WCHAR text[16];
        wfmt(text, COUNTOF(text), "%u fps", FRAME_RATES[i]);
        combo_add(c, text);
    }
    for (UINT32 i = 0; i < COUNTOF(FRAME_RATES); i++)
        if (FRAME_RATES[i] >= cfg->fps) {
            sel = i;
            break;
        }
    SendMessageW(c, CB_SETCURSEL, sel, 0);
    y += row;

    label("Bitrate (Mbps)", y);
    number_edit(ID_BITRATE, cfg->bitrate_mbps, y);
    create("STATIC", "", 0, 0, ctl_x + S(72), y + S(4), ctl_w - S(72), row_h, ID_MEMORY, st.font);
    y += row;
    checkbox(ID_CURSOR, "Record the mouse cursor", label_x, y, width - 2 * label_x, cfg->capture_cursor);
    y += row + S(10);

    // --- Audio
    heading("Audio", y);
    y += row;
    checkbox(ID_SYSTEM_AUDIO, "Record system audio", label_x, y, ctl_w, cfg->system_audio);
    y += row;
    checkbox(ID_MIC, "Record microphone", label_x, y, ctl_x - label_x, cfg->microphone);
    c = combo(ID_MIC_DEVICE, y, ctl_w);
    combo_add_ascii(c, "Default microphone");
    sel = 0;
    for (UINT32 i = 0; i < st.mic_count; i++) {
        combo_add(c, st.mics[i].name);
        if (weq(st.mics[i].id, cfg->microphone_device)) sel = i + 1;
    }
    SendMessageW(c, CB_SETCURSEL, sel, 0);
    y += row;
    label("Mic volume (%)", y);
    number_edit(ID_MIC_VOLUME, cfg->microphone_volume, y);
    y += row + S(10);

    // --- General
    heading("General", y);
    y += row;
    label("Save clips to", y);
    SetWindowTextW(create("EDIT", "", ES_AUTOHSCROLL | WS_TABSTOP, WS_EX_CLIENTEDGE, ctl_x, y, ctl_w - S(84), row_h,
                          ID_FOLDER, st.font),
                   cfg->save_folder);
    create("BUTTON", "Browse...", BS_PUSHBUTTON | WS_TABSTOP, 0, ctl_x + ctl_w - S(76), y - S(1), S(76),
           row_h + S(2), ID_BROWSE, st.font);
    y += row;
    checkbox(ID_SOUND, "Play a sound when a clip is saved", label_x, y, width - 2 * label_x, cfg->save_sound);
    y += row;
    checkbox(ID_AUTOSTART, "Start with Windows", label_x, y, width - 2 * label_x, cfg->start_with_windows);
    y += row + S(14);

    // --- Buttons (IDOK/IDCANCEL so Enter and Esc work via IsDialogMessage)
    int bw = S(88), bh = S(28);
    create("BUTTON", "Save", BS_DEFPUSHBUTTON | WS_TABSTOP, 0, width - S(20) - 2 * bw - S(8), y, bw, bh, IDOK,
           st.font);
    create("BUTTON", "Cancel", BS_PUSHBUTTON | WS_TABSTOP, 0, width - S(20) - bw, y, bw, bh, IDCANCEL, st.font);
    y += bh + S(16);

    update_dependent();
    return (SIZE){width, y};
}

static WCHAR *browse_folder(HWND owner, const WCHAR *current) {
    IFileOpenDialog *dialog = NULL;
    IShellItem *item = NULL, *result = NULL;
    WCHAR *path = NULL, *out = NULL;
    if (FAILED(CoCreateInstance(&CLSID_FileOpenDialog, NULL, CLSCTX_INPROC_SERVER, &IID_IFileOpenDialog,
                                (void **)&dialog)))
        return NULL;
    FILEOPENDIALOGOPTIONS opts;
    if (SUCCEEDED(IFileOpenDialog_GetOptions(dialog, &opts)))
        IFileOpenDialog_SetOptions(dialog, opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM);
    if (SUCCEEDED(SHCreateItemFromParsingName(current, NULL, &IID_IShellItem, (void **)&item)))
        IFileOpenDialog_SetFolder(dialog, item);
    if (SUCCEEDED(IFileOpenDialog_Show(dialog, owner)) && SUCCEEDED(IFileOpenDialog_GetResult(dialog, &result)) &&
        SUCCEEDED(IShellItem_GetDisplayName(result, SIGDN_FILESYSPATH, &path))) {
        out = wdup(path);
        CoTaskMemFree(path);
    }
    RELEASE(result);
    RELEASE(item);
    RELEASE(dialog);
    return out;
}

static LRESULT CALLBACK wndproc(HWND hwnd, UINT msg, WPARAM wparam, LPARAM lparam) {
    switch (msg) {
    case WM_COMMAND: {
        int id = LOWORD(wparam), code = HIWORD(wparam);
        if (id == IDOK) {
            Config *cfg = mem_alloc(sizeof(Config));
            read_form(cfg);
            DestroyWindow(hwnd);
            app_apply_config(cfg);
            mem_free(cfg);
        } else if (id == IDCANCEL) {
            DestroyWindow(hwnd);
        } else if (id == ID_BROWSE) {
            WCHAR *current = window_text(item(ID_FOLDER));
            WCHAR *path = browse_folder(hwnd, current);
            if (path) SetWindowTextW(item(ID_FOLDER), path);
            mem_free(current), mem_free(path);
        } else if ((id == ID_BITRATE || id == ID_LENGTH || id == ID_MIC) &&
                   (code == EN_CHANGE || code == CBN_SELCHANGE || code == BN_CLICKED) && st.hwnd) {
            update_dependent();
        }
        return 0;
    }
    case WM_CTLCOLORSTATIC: {
        // Make labels and checkboxes blend with the window background.
        HDC hdc = (HDC)wparam;
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, GetSysColor(COLOR_WINDOWTEXT));
        return (LRESULT)GetSysColorBrush(COLOR_WINDOW);
    }
    case WM_DESTROY:
        DeleteObject(st.font);
        DeleteObject(st.heading_font);
        mem_free(st.monitors);
        audio_free_devices(st.mics, st.mic_count);
        config_free(&st.cfg);
        memset(&st, 0, sizeof(st));
        return 0;
    }
    return DefWindowProcW(hwnd, msg, wparam, lparam);
}

void settings_open(const Config *cfg) {
    if (st.hwnd) {
        ShowWindow(st.hwnd, SW_RESTORE);
        SetForegroundWindow(st.hwnd);
        return;
    }
    HINSTANCE instance = GetModuleHandleW(NULL);
    WNDCLASSW wc = {0, wndproc, 0, 0, instance, NULL, LoadCursorW(NULL, IDC_ARROW), GetSysColorBrush(COLOR_WINDOW),
                    NULL, CLASS};
    RegisterClassW(&wc); // fails harmlessly if already registered

    DWORD style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    HWND hwnd = CreateWindowExW(WS_EX_CONTROLPARENT, CLASS, L"Bufferless Settings", style, CW_USEDEFAULT,
                                CW_USEDEFAULT, 100, 100, NULL, NULL, instance, NULL);
    if (!hwnd) return;
    SendMessageW(hwnd, WM_SETICON, ICON_SMALL, (LPARAM)icon_tile(GetSystemMetrics(SM_CXSMICON)));
    SendMessageW(hwnd, WM_SETICON, ICON_BIG, (LPARAM)icon_tile(GetSystemMetrics(SM_CXICON)));

    st.dpi = GetDpiForWindow(hwnd);
    NONCLIENTMETRICSW ncm = {sizeof(ncm)};
    SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, ncm.cbSize, &ncm, 0, st.dpi);
    st.font = CreateFontIndirectW(&ncm.lfMessageFont);
    LOGFONTW heading_font = ncm.lfMessageFont;
    heading_font.lfWeight = 600;
    st.heading_font = CreateFontIndirectW(&heading_font);
    config_copy(&st.cfg, cfg);
    st.monitors = capture_list_monitors(&st.monitor_count);
    st.mics = audio_list_microphones(&st.mic_count);

    st.hwnd = hwnd;
    SIZE size = build();

    // Size the window around the content and centre it on its monitor.
    RECT rect = {0, 0, size.cx, size.cy};
    AdjustWindowRectExForDpi(&rect, style, FALSE, WS_EX_CONTROLPARENT, st.dpi);
    int ww = rect.right - rect.left, wh = rect.bottom - rect.top;
    MONITORINFO mi = {sizeof(mi)};
    GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY), &mi);
    RECT work = mi.rcWork;
    SetWindowPos(hwnd, NULL, work.left + (work.right - work.left - ww) / 2, work.top + (work.bottom - work.top - wh) / 2,
                 ww, wh, SWP_NOZORDER);
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
    SetFocus(item(ID_HOTKEY));
}
