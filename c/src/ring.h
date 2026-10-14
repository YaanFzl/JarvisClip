// In-memory replay buffer of *encoded* packets. Saving a clip never
// re-encodes; it just takes references to packets from the last keyframe
// before the requested start.
#pragma once
#include "rt.h"

typedef struct {
    volatile LONG refs;
    INT64 pts; // presentation time on the shared clock, 100ns units
    BOOL key;
    UINT32 len;
    BYTE data[1];
} Packet;

Packet *packet_new(INT64 pts, BOOL key, const void *data, UINT32 len);
void packet_release(Packet *p);

typedef struct {
    UINT32 width, height, fps;
    BYTE *seq_header; // Annex B SPS/PPS from the encoder's output type, if any
    UINT32 seq_len;
} VideoFormat;

typedef struct {
    VideoFormat format;
    Packet **video, **audio;
    UINT32 video_len, audio_len;
} Clip;
void clip_free(Clip *c);

typedef struct {
    Packet **items;
    UINT32 cap, head, len;
} Queue;

typedef struct {
    SRWLOCK lock;
    Queue video, audio;
    VideoFormat format;
    BOOL has_format;
    INT64 keep; // history to keep, 100ns units
    UINT64 bytes;
} Ring;

// All functions take the lock themselves.
void ring_init(Ring *r, UINT32 seconds);
void ring_set_keep(Ring *r, UINT32 seconds);
// Packets from an encoder with a different format can't share a file, so the
// buffer is dropped when the format changes.
void ring_set_format(Ring *r, UINT32 width, UINT32 height, UINT32 fps, const BYTE *seq, UINT32 seq_len);
void ring_set_seq_header(Ring *r, const BYTE *seq, UINT32 seq_len);
void ring_push_video(Ring *r, Packet *p);
void ring_push_audio(Ring *r, Packet *p);
void ring_clear(Ring *r);
void ring_stats(Ring *r, UINT32 *seconds, UINT64 *bytes);
// The last `seconds` of footage, starting at the closest keyframe at or
// before the requested start. FALSE if there's nothing to save yet.
BOOL ring_snapshot(Ring *r, UINT32 seconds, Clip *out);
