// The tray app: hidden window, tray icon, global hotkey, clip saving.
#include "app.h"
#include "audio.h"
#include "capture.h"
#include "mux.h"
#include "util.h"

#include <shellapi.h>

#define WM_TRAY (WM_APP + 1)
#define WM_CLIP_SAVED (WM_APP + 2)
#define HOTKEY_ID 1
#define TIMER_STATUS 1
#define CMD_SAVE 100
#define CMD_OPEN_FOLDER 101
#define CMD_SETTINGS 102
#define CMD_QUIT 103

// ---------------------------------------------------------------------------
// Pipeline: the capture threads. Stopping joins them, so a restart with new
// settings never has two encoders fighting over the GPU.

typedef struct {
    volatile LONG stop;
    HANDLE threads[2];
    const WCHAR *names[2];
    int count;
    VideoThread video;
    AudioThread audio;
    SRWLOCK status_lock;
    WCHAR status[256];
} Pipeline;

static Pipeline *pipeline_start(const Config *cfg, Ring *ring) {
    Pipeline *p = mem_alloc(sizeof(Pipeline));
    ring_set_keep(ring, cfg->replay_seconds);
    InitializeSRWLock(&p->status_lock);
    wcopy(p->status, COUNTOF(p->status), L"Starting...");

    p->video = (VideoThread){{cfg->monitor, cfg->fps, cfg->bitrate_mbps * 1000000, cfg->resolution, cfg->capture_cursor},
                             ring, &p->stop, &p->status_lock, p->status, COUNTOF(p->status)};
    p->names[p->count] = L"video";
    p->threads[p->count++] = CreateThread(NULL, 0, capture_thread, &p->video, 0, NULL);

    if (cfg->system_audio || cfg->microphone) {
        p->audio.system = cfg->system_audio;
        p->audio.mic = cfg->microphone;
        wcopy(p->audio.mic_device, COUNTOF(p->audio.mic_device), cfg->microphone_device);
        p->audio.mic_volume = cfg->microphone_volume / 100.0f;
        p->audio.ring = ring;
        p->audio.stop = &p->stop;
        p->names[p->count] = L"audio";
        p->threads[p->count++] = CreateThread(NULL, 0, audio_thread, &p->audio, 0, NULL);
    }
    return p;
}

static void pipeline_stop(Pipeline *p) {
    if (!p) return;
    InterlockedExchange(&p->stop, 1);
    for (int i = 0; i < p->count; i++) {
        WaitForSingleObject(p->threads[i], INFINITE);
        CloseHandle(p->threads[i]);
        logmsg("pipeline: %s thread stopped", p->names[i]);
    }
    mem_free(p);
}

static void pipeline_status(Pipeline *p, WCHAR *out, int cap) {
    out[0] = 0;
    if (!p) return;
    AcquireSRWLockShared(&p->status_lock);
    wcopy(out, cap, p->status);
    ReleaseSRWLockShared(&p->status_lock);
}

// ---------------------------------------------------------------------------

static struct {
    HWND hwnd;
    Config cfg;
    Ring ring;
    Pipeline *pipeline;
    HICON icon_ok, icon_error;
    BOOL showing_error;
    UINT taskbar_created;
    BOOL hotkey_ok;
    WCHAR *last_clip;
} app;

void app_current_config(Config *out) {
    config_copy(out, &app.cfg);
}

// Tray icons for the normal and error states, matched to the taskbar theme:
// white on a dark taskbar, near-black on a light one. Errors dim the mark.
static void tray_icons(void) {
    if (app.icon_ok) DestroyIcon(app.icon_ok);
    if (app.icon_error) DestroyIcon(app.icon_error);
    int size = MAX(GetSystemMetrics(SM_CXSMICON), 16);
    COLORREF rgb = taskbar_is_light() ? RGB(0x1f, 0x1f, 0x1f) : RGB(0xff, 0xff, 0xff);
    app.icon_ok = icon_glyph(size, rgb, 1.0f);
    app.icon_error = icon_glyph(size, rgb, 0.4f);
}

