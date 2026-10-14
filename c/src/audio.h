// Audio pipeline: WASAPI system loopback + optional microphone, mixed on the
// shared clock and encoded to AAC-LC. Every WASAPI packet carries a QPC
// timestamp, which is what keeps audio in sync with video.
#pragma once
#include "ring.h"

typedef struct {
    BOOL system;
    BOOL mic;
    WCHAR mic_device[256]; // endpoint id, "" = default microphone
    float mic_volume;
    Ring *ring;
    volatile LONG *stop;
} AudioThread;

DWORD WINAPI audio_thread(void *param);

typedef struct {
    WCHAR *id, *name;
} DeviceInfo;
// Active microphones for the settings window (free with audio_free_devices).
DeviceInfo *audio_list_microphones(UINT32 *count);
void audio_free_devices(DeviceInfo *list, UINT32 count);
