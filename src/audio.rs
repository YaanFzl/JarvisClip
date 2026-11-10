//! Audio pipeline: WASAPI system loopback, an optional microphone and, on
//! Windows 11, per-app loopback streams, mixed on the shared clock and encoded
//! to AAC-LC. Every WASAPI packet carries a QPC timestamp, which is what keeps
//! audio in sync with video.
//!
//! Track 1 is always the mix of system audio and mic, so clips sound right in
//! any player. With separate tracks on, system audio and mic each get their own
//! track too, followed by one track per configured app.

use crate::prelude::*;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::rt::{Mutex, OnceLock};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::{SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::*;
use windows::core::{HSTRING, Interface, Result};

use crate::clock::{self, SECOND};
use crate::log;
use crate::mux::{AUDIO_BITRATE, AUDIO_CHANNELS, AUDIO_RATE};
use crate::ring::{Packet, Ring};

const CH: usize = AUDIO_CHANNELS as usize;
const FRAME: usize = 1024; // AAC frame size in samples per channel
/// How far behind real time the mixer runs, so late device packets still make it in.
const MIX_DELAY: i64 = AUDIO_RATE as i64 / 10;
/// Timestamp discrepancy we tolerate before resyncing a source (10ms).
const RESYNC: i64 = AUDIO_RATE as i64 / 100;

/// An extra track with the audio of some apps, matched by exe name.
#[derive(Clone, PartialEq, Debug)]
pub struct AppTrack {
    pub name: String,
    /// Lowercase exe file names.
    pub exes: Vec<String>,
}

impl AppTrack {
    /// "Discord: discord.exe, other.exe", or just "discord.exe" (named after
    /// the exe). ".exe" is optional.
    pub fn parse(line: &str) -> Option<Self> {
        let (name, list) = line.split_once(':').unwrap_or(("", line));
        let exes: Vec<String> = list
            .split(',')
            .map(|e| e.trim().to_lowercase())
            .filter(|e| !e.is_empty())
            .map(|e| if e.ends_with(".exe") { e } else { e + ".exe" })
            .collect();
        let first = list.split(',').map(str::trim).find(|e| !e.is_empty())?;
        let name = match name.trim() {
            "" => crate::rt::path::file_stem(first).to_string(),
            n => n.to_string(),
        };
        Some(Self { name, exes })
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct AudioConfig {
    pub system: bool,
    /// Output endpoint id, or empty for the default output.
    pub system_device: String,
    pub mic: bool,
    /// Endpoint id, or empty for the default microphone.
    pub mic_device: String,
    pub mic_volume: f32,
    /// Give each source its own track after the mix.
    pub separate: bool,
    pub apps: Vec<AppTrack>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum TrackKind {
    Mix,
    System,
    Mic,
    App(usize),
}

impl AudioConfig {
    fn tracks(&self) -> Vec<TrackKind> {
        let apps = if self.separate && app_capture_supported() { self.apps.len() } else { 0 };
        let mut tracks = Vec::new();
        if self.system || self.mic || apps > 0 {
            tracks.push(TrackKind::Mix);
        }
        if self.separate {
            if self.system {
                tracks.push(TrackKind::System);
            }
            if self.mic {
                tracks.push(TrackKind::Mic);
            }
            tracks.extend((0..apps).map(TrackKind::App));
        }
        tracks
    }

    /// Track names, in the order they appear in clips. Empty = no audio.
    pub fn track_names(&self) -> Vec<String> {
        let tracks = self.tracks();
        tracks
            .iter()
            .map(|t| match t {
                TrackKind::Mix if tracks.len() == 1 => "SoundHandler".to_string(),
                TrackKind::Mix => "All audio".to_string(),
                TrackKind::System => "System audio".to_string(),
                TrackKind::Mic => "Microphone".to_string(),
                TrackKind::App(i) => self.apps[*i].name.clone(),
            })
            .collect()
    }

    /// Whether a source is heard on a track. The mix is system audio and mic,
    /// or the apps when neither of those is recorded.
    fn feeds(&self, track: TrackKind, source: Kind) -> bool {
        match (track, source) {
            (TrackKind::Mix, Kind::System | Kind::Mic) => true,
            (TrackKind::Mix, Kind::App { .. }) => !self.system && !self.mic,
            (TrackKind::System, Kind::System) | (TrackKind::Mic, Kind::Mic) => true,
            (TrackKind::App(t), Kind::App { track, .. }) => t == track,
            _ => false,
        }
    }
}

/// Per-app capture (process loopback) arrived in Windows 11 (build 20348).
pub fn app_capture_supported() -> bool {
    static BUILD: OnceLock<u32> = OnceLock::new();
    *BUILD.get_or_init(crate::util::windows_build) >= 20348
}

pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

pub(crate) fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

pub(crate) fn device_id(d: &IMMDevice) -> String {
    unsafe {
        d.GetId()
            .map(|p| {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as _));
                s
            })
            .unwrap_or_default()
    }
}

fn list_devices(flow: EDataFlow) -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    unsafe {
        let Ok(e) = enumerator() else { return out };
        let Ok(list) = e.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else { return out };
        for i in 0..list.GetCount().unwrap_or(0) {
            let Ok(d) = list.Item(i) else { continue };
            let name = d
                .OpenPropertyStore(STGM_READ)
                .and_then(|s| s.GetValue(&PKEY_Device_FriendlyName))
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "Unknown device".into());
            out.push(DeviceInfo { id: device_id(&d), name });
        }
    }
    out
}

