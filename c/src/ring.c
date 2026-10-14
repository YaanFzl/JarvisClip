#include "ring.h"

Packet *packet_new(INT64 pts, BOOL key, const void *data, UINT32 len) {
    Packet *p = HeapAlloc(GetProcessHeap(), 0, sizeof(Packet) + len);
    p->refs = 1;
    p->pts = pts;
    p->key = key;
    p->len = len;
    memcpy(p->data, data, len);
    return p;
}

static Packet *retain(Packet *p) {
    InterlockedIncrement(&p->refs);
    return p;
}

void packet_release(Packet *p) {
    if (p && InterlockedDecrement(&p->refs) == 0) mem_free(p);
}

// --- circular queue of packet pointers

static Packet *q_at(Queue *q, UINT32 i) {
    return q->items[(q->head + i) % q->cap];
}

static void q_push(Queue *q, Packet *p) {
    if (q->len == q->cap) {
        UINT32 cap = q->cap ? q->cap * 2 : 256;
        Packet **items = mem_alloc(cap * sizeof(Packet *));
        for (UINT32 i = 0; i < q->len; i++) items[i] = q_at(q, i);
        mem_free(q->items);
        q->items = items, q->cap = cap, q->head = 0;
    }
    q->items[(q->head + q->len++) % q->cap] = p;
}

static Packet *q_pop(Queue *q) {
    Packet *p = q->items[q->head];
    q->head = (q->head + 1) % q->cap;
    q->len--;
    return p;
}

// --- ring

void ring_init(Ring *r, UINT32 seconds) {
    memset(r, 0, sizeof(*r));
    InitializeSRWLock(&r->lock);
    // A few seconds of slack so we can always start on a keyframe.
    r->keep = (seconds + 3) * SECOND;
}

static void pop_video(Ring *r) {
    Packet *p = q_pop(&r->video);
    r->bytes -= p->len;
    packet_release(p);
}

static void trim(Ring *r) {
    INT64 newest = 0;
    if (r->video.len) newest = q_at(&r->video, r->video.len - 1)->pts;
    if (r->audio.len) newest = MAX(newest, q_at(&r->audio, r->audio.len - 1)->pts);
    INT64 cutoff = newest - r->keep;
    // The buffer must always start on a keyframe.
    while (r->video.len && !q_at(&r->video, 0)->key) pop_video(r);
    // Drop whole GOPs while the next keyframe is still older than the cutoff.
    for (;;) {
        UINT32 next = 1;
        while (next < r->video.len && !q_at(&r->video, next)->key) next++;
        if (next >= r->video.len || q_at(&r->video, next)->pts > cutoff) break;
        while (next--) pop_video(r);
    }
    INT64 audio_cutoff = r->video.len ? q_at(&r->video, 0)->pts : cutoff;
    while (r->audio.len && q_at(&r->audio, 0)->pts < audio_cutoff) {
        Packet *p = q_pop(&r->audio);
        r->bytes -= p->len;
        packet_release(p);
    }
}

static void clear_locked(Ring *r) {
    while (r->video.len) pop_video(r);
    while (r->audio.len) {
        Packet *p = q_pop(&r->audio);
        packet_release(p);
    }
    r->bytes = 0;
}

void ring_set_keep(Ring *r, UINT32 seconds) {
    AcquireSRWLockExclusive(&r->lock);
    r->keep = (seconds + 3) * SECOND;
    trim(r);
    ReleaseSRWLockExclusive(&r->lock);
}

static void set_seq(VideoFormat *f, const BYTE *seq, UINT32 len) {
    mem_free(f->seq_header);
    f->seq_header = len ? mem_alloc(len) : NULL;
    if (len) memcpy(f->seq_header, seq, len);
    f->seq_len = len;
}

