#pragma once
#include "rt.h"

typedef struct {
    UINT32 hotkey_modifiers; // RegisterHotKey MOD_* flags
    UINT32 hotkey_key;       // virtual key code
    UINT32 replay_seconds;
    UINT32 monitor;
    UINT32 resolution; // output height, 0 = native
    UINT32 fps;
    UINT32 bitrate_mbps;
    BOOL capture_cursor;
    BOOL system_audio;
    BOOL microphone;
    WCHAR *microphone_device; // endpoint id, "" = default microphone
    UINT32 microphone_volume;
    WCHAR *save_folder;
    BOOL save_sound;
    BOOL start_with_windows;
} Config;

void config_default(Config *c);
void config_load(Config *c);
BOOL config_save(const Config *c);
void config_copy(Config *dst, const Config *src);
void config_free(Config *c);
// Settings that require restarting the capture pipeline when changed.
BOOL config_capture_differs(const Config *a, const Config *b);
// Rough RAM needed for the replay buffer.
UINT32 config_memory_mb(const Config *c);
// %APPDATA%\bufferless
void config_dir(WCHAR *out, int cap);