/// Microphones for the settings UI.
pub fn list_microphones() -> Vec<DeviceInfo> {
    list_devices(eCapture)
}

/// Speakers, headphones and virtual outputs for the settings UI.
pub fn list_outputs() -> Vec<DeviceInfo> {
    list_devices(eRender)
}

/// Exe names of the apps currently playing (or set up to play) audio on any
/// output, for the settings UI's app picker.
pub fn apps_with_audio() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    unsafe {
        let Ok(e) = enumerator() else { return out };
        let Ok(list) = e.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else { return out };
        for i in 0..list.GetCount().unwrap_or(0) {
            let Ok(manager) = list.Item(i).and_then(|d| d.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)) else {
                continue;
            };
            let Ok(sessions) = manager.GetSessionEnumerator() else { continue };
            for j in 0..sessions.GetCount().unwrap_or(0) {
                let Ok(session) = sessions.GetSession(j).and_then(|s| s.cast::<IAudioSessionControl2>()) else {
                    continue;
                };
                let pid = session.GetProcessId().unwrap_or(0);
                if pid == 0 || session.IsSystemSoundsSession() == windows::Win32::Foundation::S_OK {
                    continue;
                }
                let Some(exe) = crate::util::process_path(pid) else { continue };
                let exe = crate::rt::path::file_name(&exe).to_string();
                if !exe.eq_ignore_ascii_case("jarvis-clip.exe")
                    && !out.iter().any(|o| o.eq_ignore_ascii_case(&exe))
                {
                    out.push(exe);
                }
            }
        }
    }
    out.sort_by_key(|e| e.to_lowercase());
    out
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    System,
    Mic,
    /// One app's process tree, for app track `track`.
    App { track: usize, pid: u32 },
}

impl Kind {
    fn describe(self) -> String {
        match self {
            Kind::System => "system audio".into(),
            Kind::Mic => "microphone".into(),
            Kind::App { pid, .. } => format!("app audio (pid {pid})"),
        }
    }
}

struct Source {
    kind: Kind,
    /// Device we opened, to notice when the default device changes. Empty for
    /// app streams.
    device: String,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    gain: f32,
    /// Interleaved stereo samples; `fifo[0]` is frame number `start`.
    fifo: VecDeque<f32>,
    start: i64,
    /// This source's samples for the frame being mixed.
    block: Vec<f32>,
    /// The device went away; drop this source and reopen it.
    lost: bool,
}

