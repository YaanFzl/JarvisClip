// Minimal MP4 writer for H.264 + AAC-LC. Writes the moov box before mdat
// ("faststart") so clips start playing immediately when uploaded somewhere.
#include "mux.h"

#define AAC_FRAME 1024
#define MOVIE_TIMESCALE 1000
#define VIDEO_TIMESCALE 90000

// ---------------------------------------------------------------------------
// Annex B helpers

// Find the next NAL unit at or after *pos. Returns FALSE when there are none.
static BOOL next_nal(const BYTE *d, UINT32 len, UINT32 *pos, const BYTE **nal, UINT32 *nal_len) {
    UINT32 i = *pos;
    while (i + 3 <= len && !(d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1)) i++;
    if (i + 3 > len) return FALSE;
    UINT32 start = i + 3, end = start;
    while (end + 3 <= len && !(d[end] == 0 && d[end + 1] == 0 && d[end + 2] == 1)) end++;
    if (end + 3 > len) end = len;
    *pos = end;
    // A 4-byte start code leaves a zero at the end of the previous NAL.
    while (end > start && d[end - 1] == 0) end--;
    *nal = d + start;
    *nal_len = end - start;
    return TRUE;
}

static int nal_type(const BYTE *nal, UINT32 len) {
    return len ? nal[0] & 0x1f : 0;
}

// NALs that belong in the sample data (parameter sets live in avcC, and access
// unit delimiters are pointless in MP4).
static BOOL keep_nal(const BYTE *nal, UINT32 len) {
    int t = nal_type(nal, len);
    return len && t != 7 && t != 8 && t != 9;
}

static UINT32 avcc_size(const Packet *p) {
    UINT32 pos = 0, size = 0, n;
    const BYTE *nal;
    while (next_nal(p->data, p->len, &pos, &nal, &n))
        if (keep_nal(nal, n)) size += 4 + n;
    return size;
}

static void write_avcc(const Packet *p, Writer *w) {
    UINT32 pos = 0, n;
    const BYTE *nal;
    while (next_nal(p->data, p->len, &pos, &nal, &n)) {
        if (!keep_nal(nal, n)) continue;
        BYTE len[4] = {(BYTE)(n >> 24), (BYTE)(n >> 16), (BYTE)(n >> 8), (BYTE)n};
        writer_put(w, len, 4);
        writer_put(w, nal, n);
    }
}

static BOOL find_param_sets_in(const BYTE *d, UINT32 len, const BYTE **sps, UINT32 *sps_len, const BYTE **pps,
                               UINT32 *pps_len) {
    UINT32 pos = 0, n;
    const BYTE *nal;
    *sps = *pps = NULL;
    while (next_nal(d, len, &pos, &nal, &n)) {
        if (nal_type(nal, n) == 7 && !*sps) *sps = nal, *sps_len = n;
        if (nal_type(nal, n) == 8 && !*pps) *pps = nal, *pps_len = n;
    }
    return *sps && *pps;
}

static BOOL find_param_sets(const Clip *c, const BYTE **sps, UINT32 *sps_len, const BYTE **pps, UINT32 *pps_len) {
    for (UINT32 i = 0; i < c->video_len; i++)
        if (c->video[i]->key && find_param_sets_in(c->video[i]->data, c->video[i]->len, sps, sps_len, pps, pps_len))
            return TRUE;
    return c->format.seq_len && find_param_sets_in(c->format.seq_header, c->format.seq_len, sps, sps_len, pps, pps_len);
}

// ---------------------------------------------------------------------------
// Sorting (stable merge sort on 64-bit keys)

typedef struct {
    INT64 key;
    UINT32 index;
    BOOL video;
} Item;

