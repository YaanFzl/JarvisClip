use crate::prelude::*;
use crate::rt::{self, fs, path};

use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_ALT, MOD_CONTROL, VK_F9, VK_F10};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Theme {
    System,
    Light,
    Dark,
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];

    pub fn name(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Config {
    /// RegisterHotKey modifier flags (MOD_ALT, MOD_CONTROL, ...).
    pub hotkey_modifiers: u32,
    /// Virtual key code.
    pub hotkey_key: u32,
    /// Start/stop recording hotkey; key 0 = none.
    pub record_hotkey_modifiers: u32,
    pub record_hotkey_key: u32,
    pub replay_seconds: u32,
    pub monitor: u32,
    /// Output height in pixels, 0 = native.
    pub resolution: u32,
    pub fps: u32,
    pub bitrate_mbps: u32,
    pub capture_cursor: bool,
    /// Record at the desktop resolution when a game switches the display to
    /// another mode, so stretched resolutions stay stretched.
    pub keep_stretched: bool,
    pub system_audio: bool,
    /// Output endpoint id, empty = default output.
    pub system_audio_device: String,
    pub microphone: bool,
    /// Endpoint id, empty = default microphone.
    pub microphone_device: String,
    pub microphone_volume: u32,
    /// Also give each audio source its own track after the mix.
    pub separate_audio_tracks: bool,
    /// Extra per-app tracks, each "Name: app.exe, other.exe".
    pub app_tracks: Vec<String>,
    pub save_folder: String,
    /// Where the replay buffer lives while it waits to be clipped: temp
    /// files in this folder. Empty = keep it in RAM instead.
    pub buffer_folder: String,
    pub save_sound: bool,
    /// A .wav file to play instead of the built-in chime; empty = chime.
    pub save_sound_file: String,
    /// Show a notification when a clip or recording is saved.
    pub notifications: bool,
    pub theme: Theme,
    pub start_with_windows: bool,
    /// Listen for "jarvis clip that" / "jarvis record" / "jarvis stop".
    pub voice: bool,
    /// Recognizer confidence (percent) a voice command needs. Raise it if
    /// game sound or talking sets it off, lower it if it misses you.
    pub voice_min_confidence: u32,
}

/// The folder the exe is in. Portable: config, log and clips all live next to
/// the exe (on whatever drive it is on), nothing in AppData.
fn exe_dir() -> String {
    let exe = path::current_exe();
    exe.rsplit_once(['\\', '/']).map_or(".".into(), |(dir, _)| dir.into())
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey_modifiers: MOD_ALT.0,
            hotkey_key: VK_F10.0 as u32,
            // Ctrl+Alt: NVIDIA's overlay already owns Alt+F9 for its own recording.
            record_hotkey_modifiers: MOD_CONTROL.0 | MOD_ALT.0,
            record_hotkey_key: VK_F9.0 as u32,
            replay_seconds: 30,
            monitor: 0,
            resolution: 0,
            fps: 60,
            bitrate_mbps: 20,
            capture_cursor: true,
            keep_stretched: true,
            system_audio: true,
            system_audio_device: String::new(),
            microphone: false,
            microphone_device: String::new(),
            microphone_volume: 100,
            separate_audio_tracks: false,
            app_tracks: Vec::new(),
            save_folder: path::join(&exe_dir(), "clips"),
            buffer_folder: path::join(&exe_dir(), "buffer"),
            save_sound: true,
            save_sound_file: String::new(),
            notifications: true,
            theme: Theme::System,
            start_with_windows: false,
            voice: true,
            voice_min_confidence: 50,
        }
    }
}

impl Config {
    pub fn dir() -> String {
        exe_dir()
    }

    fn path() -> String {
        path::join(&Self::dir(), "config.toml")
    }

    pub fn load() -> Self {
        let mut cfg = Self::default();
        if let Some(text) = fs::read_to_string(&Self::path()) {
            cfg.parse(&text);
        }
        cfg.sanitize();
        cfg
    }

    pub fn save(&self) -> rt::Result<()> {
        fs::create_dir_all(&Self::dir())?;
        fs::write(&Self::path(), self.to_toml().as_bytes())
    }