/// Ask WASAPI to hand us 48k stereo float whatever the device runs at.
pub(crate) fn start_client(client: &IAudioClient, loopback: bool) -> Result<IAudioCaptureClient> {
    let format = WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: CH as u16,
            nSamplesPerSec: AUDIO_RATE,
            nAvgBytesPerSec: AUDIO_RATE * 4 * CH as u32,
            nBlockAlign: 4 * CH as u16,
            wBitsPerSample: 32,
            cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
        dwChannelMask: SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    };
    let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if loopback {
        flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            SECOND / 5,
            0,
            &format as *const _ as *const WAVEFORMATEX,
            None,
        )?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok(capture)
    }
}

impl Source {
    fn open(kind: Kind, cfg: &AudioConfig) -> Result<Self> {
        let (client, device) = match kind {
            Kind::System | Kind::Mic => unsafe {
                let e = enumerator()?;
                let device = match kind {
                    Kind::System if cfg.system_device.is_empty() => e.GetDefaultAudioEndpoint(eRender, eConsole)?,
                    Kind::System => e.GetDevice(&HSTRING::from(&cfg.system_device))?,
                    _ if cfg.mic_device.is_empty() => e.GetDefaultAudioEndpoint(eCapture, eConsole)?,
                    _ => e.GetDevice(&HSTRING::from(&cfg.mic_device))?,
                };
                (device.Activate::<IAudioClient>(CLSCTX_ALL, None)?, device_id(&device))
            },
            Kind::App { pid, .. } => (process_loopback::activate(pid)?, String::new()),
        };
        let capture = start_client(&client, kind != Kind::Mic)?;
        Ok(Self {
            kind,
            device,
            client,
            capture,
            gain: if kind == Kind::Mic { cfg.mic_volume } else { 1.0 },
            fifo: VecDeque::new(),
            start: 0,
            block: vec![0.0; FRAME * CH],
            lost: false,
        })
    }

    fn end(&self) -> i64 {
        self.start + (self.fifo.len() / CH) as i64
    }