static void sort_items(Item *a, UINT32 n) {
    if (n < 2) return;
    Item *tmp = mem_alloc(n * sizeof(Item));
    for (UINT32 width = 1; width < n; width *= 2) {
        for (UINT32 lo = 0; lo < n; lo += 2 * width) {
            UINT32 mid = MIN(lo + width, n), hi = MIN(lo + 2 * width, n), i = lo, j = mid, k = lo;
            while (i < mid && j < hi) tmp[k++] = a[j].key < a[i].key ? a[j++] : a[i++];
            while (i < mid) tmp[k++] = a[i++];
            while (j < hi) tmp[k++] = a[j++];
        }
        memcpy(a, tmp, n * sizeof(Item));
    }
    mem_free(tmp);
}

// ---------------------------------------------------------------------------
// Box building. Boxes are described by format strings, which keeps the layout
// readable and compiles to data instead of hundreds of calls:
//   b h w q  big-endian u8/u16/u32/u64 from the arguments (q takes a UINT64)
//   z        that many zero bytes
//   s        raw bytes (pointer, length)
//   M        the identity matrix
//   [        start a box (fourcc)
//   {        start a full box (fourcc, version, flags)
//   ]        end the innermost box

typedef struct {
    Buf buf;
    SIZE_T stack[16];
    int depth;
} Mp4;

static void emit(Mp4 *m, const char *spec, ...) {
    static const UINT32 matrix[9] = {0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000};
    Buf *b = &m->buf;
    va_list ap;
    va_start(ap, spec);
    for (; *spec; spec++) {
        switch (*spec) {
        case 'b': buf_u8(b, (BYTE)va_arg(ap, UINT32)); break;
        case 'h': buf_u16be(b, (UINT16)va_arg(ap, UINT32)); break;
        case 'w': buf_u32be(b, va_arg(ap, UINT32)); break;
        case 'q': buf_u64be(b, va_arg(ap, UINT64)); break;
        case 'z': buf_zeros(b, va_arg(ap, UINT32)); break;
        case 's': {
            const void *p = va_arg(ap, const void *);
            buf_push(b, p, va_arg(ap, UINT32));
            break;
        }
        case 'M':
            for (int i = 0; i < 9; i++) buf_u32be(b, matrix[i]);
            break;
        case '[':
        case '{':
            m->stack[m->depth++] = b->len;
            buf_u32be(b, 0);
            buf_push(b, va_arg(ap, const char *), 4);
            if (*spec == '{') {
                UINT32 version = va_arg(ap, UINT32);
                buf_u32be(b, version << 24 | va_arg(ap, UINT32));
            }
            break;
        case ']': {
            SIZE_T start = m->stack[--m->depth];
            UINT32 size = (UINT32)(b->len - start);
            BYTE *p = b->data + start;
            p[0] = (BYTE)(size >> 24), p[1] = (BYTE)(size >> 16), p[2] = (BYTE)(size >> 8), p[3] = (BYTE)size;
            break;
        }
        }
    }
    va_end(ap);
}

typedef struct {
    UINT32 count;
    INT32 value;
} Run;

typedef struct {
    UINT32 id, timescale, n;
    UINT32 *sizes;
    UINT64 *offsets;
    Run *stts, *ctts; // run-length encoded durations / composition offsets
    UINT32 stts_len, ctts_len;
    UINT32 *sync; // 1-based sample numbers of sync samples (video only)
    UINT32 sync_len;
    UINT64 duration;
    UINT64 start_offset; // delay before the first sample, movie timescale
} Track;

static void push_run(Run *runs, UINT32 *len, INT32 value) {
    if (*len && runs[*len - 1].value == value) runs[*len - 1].count++;
    else runs[(*len)++] = (Run){1, value};
}

static void push_duration(Track *t, UINT32 d) {
    push_run(t->stts, &t->stts_len, (INT32)d);
    t->duration += d;
}

static UINT32 movie_duration(const Track *t) {
    return (UINT32)(t->duration * MOVIE_TIMESCALE / t->timescale);
}

static void write_runs(Mp4 *m, const char *kind, UINT32 version, const Run *runs, UINT32 n) {
    emit(m, "{w", kind, version, 0, n);
    for (UINT32 i = 0; i < n; i++) emit(m, "ww", runs[i].count, runs[i].value);
    emit(m, "]");
}