void ring_set_format(Ring *r, UINT32 width, UINT32 height, UINT32 fps, const BYTE *seq, UINT32 seq_len) {
    AcquireSRWLockExclusive(&r->lock);
    VideoFormat *f = &r->format;
    BOOL same = r->has_format && f->width == width && f->height == height && f->fps == fps && f->seq_len == seq_len &&
                (!seq_len || memcmp(f->seq_header, seq, seq_len) == 0);
    if (!same) clear_locked(r);
    f->width = width, f->height = height, f->fps = fps;
    set_seq(f, seq, seq_len);
    r->has_format = TRUE;
    ReleaseSRWLockExclusive(&r->lock);
}

void ring_set_seq_header(Ring *r, const BYTE *seq, UINT32 seq_len) {
    AcquireSRWLockExclusive(&r->lock);
    if (r->has_format) set_seq(&r->format, seq, seq_len);
    ReleaseSRWLockExclusive(&r->lock);
}

void ring_push_video(Ring *r, Packet *p) {
    AcquireSRWLockExclusive(&r->lock);
    r->bytes += p->len;
    q_push(&r->video, p);
    trim(r);
    ReleaseSRWLockExclusive(&r->lock);
}

void ring_push_audio(Ring *r, Packet *p) {
    AcquireSRWLockExclusive(&r->lock);
    r->bytes += p->len;
    q_push(&r->audio, p);
    trim(r);
    ReleaseSRWLockExclusive(&r->lock);
}

void ring_clear(Ring *r) {
    AcquireSRWLockExclusive(&r->lock);
    clear_locked(r);
    ReleaseSRWLockExclusive(&r->lock);
}

void ring_stats(Ring *r, UINT32 *seconds, UINT64 *bytes) {
    AcquireSRWLockShared(&r->lock);
    *seconds = r->video.len ? (UINT32)((q_at(&r->video, r->video.len - 1)->pts - q_at(&r->video, 0)->pts) / SECOND) : 0;
    *bytes = r->bytes;
    ReleaseSRWLockShared(&r->lock);
}

BOOL ring_snapshot(Ring *r, UINT32 seconds, Clip *out) {
    memset(out, 0, sizeof(*out));
    AcquireSRWLockExclusive(&r->lock);
    Queue *v = &r->video;
    if (!r->has_format || !v->len) {
        ReleaseSRWLockExclusive(&r->lock);
        return FALSE;
    }
    INT64 newest = q_at(v, v->len - 1)->pts, start = newest - (INT64)seconds * SECOND;
    // Last keyframe at or before the start, else the first keyframe.
    UINT32 first = v->len;
    for (UINT32 i = v->len; i-- > 0;)
        if (q_at(v, i)->key && q_at(v, i)->pts <= start) {
            first = i;
            break;
        }
    if (first == v->len)
        for (UINT32 i = 0; i < v->len; i++)
            if (q_at(v, i)->key) {
                first = i;
                break;
            }
    if (first == v->len) {
        ReleaseSRWLockExclusive(&r->lock);
        return FALSE;
    }
    out->format = r->format;
    out->format.seq_header = NULL;
    out->format.seq_len = 0;
    set_seq(&out->format, r->format.seq_header, r->format.seq_len);
    out->video = mem_alloc((v->len - first) * sizeof(Packet *));
    for (UINT32 i = first; i < v->len; i++) out->video[out->video_len++] = retain(q_at(v, i));
    INT64 t0 = out->video[0]->pts;
    out->audio = mem_alloc((r->audio.len + 1) * sizeof(Packet *));
    for (UINT32 i = 0; i < r->audio.len; i++) {
        Packet *p = q_at(&r->audio, i);
        if (p->pts >= t0 && p->pts <= newest) out->audio[out->audio_len++] = retain(p);
    }
    ReleaseSRWLockExclusive(&r->lock);
    return TRUE;
}

void clip_free(Clip *c) {
    for (UINT32 i = 0; i < c->video_len; i++) packet_release(c->video[i]);
    for (UINT32 i = 0; i < c->audio_len; i++) packet_release(c->audio[i]);
    mem_free(c->video), mem_free(c->audio), mem_free(c->format.seq_header);
    memset(c, 0, sizeof(*c));
}