static void tray_data(NOTIFYICONDATAW *nid) {
    memset(nid, 0, sizeof(*nid));
    nid->cbSize = sizeof(*nid);
    nid->hWnd = app.hwnd;
    nid->uID = 1;
}

static void tooltip(WCHAR *out, int cap) {
    WCHAR status[256];
    UINT32 buffered;
    UINT64 bytes;
    pipeline_status(app.pipeline, status, COUNTOF(status));
    ring_stats(&app.ring, &buffered, &bytes);
    // The ring keeps a few extra seconds so clips can start on a keyframe, but
    // a clip is never longer than the replay length.
    buffered = MIN(buffered, app.cfg.replay_seconds);
    wfmt(out, cap, "Bufferless\n%s\n%us of %us buffered (%llu MB)", status, buffered, app.cfg.replay_seconds,
         bytes / 1000000);
}

static void add_tray_icon(void) {
    NOTIFYICONDATAW nid;
    tray_data(&nid);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = app.showing_error ? app.icon_error : app.icon_ok;
    tooltip(nid.szTip, COUNTOF(nid.szTip));
    nid.uVersion = NOTIFYICON_VERSION_4;
    Shell_NotifyIconW(NIM_ADD, &nid);
    Shell_NotifyIconW(NIM_SETVERSION, &nid);
}

static void refresh_tray(void) {
    WCHAR status[256];
    pipeline_status(app.pipeline, status, COUNTOF(status));
    app.showing_error = wcontains(status, L"error");
    NOTIFYICONDATAW nid;
    tray_data(&nid);
    nid.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    nid.hIcon = app.showing_error ? app.icon_error : app.icon_ok;
    tooltip(nid.szTip, COUNTOF(nid.szTip));
    Shell_NotifyIconW(NIM_MODIFY, &nid);
}

static void notify(const char *title, const char *fmt, ...) {
    NOTIFYICONDATAW nid;
    tray_data(&nid);
    nid.uFlags = NIF_INFO;
    wfmt(nid.szInfoTitle, COUNTOF(nid.szInfoTitle), title);
    va_list ap;
    va_start(ap, fmt);
    wfmtv(nid.szInfo, COUNTOF(nid.szInfo), fmt, ap);
    va_end(ap);
    nid.dwInfoFlags = NIIF_NONE | NIIF_NOSOUND;
    Shell_NotifyIconW(NIM_MODIFY, &nid);
}

static void register_hotkey(void) {
    UnregisterHotKey(app.hwnd, HOTKEY_ID);
    app.hotkey_ok = RegisterHotKey(app.hwnd, HOTKEY_ID, app.cfg.hotkey_modifiers | MOD_NOREPEAT, app.cfg.hotkey_key);
    if (!app.hotkey_ok) {
        WCHAR name[64];
        hotkey_name(app.cfg.hotkey_modifiers, app.cfg.hotkey_key, name, COUNTOF(name));
        logmsg("hotkey %s is taken", name);
        notify("Hotkey unavailable", "%s is already used by another app. Pick a different one in Settings.", name);
    }
}

// ---------------------------------------------------------------------------
// Saving clips

typedef struct {
    Clip clip;
    WCHAR folder[MAX_PATH * 2];
    WCHAR stem[300];
    HWND hwnd;
    // Result
    WCHAR path[MAX_PATH * 2];
    UINT64 size;
    WCHAR error[300];
} SaveJob;

static DWORD WINAPI save_thread(void *param) {
    SaveJob *job = param;
    INT64 start = clock_now();
    // Two saves within the same second shouldn't overwrite each other.
    for (int n = 1;; n++) {
        WCHAR name[400];
        if (n == 1) wfmt(name, COUNTOF(name), "%s.mp4", job->stem);
        else wfmt(name, COUNTOF(name), "%s (%d).mp4", job->stem, n);
        path_join(job->path, COUNTOF(job->path), job->folder, name);
        if (!file_exists(job->path)) break;
    }
    if (!dir_create_all(job->folder)) {
        hr_text(HRESULT_FROM_WIN32(GetLastError()), job->error, COUNTOF(job->error));
    } else {
        job->size = mux_write_mp4(job->path, &job->clip, job->error, COUNTOF(job->error));
    }
    if (job->size) logmsg("saved %s (%llu bytes) in %lld ms", job->path, job->size, (clock_now() - start) / 10000);
    else logmsg("save failed: %s", job->error);
    clip_free(&job->clip);
    PostMessageW(job->hwnd, WM_CLIP_SAVED, 0, (LPARAM)job);
    return 0;
}

