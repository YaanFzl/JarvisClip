//! Voice commands with Vosk (offline speech recognition, Apache-2.0).
//! `vosk\libvosk.dll` and the small English model in `vosk\model`, next to the
//! exe, are loaded at runtime; without them voice is simply off. Vosk only
//! listens for a fixed list of phrases (a grammar, with "[unk]" for anything
//! else), so it is light and rarely fires on talk or game sound.
//! (Windows' own recognizers are no use here: the SAPI desktop engine is gone
//! from Windows 11 24H2, and the WinRT one fails with 0x800455A0 in this exe.)
//!
//! Each command heard is posted to the tray window as `msg`, with the
//! command's index in wParam.

use crate::log;
use crate::prelude::*;
use crate::rt::{path, thread};

use alloc::collections::VecDeque;
use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::clock;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Media::Audio::{AUDCLNT_BUFFERFLAGS_SILENT, IAudioClient, eCapture, eConsole};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::{HSTRING, PCSTR};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    Clip,
    Record,
    Stop,
}

pub(crate) const PHRASES: [(Command, &[&str]); 3] = [
    (Command::Clip, &["jarvis clip that", "jarvis clip it", "jarvis clip"]),
    (Command::Record, &["jarvis record", "jarvis start recording"]),
    (Command::Stop, &["jarvis stop", "jarvis stop recording"]),
];

/// The model's sample rate; the mic's 48 kHz is averaged down 3:1.
const RATE: usize = 16_000;

impl Command {
    pub fn from_index(i: usize) -> Option<Self> {
        PHRASES.get(i).map(|(c, _)| *c)
    }
}

/// The few libvosk functions we use.
struct Vosk {
    recognizer: *mut c_void,
    accept: unsafe extern "C" fn(*mut c_void, *const i16, i32) -> i32,
    result: unsafe extern "C" fn(*mut c_void) -> *const c_char,
}

// The recognizer is only ever used from the voice thread.
unsafe impl Send for Vosk {}

impl Vosk {
    fn load(dir: &str) -> Result<Self, String> {
        unsafe {
            let dll = path::join(dir, "libvosk.dll");
            // Altered search path: its libstdc++ etc. load from the same folder.
            let lib = LoadLibraryExW(&HSTRING::from(&dll), None, LOAD_WITH_ALTERED_SEARCH_PATH)
                .map_err(|e| format!("can't load {dll}: {e}"))?;
            let get = |name: &[u8]| GetProcAddress(lib, PCSTR(name.as_ptr())).ok_or_else(|| format!("libvosk has no {name:?}"));
            let set_log_level: unsafe extern "C" fn(i32) = core::mem::transmute(get(b"vosk_set_log_level\0")?);
            let model_new: unsafe extern "C" fn(*const c_char) -> *mut c_void = core::mem::transmute(get(b"vosk_model_new\0")?);
            let recognizer_new: unsafe extern "C" fn(*mut c_void, f32, *const c_char) -> *mut c_void =
                core::mem::transmute(get(b"vosk_recognizer_new_grm\0")?);
            let set_words: unsafe extern "C" fn(*mut c_void, i32) = core::mem::transmute(get(b"vosk_recognizer_set_words\0")?);

            set_log_level(-1);
            let model_dir = path::join(dir, "model");
            let model = model_new(cstr(&model_dir).as_ptr().cast());
            if model.is_null() {
                return Err(format!("can't load the model in {model_dir}"));
            }
            let recognizer = recognizer_new(model, RATE as f32, cstr(&grammar()).as_ptr().cast());
            if recognizer.is_null() {
                return Err("can't create the recognizer".into());
            }
            set_words(recognizer, 1); // per-word confidence in results
            Ok(Self {
                recognizer,
                accept: core::mem::transmute(get(b"vosk_recognizer_accept_waveform_s\0")?),
                result: core::mem::transmute(get(b"vosk_recognizer_result\0")?),
            })
        }
    }

    /// Feed 16 kHz mono audio; returns the JSON result when a phrase ends.
    fn feed(&mut self, samples: &[i16]) -> Option<String> {
        unsafe {
            if (self.accept)(self.recognizer, samples.as_ptr(), samples.len() as i32) != 1 {
                return None;
            }
            let json = core::ffi::CStr::from_ptr((self.result)(self.recognizer));
            Some(json.to_string_lossy().into_owned())
        }
    }
}

fn cstr(s: &str) -> Vec<u8> {
    s.bytes().chain([0]).collect()
}

/// `["jarvis clip that", ..., "[unk]"]`
fn grammar() -> String {
    let phrases: Vec<String> = PHRASES.iter().flat_map(|(_, p)| p.iter()).chain(&["[unk]"]).map(|p| format!("\"{p}\"")).collect();
    format!("[{}]", phrases.join(", "))
}

