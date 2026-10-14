#include "audio.h"
#include "mux.h"

#include <audioclient.h>
#include <mfapi.h>
#include <mferror.h>
#include <mftransform.h>
#include <mmdeviceapi.h>
#include <mmreg.h>
#include <propsys.h>
#include <ks.h>
#include <ksmedia.h>

#define CH AUDIO_CHANNELS
#define FRAME 1024                 // AAC frame size in samples per channel
#define MIX_DELAY (AUDIO_RATE / 10) // run behind real time so late packets still make it in
#define RESYNC (AUDIO_RATE / 100)   // timestamp drift tolerated before resyncing (10ms)

// Declared by the SDK headers but not shipped in any import library.
const IID IID_IMMDeviceEnumerator = {0xa95664d2, 0x9614, 0x4f35, {0xa7, 0x46, 0xde, 0x8d, 0xb6, 0x36, 0x17, 0xe6}};
const CLSID CLSID_MMDeviceEnumerator = {0xbcde0395, 0xe52f, 0x467c, {0x8e, 0x3d, 0xc4, 0x57, 0x92, 0x91, 0x69, 0x2e}};
const IID IID_IAudioClient = {0x1cb9ad4c, 0xdbfa, 0x4c32, {0xb1, 0x78, 0xc2, 0xf5, 0x68, 0xa7, 0x03, 0xb2}};
const IID IID_IAudioCaptureClient = {0xc8adbd64, 0xe71e, 0x48a0, {0xa4, 0xde, 0x18, 0x5c, 0x39, 0x5c, 0xd3, 0x17}};
static const GUID FLOAT_SUBTYPE = {0x00000003, 0x0000, 0x0010, {0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71}};
static const CLSID CLSID_AACEncoder ={0x93af0c51, 0x2275, 0x45d2, {0xa3, 0x5b, 0xf2, 0xba, 0x21, 0xca, 0xed, 0x00}};
static const PROPERTYKEY KEY_FriendlyName = {{0xa45c254e, 0xdf1c, 0x4efd, {0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0}},
                                             14};

static IMMDeviceEnumerator *enumerator(void) {
    IMMDeviceEnumerator *e = NULL;
    CoCreateInstance(&CLSID_MMDeviceEnumerator, NULL, CLSCTX_ALL, &IID_IMMDeviceEnumerator, (void **)&e);
    return e;
}

static WCHAR *device_id(IMMDevice *d) {
    WCHAR *id = NULL, *copy;
    if (FAILED(IMMDevice_GetId(d, &id))) return wdup(L"");
    copy = wdup(id);
    CoTaskMemFree(id);
    return copy;
}

DeviceInfo *audio_list_microphones(UINT32 *count) {
    *count = 0;
    IMMDeviceEnumerator *e = enumerator();
    IMMDeviceCollection *list = NULL;
    if (!e || FAILED(IMMDeviceEnumerator_EnumAudioEndpoints(e, eCapture, DEVICE_STATE_ACTIVE, &list))) {
        RELEASE(e);
        return NULL;
    }
    UINT n = 0;
    IMMDeviceCollection_GetCount(list, &n);
    DeviceInfo *out = mem_alloc((n + 1) * sizeof(DeviceInfo));
    for (UINT i = 0; i < n; i++) {
        IMMDevice *d = NULL;
        if (FAILED(IMMDeviceCollection_Item(list, i, &d))) continue;
        IPropertyStore *props = NULL;
        PROPVARIANT v;
        PropVariantInit(&v);
        WCHAR *name = NULL;
        if (SUCCEEDED(IMMDevice_OpenPropertyStore(d, STGM_READ, &props)) &&
            SUCCEEDED(IPropertyStore_GetValue(props, &KEY_FriendlyName, &v)) && v.vt == VT_LPWSTR)
            name = wdup(v.pwszVal);
        PropVariantClear(&v);
        RELEASE(props);
        out[*count].id = device_id(d);
        out[*count].name = name ? name : wdup(L"Unknown device");
        (*count)++;
        RELEASE(d);
    }
    RELEASE(list);
    RELEASE(e);
    return out;
}