    /// Read `key = value` lines (the flat subset of TOML this file uses).
    /// Unknown keys and malformed values are ignored and keep their defaults.
    pub(crate) fn parse(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            let num = || value.parse::<u32>().ok();
            let flag = || match value {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
            let text = || parse_string(value);
            match key.trim() {
                "hotkey_modifiers" => set(&mut self.hotkey_modifiers, num()),
                "hotkey_key" => set(&mut self.hotkey_key, num()),
                "record_hotkey_modifiers" => set(&mut self.record_hotkey_modifiers, num()),
                "record_hotkey_key" => set(&mut self.record_hotkey_key, num()),
                "replay_seconds" => set(&mut self.replay_seconds, num()),
                "monitor" => set(&mut self.monitor, num()),
                "resolution" => set(&mut self.resolution, num()),
                "fps" => set(&mut self.fps, num()),
                "bitrate_mbps" => set(&mut self.bitrate_mbps, num()),
                "capture_cursor" => set(&mut self.capture_cursor, flag()),
                "keep_stretched" => set(&mut self.keep_stretched, flag()),
                "system_audio" => set(&mut self.system_audio, flag()),
                "system_audio_device" => set(&mut self.system_audio_device, text()),
                "microphone" => set(&mut self.microphone, flag()),
                "microphone_device" => set(&mut self.microphone_device, text()),
                "microphone_volume" => set(&mut self.microphone_volume, num()),
                "separate_audio_tracks" => set(&mut self.separate_audio_tracks, flag()),
                "app_tracks" => set(&mut self.app_tracks, parse_string_array(value)),
                "save_folder" => set(&mut self.save_folder, text()),
                "buffer_folder" => set(&mut self.buffer_folder, text()),
                "save_sound" => set(&mut self.save_sound, flag()),
                "save_sound_file" => set(&mut self.save_sound_file, text()),
                "notifications" => set(&mut self.notifications, flag()),
                "theme" => set(&mut self.theme, text().and_then(|t| Theme::ALL.into_iter().find(|v| v.name() == t))),
                "start_with_windows" => set(&mut self.start_with_windows, flag()),
                "voice" => set(&mut self.voice, flag()),
                "voice_min_confidence" => set(&mut self.voice_min_confidence, num()),
                _ => {}
            }
        }
    }

    fn to_toml(&self) -> String {
        let s = |v: &str| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""));
        let list = |v: &[String]| format!("[{}]", v.iter().map(|x| s(x)).collect::<Vec<_>>().join(", "));
        let mut out = String::new();
        let mut line = |key: &str, value: String| {
            out.push_str(key);
            out.push_str(" = ");
            out.push_str(&value);
            out.push('\n');
        };
        line("hotkey_modifiers", self.hotkey_modifiers.to_string());
        line("hotkey_key", self.hotkey_key.to_string());
        line("record_hotkey_modifiers", self.record_hotkey_modifiers.to_string());
        line("record_hotkey_key", self.record_hotkey_key.to_string());
        line("replay_seconds", self.replay_seconds.to_string());
        line("monitor", self.monitor.to_string());
        line("resolution", self.resolution.to_string());
        line("fps", self.fps.to_string());
        line("bitrate_mbps", self.bitrate_mbps.to_string());
        line("capture_cursor", self.capture_cursor.to_string());
        line("keep_stretched", self.keep_stretched.to_string());
        line("system_audio", self.system_audio.to_string());
        line("system_audio_device", s(&self.system_audio_device));
        line("microphone", self.microphone.to_string());
        line("microphone_device", s(&self.microphone_device));
        line("microphone_volume", self.microphone_volume.to_string());
        line("separate_audio_tracks", self.separate_audio_tracks.to_string());
        line("app_tracks", list(&self.app_tracks));
        line("save_folder", s(&self.save_folder));
        line("buffer_folder", s(&self.buffer_folder));
        line("save_sound", self.save_sound.to_string());
        line("save_sound_file", s(&self.save_sound_file));
        line("notifications", self.notifications.to_string());
        line("theme", s(self.theme.name()));
        line("start_with_windows", self.start_with_windows.to_string());
        line("voice", self.voice.to_string());
        line("voice_min_confidence", self.voice_min_confidence.to_string());
        out
    }

    pub(crate) fn sanitize(&mut self) {
        self.replay_seconds = self.replay_seconds.clamp(5, 30 * 60);
        self.fps = self.fps.clamp(10, 360);
        self.bitrate_mbps = self.bitrate_mbps.clamp(1, 150);
        self.microphone_volume = self.microphone_volume.min(400);
        self.voice_min_confidence = self.voice_min_confidence.min(100);
    }

    /// Settings that require restarting the capture pipeline when changed.
    pub fn capture_settings_differ(&self, other: &Config) -> bool {
        (self.monitor, self.resolution, self.fps, self.bitrate_mbps, self.capture_cursor, self.keep_stretched)
            != (other.monitor, other.resolution, other.fps, other.bitrate_mbps, other.capture_cursor, other.keep_stretched)
            || self.audio_config() != other.audio_config()
    }

    pub fn audio_config(&self) -> crate::audio::AudioConfig {
        crate::audio::AudioConfig {
            system: self.system_audio,
            system_device: self.system_audio_device.clone(),
            mic: self.microphone,
            mic_device: self.microphone_device.clone(),
            mic_volume: self.microphone_volume as f32 / 100.0,
            separate: self.separate_audio_tracks,
            apps: self.app_tracks.iter().filter_map(|t| crate::audio::AppTrack::parse(t)).collect(),
        }
    }

    /// Rough RAM needed for the replay buffer.
    pub fn estimated_memory_mb(&self) -> u32 {
        let tracks = self.audio_config().track_names().len() as u64;
        let bytes_per_sec = self.bitrate_mbps as u64 * 1_000_000 / 8 + tracks * crate::mux::AUDIO_BITRATE as u64 / 8;
        (bytes_per_sec * (self.replay_seconds as u64 + 3) / 1_000_000) as u32
    }
}

