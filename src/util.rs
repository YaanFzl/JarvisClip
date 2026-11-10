//! Small Win32 helpers shared by the tray app and the settings window.

use crate::prelude::*;

use crate::rt::OnceLock;

use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_FILENAME, SND_MEMORY, SND_NODEFAULT};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyNameTextW, MAPVK_VK_TO_VSC, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, MapVirtualKeyW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR, w};

/// NUL-terminated UTF-16 string.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Copy a string into a fixed-size UTF-16 buffer, truncating if needed.
pub fn copy_wide(dst: &mut [u16], s: &str) {
    let src: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..src.len()].copy_from_slice(&src);
    dst[src.len()] = 0;
}

pub fn window_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

/// Build an icon from straight-alpha RGBA pixels.
pub fn icon_from_rgba(size: u32, rgba: &[u8]) -> HICON {
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = core::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
        };
        let dst = core::slice::from_raw_parts_mut(bits as *mut u8, rgba.len());
        for (d, s) in dst.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], s[3]]); // RGBA -> BGRA
        }
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, None);
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// The app icon (indigo tile), rendered at exactly the requested size.
pub fn app_icon(size: i32) -> HICON {
    let size = size.max(16) as u32;
    icon_from_rgba(size, &crate::icon::tile(size))
}

fn personalize_flag(name: PCWSTR) -> bool {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            name,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as _),
            Some(&mut size),
        )
        .is_ok()
            && value != 0
    }
}

/// Whether the taskbar uses the light theme (so tray icons should be dark).
pub fn taskbar_is_light() -> bool {
    personalize_flag(w!("SystemUsesLightTheme"))
}

/// Whether apps should use the light theme ("Choose your app mode").
pub fn apps_use_light_theme() -> bool {
    personalize_flag(w!("AppsUseLightTheme"))
}

/// The Windows build number, e.g. 22631. 0 if unknown.
pub fn windows_build() -> u32 {
    let mut buf = [0u16; 16];
    let mut size = (buf.len() * 2) as u32;
    unsafe {
        let r = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as _),
            Some(&mut size),
        );
        if r.is_err() {
            return 0;
        }
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).parse().unwrap_or(0)
}

/// A WAV file of short tones (frequency, milliseconds), synthesized in memory.
fn tones(notes: &[(f32, u32)]) -> Vec<u8> {
    const RATE: u32 = 44_100;
    let mut pcm: Vec<i16> = Vec::new();
    for &(freq, ms) in notes {
        let n = RATE * ms / 1000;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            // Quick fade in/out so it doesn't click.
            let env = (i.min(n - i) as f32 / (RATE as f32 * 0.008)).min(1.0);
            let v = crate::rt::math::sin(t * freq * core::f32::consts::TAU) * env * 0.25;
            pcm.push((v * i16::MAX as f32) as i16);
        }
    }
    let data_len = pcm.len() as u32 * 2;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&RATE.to_le_bytes());
    w.extend_from_slice(&(RATE * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

fn play_wav(wav: &[u8]) {
    unsafe {
        let _ = PlaySoundW(PCWSTR(wav.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

/// The "clip saved" sound: `file` (a .wav) if set, otherwise a short
/// two-note blip.
pub fn play_saved_sound(file: &str) {
    if !file.is_empty() {
        let path = wide(file);
        let played = unsafe { PlaySoundW(PCWSTR(path.as_ptr()), None, SND_FILENAME | SND_ASYNC | SND_NODEFAULT) };
        if played.as_bool() {
            return;
        }
        crate::log!("couldn't play {file}, using the built-in sound");
    }
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    play_wav(WAV.get_or_init(|| tones(&[(880.0, 70), (1320.0, 90)])));
}

/// A single low blip for "recording started".
pub fn play_recording_sound() {
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    play_wav(WAV.get_or_init(|| tones(&[(660.0, 110)])));
}

pub fn play_error_sound() {
    unsafe {
        let _ = PlaySoundW(w!("SystemHand"), None, SND_ALIAS | SND_ASYNC);
    }
}

/// "2026-10-02 13-45-12"
pub fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// Full path of a process's exe.
pub fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(process);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

pub struct Process {
    pub pid: u32,
    pub parent: u32,
    /// Exe file name, e.g. "Discord.exe".
    pub exe: String,
}

/// Every running process.
pub fn processes() -> Vec<Process> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snapshot, &mut entry).is_ok();
        while ok {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            out.push(Process {
                pid: entry.th32ProcessID,
                parent: entry.th32ParentProcessID,
                exe: String::from_utf16_lossy(&entry.szExeFile[..len]),
            });
            ok = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    out
}

/// Name of the app in the foreground (usually the game), for clip file names.
pub fn foreground_app_name() -> String {
    unsafe {
        let hwnd = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let name = process_path(pid).map(|p| crate::rt::path::file_stem(&p).to_string());
        match name.as_deref() {
            None | Some("explorer") | Some("jarvis-clip") => "Desktop".into(),
            Some(n) => n.chars().filter(|c| !r#"<>:"/\|?*"#.contains(*c)).collect(),
        }
    }
}

pub fn hotkey_name(modifiers: u32, vk: u32) -> String {
    if vk == 0 {
        return "None".into();
    }
    let mut parts = Vec::new();
    for (flag, name) in [(MOD_CONTROL, "Ctrl"), (MOD_ALT, "Alt"), (MOD_SHIFT, "Shift"), (MOD_WIN, "Win")] {
        if modifiers & flag.0 != 0 {
            parts.push(name.to_string());
        }
    }
    let key = match vk {
        0x70..=0x87 => format!("F{}", vk - 0x6f),
        0x30..=0x39 | 0x41..=0x5a => char::from_u32(vk).unwrap().to_string(),
        _ => unsafe {
            let scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
            // Without the extended-key bit these read as their numpad twins
            // (Home is "Num 7") and Print Screen as "Sys Req".
            let extended = matches!(vk, 0x21..=0x2e | 0x5b..=0x5d | 0x6f | 0x90 | 0xa3 | 0xa5);
            let mut buf = [0u16; 64];
            let n = GetKeyNameTextW((scan << 16 | (extended as u32) << 24) as i32, &mut buf);
            if n > 0 { String::from_utf16_lossy(&buf[..n as usize]) } else { format!("Key {vk}") }
        },
    };
    parts.push(key);
    parts.join("+")
}

pub fn set_autostart(enable: bool) {
    let key = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    unsafe {
        if enable {
            let value = wide(&format!("\"{}\"", crate::rt::path::current_exe()));
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key,
                w!("Jarvis Clip"),
                REG_SZ.0,
                Some(value.as_ptr() as _),
                (value.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, key, w!("Jarvis Clip"));
        }
    }
}