void audio_free_devices(DeviceInfo *list, UINT32 count) {
    for (UINT32 i = 0; i < count; i++) mem_free(list[i].id), mem_free(list[i].name);
    mem_free(list);
}

static WCHAR *default_device(BOOL mic) {
    IMMDeviceEnumerator *e = enumerator();
    IMMDevice *d = NULL;
    WCHAR *id = NULL;
    if (e && SUCCEEDED(IMMDeviceEnumerator_GetDefaultAudioEndpoint(e, mic ? eCapture : eRender, eConsole, &d)))
        id = device_id(d);
    RELEASE(d);
    RELEASE(e);
    return id;
}

// ---------------------------------------------------------------------------
// Sources

typedef struct {
    BOOL mic;
    WCHAR *device; // what we opened, to notice when the default changes; NULL = reopen
    IAudioClient *client;
    IAudioCaptureClient *capture;
    float gain;
    // Interleaved stereo samples; fifo[0] is frame number `start`.
    float *fifo;
    UINT32 cap, head, len; // circular, in samples
    INT64 start;
} Source;

static void fifo_push(Source *s, float v) {
    if (s->len == s->cap) {
        UINT32 cap = s->cap ? s->cap * 2 : 48000;
        float *data = mem_alloc(cap * sizeof(float));
        for (UINT32 i = 0; i < s->len; i++) data[i] = s->fifo[(s->head + i) % s->cap];
        mem_free(s->fifo);
        s->fifo = data, s->cap = cap, s->head = 0;
    }
    s->fifo[(s->head + s->len++) % s->cap] = v;
}

static void fifo_drop(Source *s, UINT32 n) {
    n = MIN(n, s->len);
    if (s->cap) s->head = (s->head + n) % s->cap;
    s->len -= n;
}

static INT64 source_end(Source *s) {
    return s->start + s->len / CH;
}

static void source_close(Source *s) {
    if (s->client) IAudioClient_Stop(s->client);
    RELEASE(s->capture);
    RELEASE(s->client);
    mem_free(s->device), mem_free(s->fifo);
    memset(s, 0, sizeof(*s));
}

static HRESULT source_open(Source *s, BOOL mic, const AudioThread *cfg) {
    memset(s, 0, sizeof(*s));
    s->mic = mic;
    IMMDeviceEnumerator *e = enumerator();
    IMMDevice *dev = NULL;
    HRESULT hr = e ? S_OK : E_FAIL;
    if (SUCCEEDED(hr)) {
        if (mic && cfg->mic_device[0]) hr = IMMDeviceEnumerator_GetDevice(e, cfg->mic_device, &dev);
        else hr = IMMDeviceEnumerator_GetDefaultAudioEndpoint(e, mic ? eCapture : eRender, eConsole, &dev);
    }
    if (SUCCEEDED(hr)) hr = IMMDevice_Activate(dev, &IID_IAudioClient, CLSCTX_ALL, NULL, (void **)&s->client);
    if (SUCCEEDED(hr)) {
        // Ask WASAPI to hand us 48k stereo float whatever the device runs at.
        WAVEFORMATEXTENSIBLE f = {0};
        f.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE;
        f.Format.nChannels = CH;
        f.Format.nSamplesPerSec = AUDIO_RATE;
        f.Format.nAvgBytesPerSec = AUDIO_RATE * 4 * CH;
        f.Format.nBlockAlign = 4 * CH;
        f.Format.wBitsPerSample = 32;
        f.Format.cbSize = sizeof(f) - sizeof(WAVEFORMATEX);
        f.Samples.wValidBitsPerSample = 32;
        f.dwChannelMask = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT;
        f.SubFormat = FLOAT_SUBTYPE;
        DWORD flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if (!mic) flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        hr = IAudioClient_Initialize(s->client, AUDCLNT_SHAREMODE_SHARED, flags, SECOND / 5, 0, &f.Format, NULL);
    }
    if (SUCCEEDED(hr)) hr = IAudioClient_GetService(s->client, &IID_IAudioCaptureClient, (void **)&s->capture);
    if (SUCCEEDED(hr)) hr = IAudioClient_Start(s->client);
    if (SUCCEEDED(hr)) {
        s->device = device_id(dev);
        s->gain = mic ? cfg->mic_volume : 1.0f;
    }
    RELEASE(dev);
    RELEASE(e);
    if (FAILED(hr)) source_close(s);
    return hr;
}

