#pragma once
#include "config.h"

#define WM_SHOW_SETTINGS (WM_APP + 3) // posted by a second instance
#define WINDOW_CLASS L"BufferlessTrayWindow"

int app_run(BOOL first_run);
// Called by the settings window when the user hits Save (takes ownership).
void app_apply_config(Config *cfg);
void app_current_config(Config *out);

// settings.c
void settings_open(const Config *cfg);
HWND settings_window(void);