static void save_clip(void) {
    SaveJob *job = mem_alloc(sizeof(SaveJob));
    if (!ring_snapshot(&app.ring, app.cfg.replay_seconds, &job->clip)) {
        mem_free(job);
        play_error_sound();
        notify("Nothing to save yet", "The replay buffer is still empty.");
        return;
    }
    WCHAR fg[256], ts[64];
    foreground_app_name(fg, COUNTOF(fg));
    timestamp(ts, COUNTOF(ts));
    wfmt(job->stem, COUNTOF(job->stem), "%s %s", fg, ts);
    wcopy(job->folder, COUNTOF(job->folder), app.cfg.save_folder);
    job->hwnd = app.hwnd;
    HANDLE t = CreateThread(NULL, 0, save_thread, job, 0, NULL);
    if (t) CloseHandle(t);
}

static void on_clip_saved(SaveJob *job) {
    if (job->size) {
        if (app.cfg.save_sound) play_saved_sound();
        // Integer math: float formatting would need the C runtime.
        UINT64 tenths = (job->size + 50000) / 100000;
        notify("Clip saved", "%s (%llu.%llu MB)", path_file_name(job->path), tenths / 10, tenths % 10);
        mem_free(app.last_clip);
        app.last_clip = wdup(job->path);
    } else {
        play_error_sound();
        notify("Couldn't save clip", "%s", job->error);
    }
    mem_free(job);
}

// Open the clips folder, optionally with a file selected.
static void open_folder(const WCHAR *folder, const WCHAR *select) {
    dir_create_all(folder);
    if (select) {
        WCHAR params[MAX_PATH * 2 + 16];
        wfmt(params, COUNTOF(params), "/select,\"%s\"", select);
        ShellExecuteW(NULL, L"open", L"explorer.exe", params, NULL, SW_SHOWNORMAL);
    } else {
        ShellExecuteW(NULL, L"open", folder, NULL, NULL, SW_SHOWNORMAL);
    }
}

// ---------------------------------------------------------------------------

void app_apply_config(Config *cfg) {
    Config old = app.cfg;
    app.cfg = *cfg; // takes ownership of the strings
    if (!config_save(&app.cfg)) logmsg("couldn't save config");
    if (config_capture_differs(&old, &app.cfg)) {
        logmsg("capture settings changed, restarting pipeline");
        pipeline_stop(app.pipeline); // joins the old threads first
        ring_clear(&app.ring);
        app.pipeline = pipeline_start(&app.cfg, &app.ring);
    } else if (old.replay_seconds != app.cfg.replay_seconds) {
        ring_set_keep(&app.ring, app.cfg.replay_seconds);
    }
    if (old.hotkey_modifiers != app.cfg.hotkey_modifiers || old.hotkey_key != app.cfg.hotkey_key || !app.hotkey_ok)
        register_hotkey();
    if (old.start_with_windows != app.cfg.start_with_windows) set_autostart(app.cfg.start_with_windows);
    config_free(&old);
    refresh_tray();
}

static UINT show_menu(void) {
    HMENU menu = CreatePopupMenu();
    WCHAR name[64], label[96];
    hotkey_name(app.cfg.hotkey_modifiers, app.cfg.hotkey_key, name, COUNTOF(name));
    wfmt(label, COUNTOF(label), "Save clip\t%s", name);
    AppendMenuW(menu, MF_STRING, CMD_SAVE, label);
    AppendMenuW(menu, MF_STRING, CMD_OPEN_FOLDER, L"Open clips folder");
    AppendMenuW(menu, MF_STRING, CMD_SETTINGS, L"Settings...");
    AppendMenuW(menu, MF_SEPARATOR, 0, NULL);
    AppendMenuW(menu, MF_STRING, CMD_QUIT, L"Quit");
    SetMenuDefaultItem(menu, CMD_SAVE, FALSE);
    POINT pt;
    GetCursorPos(&pt);
    // Required so the menu closes when clicking elsewhere.
    SetForegroundWindow(app.hwnd);
    UINT cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, app.hwnd, NULL);
    DestroyMenu(menu);
    return cmd;
}