// Drain everything WASAPI has for us into the fifo, placed on the timeline.
static HRESULT source_read(Source *s, INT64 origin) {
    UINT32 packet = 0;
    HRESULT hr;
    while (SUCCEEDED(hr = IAudioCaptureClient_GetNextPacketSize(s->capture, &packet)) && packet > 0) {
        BYTE *data;
        UINT32 frames;
        DWORD flags;
        UINT64 qpc = 0;
        if (FAILED(hr = IAudioCaptureClient_GetBuffer(s->capture, &data, &frames, &flags, NULL, &qpc))) return hr;
        INT64 at = qpc ? (INT64)qpc : clock_now();
        INT64 idx = (at - origin) / SECOND * AUDIO_RATE + (at - origin) % SECOND * AUDIO_RATE / SECOND;
        const float *samples = (flags & AUDCLNT_BUFFERFLAGS_SILENT) || !data ? NULL : (const float *)data;

        UINT32 skip = 0;
        if (!s->len) {
            s->start = idx;
        } else {
            INT64 gap = idx - source_end(s);
            if (gap > RESYNC) {
                for (INT64 i = 0; i < gap * CH; i++) fifo_push(s, 0);
            } else if (gap < -RESYNC) {
                // Overlap: drop the part of this packet we already have.
                skip = (UINT32)MIN(-gap, (INT64)frames);
            }
            // Otherwise it's jitter: append contiguously.
        }
        for (UINT32 f = skip; f < frames; f++)
            for (int c = 0; c < CH; c++) fifo_push(s, samples ? samples[f * CH + c] * s->gain : 0);
        if (FAILED(hr = IAudioCaptureClient_ReleaseBuffer(s->capture, frames))) return hr;
    }
    return hr;
}

// Add frames [pos, pos + FRAME) into `out`, consuming them.
static void source_mix(Source *s, INT64 pos, float *out) {
    // Throw away anything that arrived too late to be mixed.
    if (s->start < pos) {
        fifo_drop(s, (UINT32)MIN((pos - s->start) * CH, (INT64)s->len));
        s->start = pos;
    }
    INT64 offset = s->start - pos;
    if (offset >= FRAME) return;
    UINT32 take = (UINT32)MIN((FRAME - offset) * CH, (INT64)s->len);
    for (UINT32 i = 0; i < take; i++) out[offset * CH + i] += s->fifo[(s->head + i) % s->cap];
    fifo_drop(s, take);
    s->start += take / CH;
}

// ---------------------------------------------------------------------------
// AAC encoder