static void write_stbl_tables(Mp4 *m, const Track *t) {
    write_runs(m, "stts", 0, t->stts, t->stts_len);
    if (t->ctts_len) write_runs(m, "ctts", 1, t->ctts, t->ctts_len); // version 1: signed offsets
    if (t->id == 1) {
        emit(m, "{w", "stss", 0, 0, t->sync_len);
        for (UINT32 i = 0; i < t->sync_len; i++) emit(m, "w", t->sync[i]);
        emit(m, "]");
    }
    // One sample per chunk keeps the bookkeeping trivial.
    emit(m, "{wwww]{ww", "stsc", 0, 0, 1, 1, 1, 1, "stsz", 0, 0, 0, t->n);
    for (UINT32 i = 0; i < t->n; i++) emit(m, "w", t->sizes[i]);
    emit(m, "]{w", "co64", 0, 0, t->n);
    for (UINT32 i = 0; i < t->n; i++) emit(m, "q", t->offsets[i]);
    emit(m, "]");
}

static void write_header(Mp4 *m, const Track *t, UINT32 width, UINT32 height) {
    // Flags: enabled | in movie. Then layer, alternate group, volume.
    emit(m, "{wwwwwzhhhhMww]", "tkhd", 0, 3, 0, 0, t->id, 0, movie_duration(t) + (UINT32)t->start_offset, 8, 0, 0,
         t->id == 2 ? 0x0100 : 0, 0, width << 16, height << 16);
    // An empty edit delays the track.
    if (t->start_offset)
        emit(m, "[{wwwwwww]]", "edts", "elst", 0, 0, 2, (UINT32)t->start_offset, 0xffffffff, 0x00010000,
             movie_duration(t), 0, 0x00010000);
}

static void write_media_header(Mp4 *m, const Track *t, const char *handler, const char *name) {
    // Language "und".
    emit(m, "[{wwwwhh]{wszs]", "mdia", "mdhd", 0, 0, 0, 0, t->timescale, (UINT32)t->duration, 0x55c4, 0, "hdlr", 0, 0,
         0, handler, 4, 12, name, lstrlenA(name) + 1);
}

static void write_dinf(Mp4 *m) {
    emit(m, "[{w{]]]", "dinf", "dref", 0, 0, 1, "url ", 0, 1);
}

static void write_moov(Mp4 *m, const Clip *c, const BYTE *sps, UINT32 sps_len, const BYTE *pps, UINT32 pps_len,
                       const Track *video, const Track *audio) {
    const VideoFormat *f = &c->format;
    UINT32 duration = movie_duration(video);
    if (audio) duration = MAX(duration, movie_duration(audio) + (UINT32)audio->start_offset);
    // Rate 1.0, volume 1.0, next track id 3.
    emit(m, "[{wwwwwhzMzw]", "moov", "mvhd", 0, 0, 0, 0, MOVIE_TIMESCALE, duration, 0x00010000, 0x0100, 10, 24, 3);

    // --- video track
    static const BYTE compressor[32] = {10, 'b', 'u', 'f', 'f', 'e', 'r', 'l', 'e', 's', 's'};
    emit(m, "[", "trak");
    write_header(m, video, f->width, f->height);
    write_media_header(m, video, "vide", "VideoHandler");
    emit(m, "[{z]", "minf", "vmhd", 0, 1, 8);
    write_dinf(m);
    // avc1 sample entry: data reference 1, size, 72 dpi, 1 frame, depth 24.
    emit(m, "[{w[zhzhhwwwhshh", "stbl", "stsd", 0, 0, 1, "avc1", 6, 1, 16, f->width, f->height, 0x00480000,
         0x00480000, 0, 1, compressor, 32, 0x0018, 0xffff);
    // avcC with 4-byte NAL lengths and one SPS and PPS; then BT.709 limited
    // range, matching what the video processor outputs.
    emit(m, "[bbbbbbhsbhs][shhhb]]]", "avcC", 1, sps[1], sps[2], sps[3], 0xff, 0xe1, sps_len, sps, sps_len, 1,
         pps_len, pps, pps_len, "colr", "nclx", 4, 1, 1, 1, 0);
    write_stbl_tables(m, video);
    emit(m, "]]]]"); // stbl, minf, mdia, trak

    // --- audio track
    if (audio) {
        emit(m, "[", "trak");
        write_header(m, audio, 0, 0);
        write_media_header(m, audio, "soun", "SoundHandler");
        emit(m, "[{w]", "minf", "smhd", 0, 0, 0);
        write_dinf(m);
        emit(m, "[{w[zhzhhww", "stbl", "stsd", 0, 0, 1, "mp4a", 6, 1, 8, AUDIO_CHANNELS, 16, 0, AUDIO_RATE << 16);
        // ES_Descriptor > DecoderConfigDescriptor (MPEG-4 audio) >
        // DecoderSpecificInfo (AudioSpecificConfig: AAC-LC, 48 kHz) + SLConfig.
        emit(m, "{bbhbbbbbbhwwbbhbbb]]]", "esds", 0, 0, 0x03, 25, 2, 0, 0x04, 17, 0x40, 0x15, 0, 0, AUDIO_BITRATE,
             AUDIO_BITRATE, 0x05, 2, 2 << 11 | 3 << 7 | AUDIO_CHANNELS << 3, 0x06, 1, 0x02);
        write_stbl_tables(m, audio);
        emit(m, "]]]]");
    }
    emit(m, "]"); // moov
}