/// Words and their confidence from a Vosk result such as
/// `{"result": [{"conf": 0.98, ..., "word": "jarvis"}, ...], "text": "..."}`.
fn parse(json: &str) -> Vec<(String, f64)> {
    let value = |obj: &str, key: &str| -> Option<String> {
        let rest = obj.split_once(&format!("\"{key}\""))?.1.trim_start_matches([' ', ':']);
        Some(match rest.strip_prefix('"') {
            Some(s) => s.split('"').next()?.to_string(),
            None => rest.split([',', '}', '\n']).next()?.trim().to_string(),
        })
    };
    json.split('{')
        .filter_map(|obj| Some((value(obj, "word")?, value(obj, "conf")?.parse().ok()?)))
        .collect()
}

/// The command in what was heard, and the lowest confidence of its words.
/// Vosk pads phrases with "[unk]" for breaths or game sound around them, so
/// the phrase only has to appear somewhere in the words, not be all of them.
fn command(words: &[(String, f64)]) -> Option<(usize, f64)> {
    let words: Vec<&(String, f64)> = words.iter().filter(|(w, _)| w != "[unk]").collect();
    let mut best: Option<(usize, usize, f64)> = None; // (command, phrase length, confidence)
    for (i, (_, phrases)) in PHRASES.iter().enumerate() {
        for phrase in *phrases {
            let want: Vec<&str> = phrase.split(' ').collect();
            for run in words.windows(want.len()) {
                if run.iter().map(|(w, _)| w.as_str()).eq(want.iter().copied())
                    && best.is_none_or(|(_, len, _)| want.len() > len)
                {
                    let confidence = run.iter().map(|(_, c)| *c).fold(1.0, f64::min);
                    best = Some((i, want.len(), confidence));
                }
            }
        }
    }
    best.map(|(i, _, c)| (i, c))
}

/// One phrase the recognizer heard, for the control window.
#[derive(Clone)]
pub struct Heard {
    /// Counts up from 1, so the page can tell new phrases from old ones.
    pub id: u64,
    pub text: String,
    pub percent: u32,
    pub accepted: bool,
    /// clock::now() when it was heard.
    pub at: i64,
}

/// "off", "loading", "listening", or what went wrong; plus the latest phrases.
static STATUS: std::sync::Mutex<(String, VecDeque<Heard>)> = std::sync::Mutex::new((String::new(), VecDeque::new()));
/// Lowest word confidence (percent) a phrase needs to count; live-adjustable.
static MIN_CONFIDENCE: AtomicU32 = AtomicU32::new(50);
/// The configured microphone's device id ("" = Windows default), and a count
/// bumped on every change so `listen` knows to reopen it.
static MICROPHONE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static MICROPHONE_CHANGES: AtomicU32 = AtomicU32::new(0);

fn set_state(state: impl Into<String>) {
    STATUS.lock().unwrap().0 = state.into();
}

pub fn status() -> (String, Vec<Heard>) {
    let s = STATUS.lock().unwrap();
    let state = if s.0.is_empty() { "off".into() } else { s.0.clone() };
    (state, s.1.iter().cloned().collect())
}

pub fn set_min_confidence(percent: u32) {
    MIN_CONFIDENCE.store(percent, Ordering::Relaxed);
}

/// Listen on this device (config `microphone_device`, "" for the default),
/// the same mic the clips record.
pub fn set_microphone(id: &str) {
    *MICROPHONE.lock().unwrap() = id.into();
    MICROPHONE_CHANGES.fetch_add(1, Ordering::Relaxed);
}

/// Listen in the background for as long as the app runs. `min_confidence`
/// (percent) is the lowest word confidence a phrase may have to count.
/// `wav`: listen to this 16 kHz mono 16-bit .wav instead of the mic (testing).
pub fn start(hwnd: HWND, msg: u32, min_confidence: u32, dir: String, wav: Option<String>) {
    let hwnd = hwnd.0 as usize;
    set_min_confidence(min_confidence);
    set_state("loading");
    // ponytail: the thread is never joined; it ends with the process.
    let _ = thread::spawn("voice", move || {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let mut vosk = match Vosk::load(&dir) {
            Ok(v) => v,
            Err(e) => {
                set_state(format!("unavailable: {e}"));
                return log!("voice commands unavailable: {e}");
            }
        };
        let mut on_result = |json: String| {
            let words = parse(&json);
            let text = words.iter().map(|(w, _)| w.as_str()).filter(|w| *w != "[unk]").collect::<Vec<_>>().join(" ");
            if text.is_empty() {
                return;
            }
            let found = command(&words);
            let percent = found.map_or(0, |(_, c)| (c * 100.0).round() as u32);
            let index = found.map(|(i, _)| i).filter(|_| percent >= MIN_CONFIDENCE.load(Ordering::Relaxed));
            log!("voice: \"{text}\" at {percent}% confidence{}", if index.is_some() { "" } else { ", ignored" });
            {
                let mut s = STATUS.lock().unwrap();
                let id = s.1.front().map_or(1, |h| h.id + 1);
                s.1.push_front(Heard { id, text: text.clone(), percent, accepted: index.is_some(), at: clock::now() });
                s.1.truncate(8);
            }
            if let Some(i) = index {
                unsafe {
                    let _ = PostMessageW(Some(HWND(hwnd as _)), msg, WPARAM(i), LPARAM(0));
                }
            }
        };
        log!("voice commands on (say \"jarvis clip that\")");
        set_state("listening");
        if let Some(wav) = wav {
            return play_wav(&wav, &mut vosk, &mut on_result);
        }
        loop {
            // Ok: the microphone setting changed, so reopen right away.
            if let Err(e) = listen(&mut vosk, &mut on_result) {
                set_state("microphone lost, retrying");
                log!("voice: microphone lost ({e}), retrying in 5 s");
                thread::sleep_ms(5000);
                set_state("listening");
            }
        }
    });
}