static HRESULT aac_open(IMFTransform **mft, DWORD *output_size) {
    IMFMediaType *in = NULL, *out = NULL;
    HRESULT hr = CoCreateInstance(&CLSID_AACEncoder, NULL, CLSCTX_INPROC_SERVER, &IID_IMFTransform, (void **)mft);
    if (SUCCEEDED(hr)) hr = MFCreateMediaType(&in);
    if (SUCCEEDED(hr)) {
        IMFMediaType_SetGUID(in, &MF_MT_MAJOR_TYPE, &MFMediaType_Audio);
        IMFMediaType_SetGUID(in, &MF_MT_SUBTYPE, &MFAudioFormat_PCM);
        IMFMediaType_SetUINT32(in, &MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
        IMFMediaType_SetUINT32(in, &MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE);
        IMFMediaType_SetUINT32(in, &MF_MT_AUDIO_NUM_CHANNELS, CH);
        IMFMediaType_SetUINT32(in, &MF_MT_AUDIO_BLOCK_ALIGNMENT, 2 * CH);
        IMFMediaType_SetUINT32(in, &MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_RATE * 2 * CH);
        hr = MFCreateMediaType(&out);
    }
    if (SUCCEEDED(hr)) {
        IMFMediaType_SetGUID(out, &MF_MT_MAJOR_TYPE, &MFMediaType_Audio);
        IMFMediaType_SetGUID(out, &MF_MT_SUBTYPE, &MFAudioFormat_AAC);
        IMFMediaType_SetUINT32(out, &MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
        IMFMediaType_SetUINT32(out, &MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE);
        IMFMediaType_SetUINT32(out, &MF_MT_AUDIO_NUM_CHANNELS, CH);
        IMFMediaType_SetUINT32(out, &MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_BITRATE / 8);
        IMFMediaType_SetUINT32(out, &MF_MT_AAC_PAYLOAD_TYPE, 0); // raw AAC frames
        hr = IMFTransform_SetInputType(*mft, 0, in, 0);
    }
    if (SUCCEEDED(hr)) hr = IMFTransform_SetOutputType(*mft, 0, out, 0);
    MFT_OUTPUT_STREAM_INFO info;
    if (SUCCEEDED(hr)) hr = IMFTransform_GetOutputStreamInfo(*mft, 0, &info);
    if (SUCCEEDED(hr)) {
        *output_size = MAX(info.cbSize, 4096);
        IMFTransform_ProcessMessage(*mft, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        IMFTransform_ProcessMessage(*mft, MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
    }
    RELEASE(in);
    RELEASE(out);
    return hr;
}

static HRESULT aac_encode(IMFTransform *mft, DWORD output_size, const INT16 *pcm, INT64 pts, Ring *ring) {
    IMFMediaBuffer *buffer = NULL;
    IMFSample *sample = NULL;
    DWORD bytes = FRAME * CH * 2;
    BYTE *p;
    HRESULT hr = MFCreateMemoryBuffer(bytes, &buffer);
    if (SUCCEEDED(hr) && SUCCEEDED(hr = IMFMediaBuffer_Lock(buffer, &p, NULL, NULL))) {
        memcpy(p, pcm, bytes);
        IMFMediaBuffer_Unlock(buffer);
        IMFMediaBuffer_SetCurrentLength(buffer, bytes);
        hr = MFCreateSample(&sample);
    }
    if (SUCCEEDED(hr)) {
        IMFSample_AddBuffer(sample, buffer);
        IMFSample_SetSampleTime(sample, pts);
        IMFSample_SetSampleDuration(sample, (INT64)FRAME * SECOND / AUDIO_RATE);
        hr = IMFTransform_ProcessInput(mft, 0, sample, 0);
    }
    RELEASE(sample);
    RELEASE(buffer);
    while (SUCCEEDED(hr)) {
        IMFSample *out_sample = NULL;
        IMFMediaBuffer *out_buffer = NULL;
        MFCreateSample(&out_sample);
        MFCreateMemoryBuffer(output_size, &out_buffer);
        IMFSample_AddBuffer(out_sample, out_buffer);
        RELEASE(out_buffer);
        MFT_OUTPUT_DATA_BUFFER out = {0, out_sample, 0, NULL};
        DWORD status = 0;
        hr = IMFTransform_ProcessOutput(mft, 0, 1, &out, &status);
        out_sample = out.pSample;
        RELEASE(out.pEvents);
        if (hr == MF_E_TRANSFORM_NEED_MORE_INPUT) {
            RELEASE(out_sample);
            return S_OK;
        }
        if (SUCCEEDED(hr) && out_sample) {
            LONGLONG t = 0;
            IMFSample_GetSampleTime(out_sample, &t);
            if (SUCCEEDED(hr = IMFSample_ConvertToContiguousBuffer(out_sample, &out_buffer))) {
                BYTE *data;
                DWORD len;
                if (SUCCEEDED(hr = IMFMediaBuffer_Lock(out_buffer, &data, NULL, &len))) {
                    if (len) ring_push_audio(ring, packet_new(t, TRUE, data, len));
                    IMFMediaBuffer_Unlock(out_buffer);
                }
                RELEASE(out_buffer);
            }
        }
        RELEASE(out_sample);
    }
    return hr;
}

// ---------------------------------------------------------------------------

static UINT32 open_sources(Source *sources, const AudioThread *t) {
    UINT32 n = 0;
    for (int mic = 0; mic < 2; mic++) {
        if (!(mic ? t->mic : t->system)) continue;
        HRESULT hr = source_open(&sources[n], mic, t);
        if (SUCCEEDED(hr)) {
            n++;
        } else {
            logmsg("audio: couldn't open %S: %h", mic ? "microphone" : "system audio", hr);
        }
    }
    return n;
}

static HRESULT session(AudioThread *t, INT64 origin, INT64 *pos) {
    IMFTransform *mft = NULL;
    DWORD output_size = 0;
    HRESULT hr = aac_open(&mft, &output_size);
    if (FAILED(hr)) {
        RELEASE(mft);
        return hr;
    }
    Source sources[2];
    UINT32 n = open_sources(sources, t);
    logmsg("audio: capturing %u source(s)", n);
    // On the heap: 12 KB of stack would need the C runtime's stack probe.
    float *mix = mem_alloc(FRAME * CH * sizeof(float));
    INT16 *pcm = mem_alloc(FRAME * CH * sizeof(INT16));
    INT64 last_check = clock_now();

    while (!*t->stop && SUCCEEDED(hr)) {
        for (UINT32 i = 0; i < n; i++) {
            hr = source_read(&sources[i], origin);
            if (hr == AUDCLNT_E_DEVICE_INVALIDATED) {
                logmsg("audio: device went away, reopening");
                mem_free(sources[i].device);
                sources[i].device = NULL; // forces a reopen below
                hr = S_OK;
            }
            if (FAILED(hr)) goto done;
        }

        // Follow default device changes (e.g. switching to headphones).
        INT64 now = clock_now();
        if (now - last_check > SECOND) {
            last_check = now;
            BOOL stale = FALSE;
            for (UINT32 i = 0; i < n && !stale; i++) {
                BOOL follows_default = !sources[i].mic || !t->mic_device[0];
                WCHAR *def = follows_default ? default_device(sources[i].mic) : NULL;
                stale = !sources[i].device || (def && !weq(def, sources[i].device));
                mem_free(def);
            }
            if (stale) {
                logmsg("audio: default device changed, reopening sources");
                for (UINT32 i = 0; i < n; i++) source_close(&sources[i]);
                n = open_sources(sources, t);
            }
        }

        INT64 elapsed = now - origin;
        INT64 target = elapsed / SECOND * AUDIO_RATE + elapsed % SECOND * AUDIO_RATE / SECOND - MIX_DELAY;
        while (*pos + FRAME <= target && SUCCEEDED(hr)) {
            memset(mix, 0, FRAME * CH * sizeof(float));
            for (UINT32 i = 0; i < n; i++) source_mix(&sources[i], *pos, mix);
            for (int i = 0; i < FRAME * CH; i++) pcm[i] = (INT16)(CLAMP(mix[i], -1.0f, 1.0f) * 32767);
            hr = aac_encode(mft, output_size, pcm, origin + *pos * SECOND / AUDIO_RATE, t->ring);
            *pos += FRAME;
        }
        Sleep(10);
    }
done:
    for (UINT32 i = 0; i < n; i++) source_close(&sources[i]);
    mem_free(mix), mem_free(pcm);
    RELEASE(mft);
    return hr;
}

DWORD WINAPI audio_thread(void *param) {
    AudioThread *t = param;
    CoInitializeEx(NULL, COINIT_MULTITHREADED);
    // The audio timeline survives session restarts: after an error we fill the
    // downtime with silence instead of leaving a hole that breaks A/V sync.
    INT64 origin = clock_now(), pos = 0;
    while (!*t->stop) {
        HRESULT hr = session(t, origin, &pos);
        if (SUCCEEDED(hr)) break;
        logmsg("audio: session ended: %h", hr);
        for (int i = 0; i < 10 && !*t->stop; i++) Sleep(100);
    }
    CoUninitialize();
    return 0;
}