// Timestamp relative to the clip start, in track timescale units.
static UINT64 to_scale(INT64 t, UINT32 scale) {
    t = MAX(t, 0);
    // Split to avoid overflowing 64 bits.
    return (UINT64)(t / SECOND * scale + t % SECOND * scale / SECOND);
}

static UINT64 assign_offsets(UINT64 base, const Item *order, UINT32 n, Track *video, Track *audio) {
    UINT64 off = base;
    for (UINT32 i = 0; i < n; i++) {
        Track *t = order[i].video ? video : audio;
        t->offsets[order[i].index] = off;
        off += t->sizes[order[i].index];
    }
    return off - base;
}

static void track_alloc(Track *t, UINT32 id, UINT32 timescale, UINT32 n) {
    memset(t, 0, sizeof(*t));
    t->id = id, t->timescale = timescale, t->n = n;
    t->sizes = mem_alloc(n * sizeof(UINT32) + 4);
    t->offsets = mem_alloc(n * sizeof(UINT64) + 8);
    t->stts = mem_alloc(n * sizeof(Run) + sizeof(Run));
    t->ctts = mem_alloc(n * sizeof(Run) + sizeof(Run));
    t->sync = mem_alloc(n * sizeof(UINT32) + 4);
}

static void track_free(Track *t) {
    mem_free(t->sizes), mem_free(t->offsets), mem_free(t->stts), mem_free(t->ctts), mem_free(t->sync);
}

