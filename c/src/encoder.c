#include "encoder.h"

#include <codecapi.h>
#include <mferror.h>
#include <strmif.h>

static UINT64 pack(UINT32 hi, UINT32 lo) {
    return (UINT64)hi << 32 | lo;
}

static void set_codec(ICodecAPI *codec, const GUID *api, VARTYPE vt, UINT32 value, const char *what, BOOL verbose) {
    VARIANT v;
    memset(&v, 0, sizeof(v));
    v.vt = vt;
    if (vt == VT_BOOL) v.boolVal = value ? VARIANT_TRUE : VARIANT_FALSE;
    else v.ulVal = value;
    HRESULT hr = ICodecAPI_SetValue(codec, api, &v);
    if (FAILED(hr) && verbose) {
        logmsg("encoder: couldn't set %S: %h", what, hr);
    }
}

static void set_codec_api(ICodecAPI *codec, UINT32 fps, UINT32 bitrate, BOOL verbose) {
    set_codec(codec, &CODECAPI_AVEncCommonRateControlMode, VT_UI4, eAVEncCommonRateControlMode_CBR, "rate control",
              verbose);
    set_codec(codec, &CODECAPI_AVEncCommonMeanBitRate, VT_UI4, bitrate, "bitrate", verbose);
    // One keyframe per second: clips start at most 1s earlier than asked.
    set_codec(codec, &CODECAPI_AVEncMPVGOPSize, VT_UI4, fps, "GOP size", verbose);
    // No B-frames keeps latency down. Low latency mode implies it on most
    // encoders anyway (NVIDIA rejects the explicit setting), and the muxer
    // copes with reordered frames regardless.
    set_codec(codec, &CODECAPI_AVEncMPVDefaultBPictureCount, VT_UI4, 0, "B-frames", verbose);
    set_codec(codec, &CODECAPI_AVLowLatencyMode, VT_BOOL, 1, "low latency", verbose);
}

static HRESULT try_open(Encoder *e, IMFActivate *activate, ID3D11Device *device, UINT32 width, UINT32 height,
                        UINT32 fps, UINT32 bitrate) {
    IMFAttributes *attrs = NULL;
    ICodecAPI *codec = NULL;
    IMFMediaType *out_type = NULL, *in_type = NULL;
    UINT token = 0;
    HRESULT hr = IMFActivate_ActivateObject(activate, &IID_IMFTransform, (void **)&e->mft);
    if (FAILED(hr)) goto done;
    if (FAILED(hr = IMFTransform_GetAttributes(e->mft, &attrs))) goto done;
    // Hardware MFTs are async and refuse to work until unlocked.
    if (FAILED(hr = IMFAttributes_SetUINT32(attrs, &MF_TRANSFORM_ASYNC_UNLOCK, 1))) goto done;
    IMFAttributes_SetUINT32(attrs, &MF_LOW_LATENCY, 1);

    if (FAILED(hr = MFCreateDXGIDeviceManager(&token, &e->manager))) goto done;
    if (FAILED(hr = IMFDXGIDeviceManager_ResetDevice(e->manager, (IUnknown *)device, token))) goto done;
    hr = IMFTransform_ProcessMessage(e->mft, MFT_MESSAGE_SET_D3D_MANAGER, (ULONG_PTR)e->manager);
    if (FAILED(hr)) goto done;

    // E_NOTIMPL means fixed stream ids starting at 0.
    if (FAILED(IMFTransform_GetStreamIDs(e->mft, 1, &e->input_id, 1, &e->output_id))) e->input_id = e->output_id = 0;

    if (FAILED(hr = IMFTransform_QueryInterface(e->mft, &IID_ICodecAPI, (void **)&codec))) goto done;
    set_codec_api(codec, fps, bitrate, FALSE);

    if (FAILED(hr = MFCreateMediaType(&out_type))) goto done;
    IMFMediaType_SetGUID(out_type, &MF_MT_MAJOR_TYPE, &MFMediaType_Video);
    IMFMediaType_SetGUID(out_type, &MF_MT_SUBTYPE, &MFVideoFormat_H264);
    IMFMediaType_SetUINT32(out_type, &MF_MT_AVG_BITRATE, bitrate);
    IMFMediaType_SetUINT64(out_type, &MF_MT_FRAME_SIZE, pack(width, height));
    IMFMediaType_SetUINT64(out_type, &MF_MT_FRAME_RATE, pack(fps, 1));
    IMFMediaType_SetUINT64(out_type, &MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1));
    IMFMediaType_SetUINT32(out_type, &MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    IMFMediaType_SetUINT32(out_type, &MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High);
    IMFMediaType_SetUINT32(out_type, &MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709);
    IMFMediaType_SetUINT32(out_type, &MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709);
    IMFMediaType_SetUINT32(out_type, &MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709);
    IMFMediaType_SetUINT32(out_type, &MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235);
    if (FAILED(hr = IMFTransform_SetOutputType(e->mft, e->output_id, out_type, 0))) goto done;

    if (FAILED(hr = MFCreateMediaType(&in_type))) goto done;
    IMFMediaType_SetGUID(in_type, &MF_MT_MAJOR_TYPE, &MFMediaType_Video);
    IMFMediaType_SetGUID(in_type, &MF_MT_SUBTYPE, &MFVideoFormat_NV12);
    IMFMediaType_SetUINT64(in_type, &MF_MT_FRAME_SIZE, pack(width, height));
    IMFMediaType_SetUINT64(in_type, &MF_MT_FRAME_RATE, pack(fps, 1));
    IMFMediaType_SetUINT64(in_type, &MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1));
    IMFMediaType_SetUINT32(in_type, &MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    if (FAILED(hr = IMFTransform_SetInputType(e->mft, e->input_id, in_type, 0))) goto done;

    // Some drivers only honour these once the types are set.
    set_codec_api(codec, fps, bitrate, TRUE);

    MFT_OUTPUT_STREAM_INFO info;
    if (FAILED(hr = IMFTransform_GetOutputStreamInfo(e->mft, e->output_id, &info))) goto done;
    e->provides_samples = (info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES)) != 0;
    e->output_size = info.cbSize;

    if (FAILED(hr = IMFTransform_QueryInterface(e->mft, &IID_IMFMediaEventGenerator, (void **)&e->events))) goto done;
    IMFTransform_ProcessMessage(e->mft, MFT_MESSAGE_COMMAND_FLUSH, 0);
    IMFTransform_ProcessMessage(e->mft, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
    hr = IMFTransform_ProcessMessage(e->mft, MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);

done:
    RELEASE(attrs);
    RELEASE(codec);
    RELEASE(out_type);
    RELEASE(in_type);
    return hr;
}