static LRESULT CALLBACK wndproc(HWND hwnd, UINT msg, WPARAM wparam, LPARAM lparam) {
    switch (msg) {
    case WM_HOTKEY:
        if (wparam == HOTKEY_ID) save_clip();
        return 0;
    case WM_TRAY:
        // NOTIFYICON_VERSION_4: the event is in the low word of lparam.
        switch (LOWORD(lparam)) {
        case WM_CONTEXTMENU:
            switch (show_menu()) {
            case CMD_SAVE: save_clip(); break;
            case CMD_OPEN_FOLDER: open_folder(app.cfg.save_folder, NULL); break;
            case CMD_SETTINGS: settings_open(&app.cfg); break;
            case CMD_QUIT: PostQuitMessage(0); break;
            }
            break;
        case WM_LBUTTONDBLCLK: settings_open(&app.cfg); break;
        case NIN_BALLOONUSERCLICK: open_folder(app.cfg.save_folder, app.last_clip); break;
        }
        return 0;
    case WM_CLIP_SAVED:
        on_clip_saved((SaveJob *)lparam);
        return 0;
    case WM_SHOW_SETTINGS:
        settings_open(&app.cfg);
        return 0;
    case WM_TIMER:
        if (wparam == TIMER_STATUS) refresh_tray();
        return 0;
    case WM_SETTINGCHANGE:
        // Light/dark switches arrive as "ImmersiveColorSet"; just redraw.
        tray_icons();
        refresh_tray();
        break;
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    default:
        if (msg && msg == app.taskbar_created) {
            add_tray_icon();
            return 0;
        }
    }
    return DefWindowProcW(hwnd, msg, wparam, lparam);
}

int app_run(BOOL first_run) {
    HINSTANCE instance = GetModuleHandleW(NULL);
    WNDCLASSW wc = {0, wndproc, 0, 0, instance, NULL, NULL, NULL, NULL, WINDOW_CLASS};
    RegisterClassW(&wc);
    // A real (hidden) top-level window rather than a message-only one, so we
    // receive the TaskbarCreated broadcast when Explorer restarts.
    app.hwnd = CreateWindowExW(0, WINDOW_CLASS, L"Bufferless", WS_OVERLAPPED, 0, 0, 0, 0, NULL, NULL, instance, NULL);

    config_load(&app.cfg);
    // Re-point the startup entry at this exe in case it was moved or replaced.
    if (app.cfg.start_with_windows) set_autostart(TRUE);
    if (first_run) config_save(&app.cfg);
    tray_icons();
    ring_init(&app.ring, app.cfg.replay_seconds);
    app.pipeline = pipeline_start(&app.cfg, &app.ring);
    app.taskbar_created = RegisterWindowMessageW(L"TaskbarCreated");

    add_tray_icon();
    register_hotkey();
    if (first_run && app.hotkey_ok) {
        WCHAR name[64];
        hotkey_name(app.cfg.hotkey_modifiers, app.cfg.hotkey_key, name, COUNTOF(name));
        notify("Bufferless is running", "Press %s to save the last %u seconds. Right-click the tray icon for settings.",
               name, app.cfg.replay_seconds);
    }
    SetTimer(app.hwnd, TIMER_STATUS, 2000, NULL);

    MSG msg;
    while (GetMessageW(&msg, NULL, 0, 0) > 0) {
        HWND dlg = settings_window();
        if (dlg && IsDialogMessageW(dlg, &msg)) continue;
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    // Stops the pipeline and joins its threads.
    pipeline_stop(app.pipeline);
    NOTIFYICONDATAW nid;
    tray_data(&nid);
    Shell_NotifyIconW(NIM_DELETE, &nid);
    return 0;
}
