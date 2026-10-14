#pragma once
#include "ring.h"

#define AUDIO_RATE 48000
#define AUDIO_CHANNELS 2
#define AUDIO_BITRATE 192000

// Write the clip as a faststart MP4. Returns the file size, or 0 and an error
// message in `error`.
UINT64 mux_write_mp4(const WCHAR *path, const Clip *clip, WCHAR *error, int error_cap);