/// Capture the configured microphone until it fails or the setting changes.
fn listen(vosk: &mut Vosk, on_result: &mut impl FnMut(String)) -> windows::core::Result<()> {
    use crate::audio::{enumerator, start_client};
    unsafe {
        let changes = MICROPHONE_CHANGES.load(Ordering::Relaxed);
        let id = MICROPHONE.lock().unwrap().clone();
        let device = if id.is_empty() {
            enumerator()?.GetDefaultAudioEndpoint(eCapture, eConsole)?
        } else {
            enumerator()?.GetDevice(&HSTRING::from(&id))?
        };
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let capture = start_client(&client, false)?; // 48 kHz stereo f32
        let mut mono = Vec::new();
        let mut sum = 0.0f32;
        let mut count = 0;
        loop {
            thread::sleep_ms(50);
            if MICROPHONE_CHANGES.load(Ordering::Relaxed) != changes {
                return Ok(());
            }
            while capture.GetNextPacketSize()? > 0 {
                let mut data = core::ptr::null_mut();
                let (mut frames, mut flags) = (0u32, 0u32);
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let samples =
                    if silent || data.is_null() { &[][..] } else { core::slice::from_raw_parts(data as *const f32, frames as usize * 2) };
                for frame in 0..frames as usize {
                    sum += samples.get(frame * 2).map_or(0.0, |l| l + samples[frame * 2 + 1]) / 2.0;
                    count += 1;
                    if count == 3 {
                        mono.push((sum / 3.0 * 32767.0).clamp(-32768.0, 32767.0) as i16);
                        (sum, count) = (0.0, 0);
                    }
                }
                capture.ReleaseBuffer(frames)?;
            }
            if let Some(json) = vosk.feed(&mono) {
                on_result(json);
            }
            mono.clear();
        }
    }
}

/// Feed a .wav file in real time, as if it came from the mic.
fn play_wav(wav: &str, vosk: &mut Vosk, on_result: &mut impl FnMut(String)) {
    let Ok(bytes) = std::fs::read(wav) else { return log!("voice: can't read {wav}") };
    let samples: Vec<i16> = bytes[44.min(bytes.len())..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    log!("voice: listening to {wav}");
    for chunk in samples.chunks(RATE / 10) {
        if let Some(json) = vosk.feed(chunk) {
            on_result(json);
        }
        thread::sleep_ms(100);
    }
    log!("voice: end of {wav}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_results_and_maps_commands() {
        // What the mic really gave: the phrase padded with [unk].
        let word = |w: &str, c: f64| format!("{{\n      \"conf\" : {c:.6},\n      \"end\" : 1.0,\n      \"start\" : 0.5,\n      \"word\" : \"{w}\"\n    }}");
        let json = format!(
            "{{\n  \"result\" : [{}, {}, {}, {}, {}],\n  \"text\" : \"[unk] jarvis clip that [unk]\"\n}}",
            word("[unk]", 0.4),
            word("jarvis", 1.0),
            word("clip", 0.62),
            word("that", 0.9),
            word("[unk]", 0.3)
        );
        let words = parse(&json);
        assert_eq!(words.len(), 5);
        let (i, confidence) = command(&words).unwrap();
        assert_eq!(Command::from_index(i), Some(Command::Clip));
        assert!((confidence - 0.62).abs() < 1e-9, "only the phrase's own words count");

        let w = |s: &str| s.split(' ').map(|x| (x.to_string(), 1.0)).collect::<Vec<_>>();
        assert_eq!(command(&w("jarvis stop recording")).map(|(i, _)| i), Some(2));
        assert_eq!(command(&w("[unk] [unk]")), None);
        assert_eq!(command(&w("jarvis")), None);
        assert!(parse("{\"text\" : \"\"}").is_empty());
        assert!(grammar().ends_with("\"jarvis stop recording\", \"[unk]\"]"));
    }
}
