// Hardware H.264 encoding through Media Foundation. Hardware MFTs wrap
// NVENC / AMF / QuickSync, so this works on any vendor's GPU. Input frames
// are NV12 D3D11 textures, so pixels never leave the GPU.
#pragma once
#include "ring.h"

#include <d3d11.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mftransform.h>

typedef struct {
    IMFTransform *mft;
    IMFMediaEventGenerator *events;
    IMFDXGIDeviceManager *manager;
    DWORD input_id, output_id;
    UINT32 need_input; // METransformNeedInput events not answered yet
    BOOL provides_samples;
    DWORD output_size;
    WCHAR name[128];
} Encoder;

HRESULT encoder_open(Encoder *e, ID3D11Device *device, LUID adapter, UINT32 width, UINT32 height, UINT32 fps,
                     UINT32 bitrate);
// Process pending encoder events without blocking; finished packets go to the ring.
HRESULT encoder_poll(Encoder *e, Ring *ring);
HRESULT encoder_submit(Encoder *e, ID3D11Texture2D *texture, INT64 pts, INT64 duration);
// The current SPS/PPS, if the encoder reports one (caller frees).
BYTE *encoder_sequence_header(Encoder *e, UINT32 *len);
void encoder_close(Encoder *e);