UINT64 mux_write_mp4(const WCHAR *path, const Clip *c, WCHAR *error, int error_cap) {
    const BYTE *sps, *pps;
    UINT32 sps_len, pps_len;
    if (!c->video_len) return wfmt(error, error_cap, "empty clip"), 0;
    if (!find_param_sets(c, &sps, &sps_len, &pps, &pps_len)) return wfmt(error, error_cap, "no SPS/PPS in stream"), 0;
    if (sps_len < 4) return wfmt(error, error_cap, "bad SPS"), 0;

    INT64 t0 = c->video[0]->pts;
    for (UINT32 i = 1; i < c->video_len; i++) t0 = MIN(t0, c->video[i]->pts);

    // Video sample table. Samples are stored in decode order; decode times are
    // the sorted presentation times, which also covers encoders that emit
    // B-frames (their reordering goes into ctts). Durations come from timestamp
    // deltas so dropped frames keep the clip in sync with audio.
    Track video, audio_track, *audio = NULL;
    track_alloc(&video, 1, VIDEO_TIMESCALE, c->video_len);
    Item *dts = mem_alloc(c->video_len * sizeof(Item));
    for (UINT32 i = 0; i < c->video_len; i++) dts[i].key = (INT64)to_scale(c->video[i]->pts - t0, VIDEO_TIMESCALE);
    sort_items(dts, c->video_len);
    UINT32 nominal = VIDEO_TIMESCALE / MAX(c->format.fps, 1);
    BOOL reordered = FALSE;
    for (UINT32 i = 0; i < c->video_len; i++) {
        INT64 pts = (INT64)to_scale(c->video[i]->pts - t0, VIDEO_TIMESCALE);
        video.sizes[i] = avcc_size(c->video[i]);
        if (c->video[i]->key) video.sync[video.sync_len++] = i + 1;
        UINT32 d = i + 1 < c->video_len ? (UINT32)(dts[i + 1].key - dts[i].key) : nominal;
        push_duration(&video, MAX(d, 1));
        if (pts != dts[i].key) reordered = TRUE;
    }
    if (reordered)
        for (UINT32 i = 0; i < c->video_len; i++)
            push_run(video.ctts, &video.ctts_len,
                     (INT32)((INT64)to_scale(c->video[i]->pts - t0, VIDEO_TIMESCALE) - dts[i].key));
    mem_free(dts);

    if (c->audio_len) {
        audio = &audio_track;
        track_alloc(audio, 2, AUDIO_RATE, c->audio_len);
        audio->start_offset = to_scale(c->audio[0]->pts - t0, MOVIE_TIMESCALE);
        for (UINT32 i = 0; i < c->audio_len; i++) {
            audio->sizes[i] = c->audio[i]->len;
            push_duration(audio, AAC_FRAME);
        }
    }

    // Interleave samples by timestamp.
    UINT32 n = c->video_len + c->audio_len;
    Item *order = mem_alloc(n * sizeof(Item));
    for (UINT32 i = 0; i < c->video_len; i++) order[i] = (Item){c->video[i]->pts, i, TRUE};
    for (UINT32 i = 0; i < c->audio_len; i++) order[c->video_len + i] = (Item){c->audio[i]->pts, i, FALSE};
    sort_items(order, n);

    Mp4 ftyp = {0};
    emit(&ftyp, "[sws]", "ftyp", "isom", 4, 0x200, "isomiso2avc1mp41", 16);

    // co64 entries are fixed size, so the moov size doesn't depend on the
    // offsets: build once to measure, then again with real offsets.
    UINT64 payload = assign_offsets(0, order, n, &video, audio);
    Mp4 moov = {0};
    write_moov(&moov, c, sps, sps_len, pps, pps_len, &video, audio);
    const UINT64 mdat_header = 16;
    UINT64 base = ftyp.buf.len + moov.buf.len + mdat_header;
    assign_offsets(base, order, n, &video, audio);
    moov.buf.len = 0;
    write_moov(&moov, c, sps, sps_len, pps, pps_len, &video, audio);

    UINT64 size = 0;
    Writer w;
    if (writer_open(&w, path, 1 << 20)) {
        writer_put(&w, ftyp.buf.data, ftyp.buf.len);
        writer_put(&w, moov.buf.data, moov.buf.len);
        Mp4 hdr = {0};
        emit(&hdr, "wsq", 1, "mdat", 4, mdat_header + payload);
        writer_put(&w, hdr.buf.data, hdr.buf.len);
        buf_free(&hdr.buf);
        for (UINT32 i = 0; i < n; i++) {
            if (order[i].video) write_avcc(c->video[order[i].index], &w);
            else writer_put(&w, c->audio[order[i].index]->data, c->audio[order[i].index]->len);
        }
        if (writer_close(&w)) size = base + payload;
        else wfmt(error, error_cap, "couldn't write the file");
    } else {
        hr_text(HRESULT_FROM_WIN32(GetLastError()), error, error_cap);
    }

    buf_free(&ftyp.buf), buf_free(&moov.buf), mem_free(order);
    track_free(&video);
    if (audio) track_free(audio);
    return size;
}
