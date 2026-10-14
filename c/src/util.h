#pragma once
#include "rt.h"

// The Bufferless mark ("afterimage"), rendered at runtime.
HICON icon_tile(int size);                          // app icon: mark on the indigo tile
HICON icon_glyph(int size, COLORREF rgb, float opacity); // bare mark for the tray

void play_saved_sound(void);
void play_error_sound(void);
void timestamp(WCHAR *out, int cap);          // "2026-10-02 13-45-12"
void foreground_app_name(WCHAR *out, int cap); // usually the game, for clip names
void hotkey_name(UINT32 modifiers, UINT32 vk, WCHAR *out, int cap);
void set_autostart(BOOL enable);
BOOL taskbar_is_light(void);