    /// Drain everything WASAPI has for us into the fifo, placed on the timeline.
    fn read(&mut self, origin: i64) -> Result<()> {
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
                let mut data = core::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let at = if qpc != 0 { qpc as i64 } else { clock::now() };
                let idx = ((at - origin) as i128 * AUDIO_RATE as i128 / SECOND as i128) as i64;

                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let samples: &[f32] = if silent || data.is_null() {
                    &[]
                } else {
                    core::slice::from_raw_parts(data as *const f32, frames as usize * CH)
                };

                let mut skip = 0usize;
                if self.fifo.is_empty() {
                    self.start = idx;
                } else {
                    let gap = idx - self.end();
                    if gap > RESYNC {
                        self.fifo.extend(core::iter::repeat_n(0.0, gap as usize * CH));
                    } else if gap < -RESYNC {
                        // Overlap: drop the part of this packet we already have.
                        skip = ((-gap) as usize).min(frames as usize);
                    }
                    // Otherwise it's jitter: append contiguously.
                }
                for f in skip..frames as usize {
                    for c in 0..CH {
                        let s = samples.get(f * CH + c).copied().unwrap_or(0.0);
                        self.fifo.push_back(s * self.gain);
                    }
                }
                self.capture.ReleaseBuffer(frames)?;
            }
        }
        Ok(())
    }

    /// Move frames [pos, pos + FRAME) into `block` (silence where there are none).
    fn take_block(&mut self, pos: i64) {
        self.block.fill(0.0);
        let n = FRAME as i64;
        // Throw away anything that arrived too late to be mixed.
        if self.start < pos {
            let stale = ((pos - self.start) as usize * CH).min(self.fifo.len());
            self.fifo.drain(..stale);
            self.start = pos;
        }
        let offset = self.start - pos;
        if offset >= n {
            return;
        }
        let take = ((n - offset) as usize * CH).min(self.fifo.len());
        for (i, s) in self.fifo.drain(..take).enumerate() {
            self.block[offset as usize * CH + i] = s;
        }
        self.start += (take / CH) as i64;
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Process loopback clients are only handed out asynchronously, through a
/// completion callback. This is a minimal hand-written COM object for it that
/// just signals an event.
mod process_loopback {
    use core::ffi::c_void;
    use core::mem::ManuallyDrop;
    use core::sync::atomic::{AtomicU32, Ordering};

    use windows::Win32::Foundation::{CloseHandle, E_NOINTERFACE, HANDLE, S_OK, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::IAgileObject;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
    use windows::Win32::System::Variant::VT_BLOB;
    use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, Result};

    #[repr(C)]
    struct Handler {
        vtable: *const IActivateAudioInterfaceCompletionHandler_Vtbl,
        refs: AtomicU32,
        done: HANDLE,
    }

    static VTABLE: IActivateAudioInterfaceCompletionHandler_Vtbl = IActivateAudioInterfaceCompletionHandler_Vtbl {
        base__: IUnknown_Vtbl { QueryInterface: query_interface, AddRef: add_ref, Release: release },
        ActivateCompleted: activate_completed,
    };

    unsafe extern "system" fn query_interface(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        unsafe {
            let iid = &*iid;
            // Agile: the callback arrives on a worker thread.
            if *iid == IUnknown::IID || *iid == IActivateAudioInterfaceCompletionHandler::IID || *iid == IAgileObject::IID
            {
                add_ref(this);
                *out = this;
                S_OK
            } else {
                *out = core::ptr::null_mut();
                E_NOINTERFACE
            }
        }
    }

    unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
        unsafe { (*(this as *const Handler)).refs.fetch_add(1, Ordering::Relaxed) + 1 }
    }

    unsafe extern "system" fn release(this: *mut c_void) -> u32 {
        unsafe {
            let left = (*(this as *const Handler)).refs.fetch_sub(1, Ordering::AcqRel) - 1;
            if left == 0 {
                let handler = alloc::boxed::Box::from_raw(this as *mut Handler);
                let _ = CloseHandle(handler.done);
            }
            left
        }
    }

    unsafe extern "system" fn activate_completed(this: *mut c_void, _operation: *mut c_void) -> HRESULT {
        unsafe {
            let _ = SetEvent((*(this as *const Handler)).done);
        }
        S_OK
    }

    /// An audio client capturing everything `pid` and its child processes play.
    pub fn activate(pid: u32) -> Result<IAudioClient> {
        unsafe {
            let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: pid,
                        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            };
            // Never dropped: PROPVARIANT's Drop would hand the blob, which
            // lives on this stack, to CoTaskMemFree.
            let mut prop = ManuallyDrop::new(PROPVARIANT::default());
            let value = &mut prop.Anonymous.Anonymous;
            value.vt = VT_BLOB;
            value.Anonymous.blob.cbSize = size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
            value.Anonymous.blob.pBlobData = &mut params as *mut _ as *mut u8;

            // The handler owns the event, so a late callback never signals a
            // closed handle.
            let done = CreateEventW(None, true, false, None)?;
            let raw = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(Handler {
                vtable: &VTABLE,
                refs: AtomicU32::new(1),
                done,
            }));
            let handler = IActivateAudioInterfaceCompletionHandler::from_raw(raw as *mut c_void);
            let operation = ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&*prop),
                &handler,
            )?;
            if WaitForSingleObject(done, 2000) != WAIT_OBJECT_0 {
                return Err(crate::rt::error("process loopback activation timed out"));
            }
            let mut result = S_OK;
            let mut client = None;
            operation.GetActivateResult(&mut result, &mut client)?;
            result.ok()?;
            client.ok_or_else(|| crate::rt::error("no audio client"))?.cast()
        }
    }
}

struct AacEncoder {
    mft: IMFTransform,
    output_size: u32,
}