HRESULT encoder_open(Encoder *e, ID3D11Device *device, LUID adapter, UINT32 width, UINT32 height, UINT32 fps,
                     UINT32 bitrate) {
    memset(e, 0, sizeof(*e));
    MFT_REGISTER_TYPE_INFO input = {MFMediaType_Video, MFVideoFormat_NV12};
    MFT_REGISTER_TYPE_INFO output = {MFMediaType_Video, MFVideoFormat_H264};
    IMFAttributes *attrs = NULL;
    IMFActivate **list = NULL;
    UINT32 count = 0;
    HRESULT hr = MFCreateAttributes(&attrs, 1);
    if (SUCCEEDED(hr)) hr = IMFAttributes_SetBlob(attrs, &MFT_ENUM_ADAPTER_LUID, (UINT8 *)&adapter, sizeof(adapter));
    // Only hardware encoders living on the GPU we capture from.
    if (SUCCEEDED(hr))
        hr = MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, &input, &output,
                      attrs, &list, &count);
    RELEASE(attrs);
    if (FAILED(hr)) return hr;
    if (!count) {
        logmsg("encoder: no hardware H.264 encoder found on this GPU");
        return MF_E_TOPO_CODEC_NOT_FOUND;
    }
    hr = E_FAIL;
    for (UINT32 i = 0; i < count; i++) {
        WCHAR name[128] = L"unknown encoder";
        UINT32 len;
        IMFActivate_GetString(list[i], &MFT_FRIENDLY_NAME_Attribute, name, COUNTOF(name), &len);
        if (FAILED(hr)) {
            hr = try_open(e, list[i], device, width, height, fps, bitrate);
            if (SUCCEEDED(hr)) {
                wcopy(e->name, COUNTOF(e->name), name);
                logmsg("encoder: using %s", name);
            } else {
                logmsg("encoder: %s failed: %h", name, hr);
                IMFActivate_ShutdownObject(list[i]);
                encoder_close(e);
            }
        }
        IMFActivate_Release(list[i]);
    }
    CoTaskMemFree(list);
    return hr;
}

static BYTE *sequence_header(IMFMediaType *t, UINT32 *len) {
    UINT32 size = 0;
    if (FAILED(IMFMediaType_GetBlobSize(t, &MF_MT_MPEG_SEQUENCE_HEADER, &size)) || !size) return NULL;
    BYTE *buf = mem_alloc(size);
    if (FAILED(IMFMediaType_GetBlob(t, &MF_MT_MPEG_SEQUENCE_HEADER, buf, size, NULL))) {
        mem_free(buf);
        return NULL;
    }
    *len = size;
    return buf;
}

BYTE *encoder_sequence_header(Encoder *e, UINT32 *len) {
    IMFMediaType *t = NULL;
    *len = 0;
    if (FAILED(IMFTransform_GetOutputCurrentType(e->mft, e->output_id, &t))) return NULL;
    BYTE *seq = sequence_header(t, len);
    IMFMediaType_Release(t);
    return seq;
}

