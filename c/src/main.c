// Bufferless, C edition. Same app as the Rust build, linked without the C
// runtime: the process starts at entry() below.
#include "app.h"

#include <commctrl.h>
#include <mfapi.h>

#ifndef BUFFERLESS_VERSION
#define BUFFERLESS_VERSION L"dev"
#endif

#ifdef NO_MANIFEST
// The Crinkler build can't carry a manifest, which is what normally gives the
// controls their modern look (Common Controls v6). Windows' own shell32.dll
// has a manifest requesting exactly that, so activate it for the UI thread.
static void enable_visual_styles(void) {
    WCHAR path[MAX_PATH];
    GetSystemDirectoryW(path, MAX_PATH);
    path_join(path, MAX_PATH, path, L"shell32.dll");
    ACTCTXW ctx = {sizeof(ctx)};
    ctx.dwFlags = ACTCTX_FLAG_RESOURCE_NAME_VALID;
    ctx.lpSource = path;
    ctx.lpResourceName = MAKEINTRESOURCEW(124);
    HANDLE h = CreateActCtxW(&ctx);
    ULONG_PTR cookie;
    if (h != INVALID_HANDLE_VALUE) ActivateActCtx(h, &cookie);
}
#endif

static int run(void) {
#ifdef NO_MANIFEST
    enable_visual_styles();
#endif
    // The manifest could set this too; harmless if it fails.
    SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

    // Single instance: a second launch just opens the running one's settings.
    CreateMutexW(NULL, TRUE, L"Local\\Bufferless.SingleInstance");
    if (GetLastError() == ERROR_ALREADY_EXISTS) {
        HWND hwnd = FindWindowW(WINDOW_CLASS, NULL);
        if (hwnd) PostMessageW(hwnd, WM_SHOW_SETTINGS, 0, 0);
        return 0;
    }

    CoInitializeEx(NULL, COINIT_APARTMENTTHREADED);
    if (FAILED(MFStartup(MF_VERSION, MFSTARTUP_FULL))) return 1;
    INITCOMMONCONTROLSEX icc = {sizeof(icc), ICC_HOTKEY_CLASS | ICC_STANDARD_CLASSES};
    InitCommonControlsEx(&icc);

    WCHAR dir[MAX_PATH * 2], path[MAX_PATH * 2];
    config_dir(dir, COUNTOF(dir));
    dir_create_all(dir);
    path_join(path, COUNTOF(path), dir, L"config.toml");
    BOOL first_run = !file_exists(path);
    path_join(path, COUNTOF(path), dir, L"bufferless.log");
    log_init(path);
    logmsg("bufferless %s starting", BUFFERLESS_VERSION);

    app_run(first_run);

    MFShutdown();
    logmsg("bye");
    return 0;
}

void entry(void) {
    ExitProcess(run());
}