impl AacEncoder {
    fn new() -> Result<Self> {
        unsafe {
            let mft: IMFTransform = CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER)?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
            input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CH as u32)?;
            input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 2 * CH as u32)?;
            input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_RATE * 2 * CH as u32)?;

            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
            output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CH as u32)?;
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_BITRATE / 8)?;
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw AAC frames

            mft.SetInputType(0, &input, 0)?;
            mft.SetOutputType(0, &output, 0)?;
            let output_size = mft.GetOutputStreamInfo(0)?.cbSize.max(4096);
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self { mft, output_size })
        }
    }

    fn encode(&mut self, pcm: &[i16], pts: i64, on_packet: &mut impl FnMut(Packet)) -> Result<()> {
        unsafe {
            let bytes = pcm.len() * 2;
            let buffer = MFCreateMemoryBuffer(bytes as u32)?;
            let mut ptr = core::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            core::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, ptr, bytes);
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration((pcm.len() / CH) as i64 * SECOND / AUDIO_RATE as i64)?;
            self.mft.ProcessInput(0, &sample, 0)?;

            loop {
                let out_sample = MFCreateSample()?;
                out_sample.AddBuffer(&MFCreateMemoryBuffer(self.output_size)?)?;
                let mut out = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(out_sample)),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0;
                let r = self.mft.ProcessOutput(0, &mut out, &mut status);
                let sample = ManuallyDrop::take(&mut out[0].pSample);
                drop(ManuallyDrop::take(&mut out[0].pEvents));
                match r {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) => return Err(e),
                }
                let Some(sample) = sample else { continue };
                let pts = sample.GetSampleTime()?;
                let buffer = sample.ConvertToContiguousBuffer()?;
                let mut ptr = core::ptr::null_mut();
                let mut len = 0u32;
                buffer.Lock(&mut ptr, None, Some(&mut len))?;
                let data = core::slice::from_raw_parts(ptr, len as usize).to_vec();
                buffer.Unlock()?;
                if !data.is_empty() {
                    on_packet(Packet { pts, key: true, data });
                }
            }
        }
    }
}

pub fn run(cfg: AudioConfig, ring: Arc<Mutex<Ring>>, stop: Arc<AtomicBool>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    // The audio timeline survives session restarts: after an error we fill the
    // downtime with silence instead of leaving a hole that breaks A/V sync.
    let origin = clock::now();
    let mut pos: i64 = 0;
    while !stop.load(Ordering::Relaxed) {
        match session(&cfg, &ring, &stop, origin, &mut pos) {
            Ok(()) => break,
            Err(e) => {
                log!("audio: session ended: {e}");
                for _ in 0..10 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    crate::rt::thread::sleep_ms(100);
                }
            }
        }
    }
}

fn default_device(kind: Kind) -> Option<String> {
    let flow = if kind == Kind::Mic { eCapture } else { eRender };
    unsafe { enumerator().ok()?.GetDefaultAudioEndpoint(flow, eConsole).ok().map(|d| device_id(&d)) }
}

/// Keeps the set of open sources in line with the config: reopens devices
/// that went away or stopped being the default, and follows apps as they
/// start and exit.
struct Sources {
    list: Vec<Source>,
    /// Sources that failed to open, so each failure is only logged once.
    failed: Vec<Kind>,
}

impl Sources {
    fn open(&mut self, kind: Kind, cfg: &AudioConfig) {
        match Source::open(kind, cfg) {
            Ok(s) => {
                log!("audio: opened {}", kind.describe());
                self.failed.retain(|&k| k != kind);
                self.list.push(s);
            }
            Err(e) => {
                if !self.failed.contains(&kind) {
                    log!("audio: couldn't open {}: {e}", kind.describe());
                    self.failed.push(kind);
                }
            }
        }
    }

    fn update_devices(&mut self, cfg: &AudioConfig) {
        for (kind, enabled, chosen) in
            [(Kind::System, cfg.system, &cfg.system_device), (Kind::Mic, cfg.mic, &cfg.mic_device)]
        {
            if !enabled {
                continue;
            }
            let current = self.list.iter().position(|s| s.kind == kind);
            let stale = match current {
                None => true,
                Some(i) => {
                    let s = &self.list[i];
                    // Follow default device changes (e.g. switching to headphones).
                    s.lost || (chosen.is_empty() && default_device(kind).is_some_and(|d| d != s.device))
                }
            };
            if stale {
                if let Some(i) = current {
                    log!("audio: {} changed, reopening", kind.describe());
                    self.list.remove(i);
                }
                self.open(kind, cfg);
            }
        }
    }