fn set<T>(field: &mut T, value: Option<T>) {
    if let Some(v) = value {
        *field = v;
    }
}

/// A TOML basic ("...") or literal ('...') string.
fn parse_string(value: &str) -> Option<String> {
    let (s, rest) = take_string(value)?;
    rest.trim().is_empty().then_some(s)
}

/// Parse a string at the start of `value`, returning it and what follows.
fn take_string(value: &str) -> Option<(String, &str)> {
    if let Some(inner) = value.strip_prefix('\'') {
        let end = inner.find('\'')?;
        return Some((inner[..end].to_string(), &inner[end + 1..]));
    }
    let inner = value.strip_prefix('"')?;
    // Find the closing quote. `to_toml` doubles every backslash, so a lone one
    // (not before `\` or `"`) means a hand-typed Windows path like
    // "D:\games\new clips" or "\\nas\clips": that string is kept as typed.
    let bytes = inner.as_bytes();
    let (mut i, mut hand_typed) = (0, false);
    while bytes.get(i)? != &b'"' {
        if bytes[i] == b'\\' {
            if matches!(bytes.get(i + 1), Some(b'\\' | b'"')) {
                i += 1;
            } else {
                hand_typed = true;
            }
        }
        i += 1;
    }
    let raw = &inner[..i];
    let out = if hand_typed {
        raw.replace("\\\"", "\"")
    } else {
        let mut out = String::with_capacity(raw.len());
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            out.push(if c == '\\' { chars.next()? } else { c });
        }
        out
    };
    Some((out, &inner[i + 1..]))
}

/// A one-line TOML array of strings: `["a", 'b']`.
fn parse_string_array(value: &str) -> Option<Vec<String>> {
    let mut rest = value.strip_prefix('[')?.trim_start();
    let mut out = Vec::new();
    loop {
        if let Some(after) = rest.strip_prefix(']') {
            return after.trim().is_empty().then_some(out);
        }
        let (s, after) = take_string(rest)?;
        out.push(s);
        rest = after.trim_start();
        if let Some(after) = rest.strip_prefix(',') {
            rest = after.trim_start();
        } else if !rest.starts_with(']') {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut cfg = Config::default();
        cfg.save_folder = r#"C:\Users\x\Vids "quoted""#.into();
        cfg.microphone = true;
        cfg.capture_cursor = false;
        cfg.fps = 144;
        cfg.theme = Theme::Dark;
        cfg.app_tracks = vec!["Discord: discord.exe".into(), r#"Odd "name": a.exe, b.exe"#.into()];
        let mut back = Config::default();
        back.parse(&cfg.to_toml());
        assert_eq!(back, cfg);
    }

    #[test]
    fn reads_files_written_by_the_toml_crate() {
        let mut cfg = Config::default();
        cfg.parse(
            r"hotkey_modifiers = 3
save_folder = 'C:\Users\you\Videos\Bufferless'
bogus = 1
fps = nope
",
        );
        assert_eq!(cfg.hotkey_modifiers, 3);
        assert_eq!(cfg.save_folder, r"C:\Users\you\Videos\Bufferless");
        assert_eq!(cfg.fps, Config::default().fps);
    }

    #[test]
    fn keeps_unescaped_backslashes_in_hand_typed_paths() {
        let mut cfg = Config::default();
        cfg.parse(r#"buffer_folder = "D:\ML\jarvis-clip\buffer""#);
        assert_eq!(cfg.buffer_folder, r"D:\ML\jarvis-clip\buffer");
        // \n, \t and \\ in a hand-typed path are not escapes.
        cfg.parse(r#"save_folder = "D:\games\new clips""#);
        assert_eq!(cfg.save_folder, r"D:\games\new clips");
        cfg.parse(r#"buffer_folder = "E:\temp""#);
        assert_eq!(cfg.buffer_folder, r"E:\temp");
        cfg.parse(r#"save_folder = "\\nas\clips""#);
        assert_eq!(cfg.save_folder, r"\\nas\clips");
        // Written by to_toml: every backslash doubled.
        cfg.parse(r#"save_folder = "\\\\nas\\clips""#);
        assert_eq!(cfg.save_folder, r"\\nas\clips");
    }

    #[test]
    fn parses_string_arrays() {
        let parsed = parse_string_array(r#"["a", 'b\c' ,"d\"e"]"#);
        assert_eq!(parsed, Some(vec!["a".into(), r"b\c".into(), "d\"e".into()]));
        assert_eq!(parse_string_array("[]"), Some(vec![]));
        assert_eq!(parse_string_array(r#"["a" "b"]"#), None);
        assert_eq!(parse_string_array(r#"["a""#), None);
    }
}
