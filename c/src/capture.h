// Video pipeline: desktop duplication -> cursor overlay -> GPU colour
// conversion/scaling -> hardware encoder -> ring buffer. Runs on its own
// thread at a fixed frame rate; if the screen hasn't changed we just re-encode
// the last frame.
#pragma once
#include "ring.h"

typedef struct {
    UINT32 monitor;
    UINT32 fps;
    UINT32 bitrate;
    UINT32 height; // output height, 0 = native
    BOOL cursor;
} VideoConfig;

typedef struct {
    VideoConfig cfg;
    Ring *ring;
    volatile LONG *stop;
    // Shown in the tray tooltip.
    SRWLOCK *status_lock;
    WCHAR *status;
    int status_cap;
} VideoThread;

DWORD WINAPI capture_thread(void *param);

typedef struct {
    WCHAR name[64];
    UINT32 width, height;
} MonitorInfo;
// Monitors in the order VideoConfig.monitor indexes them (caller frees).
MonitorInfo *capture_list_monitors(UINT32 *count);