    fn update_apps(&mut self, cfg: &AudioConfig, tracks: &[TrackKind]) {
        let processes = crate::util::processes();
        for &t in tracks {
            let TrackKind::App(track) = t else { continue };
            let exes = &cfg.apps[track].exes;
            // One stream per process tree: skip processes started by another
            // instance of the same app (browsers and Discord run dozens).
            let matching: Vec<&crate::util::Process> =
                processes.iter().filter(|p| exes.iter().any(|e| e.eq_ignore_ascii_case(&p.exe))).collect();
            let roots: Vec<u32> = matching
                .iter()
                .filter(|p| !matching.iter().any(|q| q.pid == p.parent && q.pid != p.pid))
                .map(|p| p.pid)
                .collect();
            self.list.retain(|s| match s.kind {
                Kind::App { track: t, pid } if t == track => !s.lost && roots.contains(&pid),
                _ => true,
            });
            self.failed.retain(|k| !matches!(k, Kind::App { track: t, pid } if *t == track && !roots.contains(pid)));
            for pid in roots {
                let kind = Kind::App { track, pid };
                if !self.list.iter().any(|s| s.kind == kind) && !self.failed.contains(&kind) {
                    self.open(kind, cfg);
                }
            }
        }
    }
}

fn session(cfg: &AudioConfig, ring: &Mutex<Ring>, stop: &AtomicBool, origin: i64, pos: &mut i64) -> Result<()> {
    let tracks = cfg.tracks();
    let mut encoders = tracks.iter().map(|_| AacEncoder::new()).collect::<Result<Vec<_>>>()?;
    let mut sources = Sources { list: Vec::new(), failed: Vec::new() };
    sources.update_devices(cfg);
    sources.update_apps(cfg, &tracks);
    log!("audio: {} track(s), {} source(s)", tracks.len(), sources.list.len());

    let mut mix = vec![0f32; FRAME * CH];
    let mut pcm = vec![0i16; FRAME * CH];
    let mut last_device_check = clock::now();
    let mut last_app_check = clock::now();

    while !stop.load(Ordering::Relaxed) {
        for s in sources.list.iter_mut() {
            if let Err(e) = s.read(origin) {
                if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                    log!("audio: {} went away", s.kind.describe());
                    s.lost = true;
                } else {
                    return Err(e);
                }
            }
        }

        let now = clock::now();
        if now - last_device_check > SECOND {
            last_device_check = now;
            sources.update_devices(cfg);
        }
        if now - last_app_check > 2 * SECOND {
            last_app_check = now;
            sources.update_apps(cfg, &tracks);
        }

        let target = (now - origin) as i128 * AUDIO_RATE as i128 / SECOND as i128 - MIX_DELAY as i128;
        while *pos + FRAME as i64 <= target as i64 {
            for s in sources.list.iter_mut() {
                s.take_block(*pos);
            }
            let pts = origin + *pos * SECOND / AUDIO_RATE as i64;
            for (i, (&track, encoder)) in tracks.iter().zip(encoders.iter_mut()).enumerate() {
                mix.fill(0.0);
                for s in sources.list.iter().filter(|s| cfg.feeds(track, s.kind)) {
                    for (m, b) in mix.iter_mut().zip(&s.block) {
                        *m += b;
                    }
                }
                for (o, &m) in pcm.iter_mut().zip(&mix) {
                    *o = (m.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                }
                encoder.encode(&pcm, pts, &mut |p| ring.lock().push_audio(i, p))?;
            }
            *pos += FRAME as i64;
        }
        crate::rt::thread::sleep_ms(10);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs Windows 11 and an audio device, so it's opt-in:
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn activates_process_loopback() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        for _ in 0..3 {
            let client = process_loopback::activate(std::process::id()).unwrap();
            start_client(&client, true).unwrap();
        }
    }

    #[test]
    fn parses_app_tracks() {
        let t = AppTrack::parse("Browser: Chrome.exe, firefox ,").unwrap();
        assert_eq!(t.name, "Browser");
        assert_eq!(t.exes, vec!["chrome.exe", "firefox.exe"]);
        let t = AppTrack::parse(" Discord.exe ").unwrap();
        assert_eq!((t.name.as_str(), t.exes.as_slice()), ("Discord", &["discord.exe".to_string()][..]));
        assert!(AppTrack::parse("Nothing:").is_none());
        assert!(AppTrack::parse("  ").is_none());
    }
}