static HRESULT pull_output(Encoder *e, Ring *ring) {
    IMFSample *sample = NULL;
    IMFMediaBuffer *buffer = NULL;
    if (!e->provides_samples) {
        MFCreateSample(&sample);
        MFCreateMemoryBuffer(MAX(e->output_size, 1 << 20), &buffer);
        IMFSample_AddBuffer(sample, buffer);
        RELEASE(buffer);
    }
    MFT_OUTPUT_DATA_BUFFER out = {e->output_id, sample, 0, NULL};
    DWORD status = 0;
    HRESULT hr = IMFTransform_ProcessOutput(e->mft, 0, 1, &out, &status);
    sample = out.pSample;
    RELEASE(out.pEvents);
    if (hr == MF_E_TRANSFORM_STREAM_CHANGE) {
        // The encoder wants a renegotiated output type, which also carries
        // the final SPS/PPS.
        IMFMediaType *t = NULL;
        hr = IMFTransform_GetOutputAvailableType(e->mft, e->output_id, 0, &t);
        if (SUCCEEDED(hr)) hr = IMFTransform_SetOutputType(e->mft, e->output_id, t, 0);
        RELEASE(t);
        UINT32 len;
        BYTE *seq = encoder_sequence_header(e, &len);
        if (seq) ring_set_seq_header(ring, seq, len), mem_free(seq);
        RELEASE(sample);
        return hr;
    }
    if (hr == MF_E_TRANSFORM_NEED_MORE_INPUT || (SUCCEEDED(hr) && !sample)) {
        RELEASE(sample);
        return S_OK;
    }
    if (FAILED(hr)) {
        RELEASE(sample);
        return hr;
    }
    LONGLONG pts = 0;
    UINT32 clean = 0;
    IMFSample_GetSampleTime(sample, &pts);
    IMFSample_GetUINT32(sample, &MFSampleExtension_CleanPoint, &clean);
    if (SUCCEEDED(hr = IMFSample_ConvertToContiguousBuffer(sample, &buffer))) {
        BYTE *data;
        DWORD len;
        if (SUCCEEDED(hr = IMFMediaBuffer_Lock(buffer, &data, NULL, &len))) {
            ring_push_video(ring, packet_new(pts, clean != 0, data, len));
            IMFMediaBuffer_Unlock(buffer);
        }
        RELEASE(buffer);
    }
    RELEASE(sample);
    return hr;
}

HRESULT encoder_poll(Encoder *e, Ring *ring) {
    for (;;) {
        IMFMediaEvent *event = NULL;
        HRESULT hr = IMFMediaEventGenerator_GetEvent(e->events, MF_EVENT_FLAG_NO_WAIT, &event);
        if (hr == MF_E_NO_EVENTS_AVAILABLE) return S_OK;
        if (FAILED(hr)) return hr;
        MediaEventType type = 0;
        IMFMediaEvent_GetType(event, &type);
        IMFMediaEvent_Release(event);
        if (type == METransformNeedInput) e->need_input++;
        else if (type == METransformHaveOutput && FAILED(hr = pull_output(e, ring))) return hr;
    }
}

HRESULT encoder_submit(Encoder *e, ID3D11Texture2D *texture, INT64 pts, INT64 duration) {
    IMFMediaBuffer *buffer = NULL;
    IMF2DBuffer *b2d = NULL;
    IMFSample *sample = NULL;
    HRESULT hr = MFCreateDXGISurfaceBuffer(&IID_ID3D11Texture2D, (IUnknown *)texture, 0, FALSE, &buffer);
    if (FAILED(hr)) return hr;
    if (SUCCEEDED(IMFMediaBuffer_QueryInterface(buffer, &IID_IMF2DBuffer, (void **)&b2d))) {
        DWORD len;
        if (SUCCEEDED(IMF2DBuffer_GetContiguousLength(b2d, &len))) IMFMediaBuffer_SetCurrentLength(buffer, len);
        RELEASE(b2d);
    }
    if (SUCCEEDED(hr = MFCreateSample(&sample))) {
        IMFSample_AddBuffer(sample, buffer);
        IMFSample_SetSampleTime(sample, pts);
        IMFSample_SetSampleDuration(sample, duration);
        hr = IMFTransform_ProcessInput(e->mft, e->input_id, sample, 0);
        RELEASE(sample);
    }
    RELEASE(buffer);
    if (SUCCEEDED(hr)) e->need_input--;
    return hr;
}

void encoder_close(Encoder *e) {
    if (e->mft) {
        IMFShutdown *shutdown = NULL;
        IMFTransform_ProcessMessage(e->mft, MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        IMFTransform_ProcessMessage(e->mft, MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        if (SUCCEEDED(IMFTransform_QueryInterface(e->mft, &IID_IMFShutdown, (void **)&shutdown))) {
            IMFShutdown_Shutdown(shutdown);
            RELEASE(shutdown);
        }
    }
    RELEASE(e->events);
    RELEASE(e->mft);
    RELEASE(e->manager);
}
