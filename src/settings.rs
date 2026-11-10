//! Settings window built from plain Win32 controls, in two columns.

use crate::prelude::*;

use core::cell::RefCell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, IsWindowEnabled, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, SetFocus, VK_BACK,
    VK_CONTROL, VK_DELETE, VK_ESCAPE, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU,
    VK_RETURN, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_SNAPSHOT, VK_TAB,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    DefSubclassProc, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
    IShellItem, RemoveWindowSubclass, SHCreateItemFromParsingName, SIGDN_FILESYSPATH, SetWindowSubclass,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::app;
use crate::audio::{self, DeviceInfo};
use crate::capture::{self, MonitorInfo};
use crate::config::{Config, Theme};
use crate::rt::UiCell;
use crate::theme::Palette;
use crate::util::{hotkey_name, wide, window_text};

const CLASS: PCWSTR = w!("JarvisClipSettings");

const ID_HOTKEY: i32 = 100;
const ID_LENGTH: i32 = 101;
const ID_MONITOR: i32 = 102;
const ID_RESOLUTION: i32 = 103;
const ID_FPS: i32 = 104;
const ID_BITRATE: i32 = 105;
const ID_MEMORY: i32 = 106;
const ID_SYSTEM_AUDIO: i32 = 107;
const ID_MIC: i32 = 108;
const ID_MIC_DEVICE: i32 = 109;
const ID_FOLDER: i32 = 110;
const ID_BROWSE: i32 = 111;
const ID_SOUND: i32 = 112;
const ID_AUTOSTART: i32 = 113;
const ID_MIC_VOLUME: i32 = 114;
const ID_CURSOR: i32 = 115;
const ID_RECORD_HOTKEY: i32 = 116;
const ID_STRETCH: i32 = 117;
const ID_SYSTEM_DEVICE: i32 = 118;
const ID_SEPARATE: i32 = 119;
const ID_APP_TRACKS: i32 = 120;
const ID_ADD_APP: i32 = 121;
const ID_APP_HINT: i32 = 122;
const ID_SOUND_FILE: i32 = 123;
const ID_BROWSE_SOUND: i32 = 124;
const ID_NOTIFY: i32 = 125;
const ID_THEME: i32 = 126;

/// Edit boxes, which get the field colours even when disabled.
const FIELDS: [i32; 7] = [ID_HOTKEY, ID_RECORD_HOTKEY, ID_BITRATE, ID_MIC_VOLUME, ID_FOLDER, ID_APP_TRACKS, ID_SOUND_FILE];

pub(crate) const LENGTHS: [(u32, &str); 9] = [
    (15, "15 seconds"),
    (30, "30 seconds"),
    (60, "1 minute"),
    (90, "1.5 minutes"),
    (120, "2 minutes"),
    (180, "3 minutes"),
    (300, "5 minutes"),
    (600, "10 minutes"),
    (1200, "20 minutes"),
];
pub(crate) const RESOLUTIONS: [(u32, &str); 5] = [(0, "Native"), (1440, "1440p"), (1080, "1080p"), (900, "900p"), (720, "720p")];
pub(crate) const FRAME_RATES: [u32; 8] = [30, 60, 90, 120, 144, 165, 240, 360];
const THEMES: [&str; 3] = ["Same as Windows", "Light", "Dark"];

struct Settings {
    hwnd: HWND,
    cfg: Config,
    monitors: Vec<MonitorInfo>,
    mics: Vec<DeviceInfo>,
    outputs: Vec<DeviceInfo>,
    font: HFONT,
    heading_font: HFONT,
    palette: Palette,
}

static STATE: UiCell<RefCell<Option<Settings>>> = UiCell::new(RefCell::new(None));

pub fn window() -> Option<HWND> {
    STATE.with(|s| s.borrow().as_ref().map(|s| s.hwnd))
}

fn send(hwnd: HWND, id: i32, msg: u32, wparam: usize, lparam: isize) -> isize {
    unsafe {
        let ctl = GetDlgItem(Some(hwnd), id).unwrap_or_default();
        SendMessageW(ctl, msg, Some(WPARAM(wparam)), Some(LPARAM(lparam))).0
    }
}

fn item(hwnd: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(Some(hwnd), id).unwrap_or_default() }
}

fn checked(hwnd: HWND, id: i32) -> bool {
    send(hwnd, id, BM_GETCHECK, 0, 0) == BST_CHECKED.0 as isize
}

fn combo_index(hwnd: HWND, id: i32) -> usize {
    send(hwnd, id, CB_GETCURSEL, 0, 0).max(0) as usize
}

fn set_text(ctl: HWND, text: &str) {
    let t = wide(text);
    unsafe {
        let _ = SetWindowTextW(ctl, PCWSTR(t.as_ptr()));
    }
}

pub fn open(cfg: Config) {
    if let Some(hwnd) = window() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        return;
    }
    unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            ..Default::default()
        };
        RegisterClassW(&class); // fails harmlessly if already registered

        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            CLASS,
            w!("Jarvis Clip Settings"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            100,
            100,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        let small = crate::util::app_icon(GetSystemMetrics(SM_CXSMICON));
        let big = crate::util::app_icon(GetSystemMetrics(SM_CXICON));
        SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_SMALL as usize)), Some(LPARAM(small.0 as isize)));
        SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_BIG as usize)), Some(LPARAM(big.0 as isize)));

        let dpi = GetDpiForWindow(hwnd);
        let mut ncm = NONCLIENTMETRICSW { cbSize: size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
        let _ =
            SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, ncm.cbSize, Some(&mut ncm as *mut _ as _), 0, dpi);
        let font = CreateFontIndirectW(&ncm.lfMessageFont);
        let mut heading = ncm.lfMessageFont;
        heading.lfWeight = 600;
        let heading_font = CreateFontIndirectW(&heading);

        let palette = Palette::new(crate::theme::is_dark(cfg.theme));
        palette.apply_to_window(hwnd);
        let state = Settings {
            hwnd,
            cfg,
            monitors: capture::list_monitors(),
            mics: audio::list_microphones(),
            outputs: audio::list_outputs(),
            font,
            heading_font,
            palette,
        };
        let (w, h) = state.build(dpi);

        // Size the window around the content and centre it on its monitor.
        let mut rect = RECT { left: 0, top: 0, right: w, bottom: h };
        let _ = AdjustWindowRectExForDpi(&mut rect, style, false, WS_EX_CONTROLPARENT, dpi);
        let (ww, wh) = (rect.right - rect.left, rect.bottom - rect.top);
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut mi);
        let work = mi.rcWork;
        let x = work.left + (work.right - work.left - ww) / 2;
        let y = work.top + (work.bottom - work.top - wh) / 2;
        let _ = SetWindowPos(hwnd, None, x, y.max(work.top), ww, wh, SWP_NOZORDER);

        STATE.with(|s| *s.borrow_mut() = Some(state));
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(item(hwnd, ID_HOTKEY)));
    }
}

impl Settings {
    fn create(
        &self,
        class: PCWSTR,
        text: &str,
        style: u32,
        ex: WINDOW_EX_STYLE,
        (x, y, w, h): (i32, i32, i32, i32),
        id: i32,
        font: HFONT,
    ) -> HWND {
        unsafe {
            let text = wide(text);
            let ctl = CreateWindowExW(
                ex,
                class,
                PCWSTR(text.as_ptr()),
                WINDOW_STYLE(style) | WS_CHILD | WS_VISIBLE,
                x,
                y,
                w,
                h,
                Some(self.hwnd),
                Some(HMENU(id as isize as _)),
                None,
                None,
            )
            .unwrap_or_default();
            SendMessageW(ctl, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            ctl
        }
    }

    /// Create all controls. Returns the client size.
    fn build(&self, dpi: u32) -> (i32, i32) {
        let s = |v: i32| v * dpi as i32 / 96;
        let (margin, label_w, ctl_w, row_h, row) = (s(20), s(150), s(250), s(24), s(32));
        let col_w = label_w + ctl_w;
        let width = margin + col_w + s(36) + col_w + margin;
        let tab = WS_TABSTOP.0;
        let font = self.font;
        let p = &self.palette;

        let heading = |text: &str, x: i32, y: i32| {
            self.create(w!("STATIC"), text, 0, Default::default(), (x, y, col_w, row_h), -1, self.heading_font);
        };
        let label = |text: &str, x: i32, y: i32| {
            self.create(w!("STATIC"), text, 0, Default::default(), (x, y + s(4), label_w, row_h), -1, font);
        };
        let static_text = |id: i32, text: &str, rect: (i32, i32, i32, i32)| {
            self.create(w!("STATIC"), text, 0, Default::default(), rect, id, font);
        };
        let edit = |id: i32, text: &str, style: u32, rect: (i32, i32, i32, i32)| {
            let ctl = self.create(w!("EDIT"), text, style | tab, WS_EX_CLIENTEDGE, rect, id, font);
            let multiline = style & ES_MULTILINE as u32 != 0;
            p.apply_to_control(ctl, if multiline { w!("DarkMode_Explorer") } else { w!("DarkMode_CFD") });
            ctl
        };
        let button = |id: i32, text: &str, style: i32, rect: (i32, i32, i32, i32)| {
            let ctl = self.create(w!("BUTTON"), text, (style | tab as i32) as u32, Default::default(), rect, id, font);
            p.apply_to_control(ctl, w!("DarkMode_Explorer"));
        };
        let checkbox = |id: i32, text: &str, x: i32, y: i32, w: i32, on: bool| {
            button(id, text, BS_AUTOCHECKBOX, (x, y, w, row_h));
            send(self.hwnd, id, BM_SETCHECK, on as usize, 0);
        };
        let combo = |id: i32, x: i32, y: i32, items: &[String], selected: usize| {
            let style = (CBS_DROPDOWNLIST | WS_VSCROLL.0 as i32) as u32 | tab;
            let ctl = self.create(w!("COMBOBOX"), "", style, Default::default(), (x, y, ctl_w, s(300)), id, font);
            for it in items {
                let t = wide(it);
                unsafe { SendMessageW(ctl, CB_ADDSTRING, None, Some(LPARAM(t.as_ptr() as isize))) };
            }
            unsafe { SendMessageW(ctl, CB_SETCURSEL, Some(WPARAM(selected)), None) };
            p.apply_to_control(ctl, w!("DarkMode_CFD"));
            let mut info = COMBOBOXINFO { cbSize: size_of::<COMBOBOXINFO>() as u32, ..Default::default() };
            if unsafe { GetComboBoxInfo(ctl, &mut info) }.is_ok() {
                p.apply_to_control(info.hwndList, w!("DarkMode_Explorer"));
            }
        };
        let hotkey = |id: i32, x: i32, y: i32, modifiers: u32, key: u32| {
            let ctl = edit(id, "", ES_AUTOHSCROLL as u32, (x, y, ctl_w, row_h));
            hotkey_box::attach(ctl, modifiers, key);
        };
        let cfg = &self.cfg;

        // --- Left column: recording and general
        let x = margin;
        let cx = x + label_w;
        let mut y = s(14);
        heading("Recording", x, y);
        y += row;
        label("Save clip hotkey", x, y);
        hotkey(ID_HOTKEY, cx, y, cfg.hotkey_modifiers, cfg.hotkey_key);
        y += row;
        label("Record hotkey", x, y);
        hotkey(ID_RECORD_HOTKEY, cx, y, cfg.record_hotkey_modifiers, cfg.record_hotkey_key);
        y += row;

        label("Replay length", x, y);
        let lengths: Vec<String> = LENGTHS.iter().map(|l| l.1.to_string()).collect();
        let sel = LENGTHS.iter().position(|l| l.0 >= cfg.replay_seconds).unwrap_or(2);
        combo(ID_LENGTH, cx, y, &lengths, sel);
        y += row;

        label("Display", x, y);
        let monitors: Vec<String> =
            self.monitors.iter().map(|m| format!("{} - {}x{}", m.name, m.width, m.height)).collect();
        combo(ID_MONITOR, cx, y, &monitors, (cfg.monitor as usize).min(monitors.len().saturating_sub(1)));
        y += row;

        label("Resolution", x, y);
        let res: Vec<String> = RESOLUTIONS.iter().map(|r| r.1.to_string()).collect();
        let sel = RESOLUTIONS.iter().position(|r| r.0 == cfg.resolution).unwrap_or(0);
        combo(ID_RESOLUTION, cx, y, &res, sel);
        y += row;

        label("Frame rate", x, y);
        let rates: Vec<String> = FRAME_RATES.iter().map(|r| format!("{r} fps")).collect();
        let sel = FRAME_RATES.iter().position(|&r| r >= cfg.fps).unwrap_or(1);
        combo(ID_FPS, cx, y, &rates, sel);
        y += row;

        label("Bitrate (Mbps)", x, y);
        edit(ID_BITRATE, &cfg.bitrate_mbps.to_string(), (ES_NUMBER | ES_AUTOHSCROLL) as u32, (cx, y, s(60), row_h));
        static_text(ID_MEMORY, "", (cx + s(72), y + s(4), ctl_w - s(72), row_h));
        y += row;
        checkbox(ID_CURSOR, "Record the mouse cursor", x, y, col_w, cfg.capture_cursor);
        y += row;
        checkbox(ID_STRETCH, "Keep stretched resolutions stretched", x, y, col_w, cfg.keep_stretched);
        y += row + s(10);

        heading("General", x, y);
        y += row;
        label("Save clips to", x, y);
        edit(ID_FOLDER, &cfg.save_folder, ES_AUTOHSCROLL as u32, (cx, y, ctl_w - s(84), row_h));
        button(ID_BROWSE, "Browse...", BS_PUSHBUTTON, (cx + ctl_w - s(76), y - s(1), s(76), row_h + s(2)));
        y += row;
        label("Theme", x, y);
        let themes: Vec<String> = THEMES.iter().map(|t| t.to_string()).collect();
        combo(ID_THEME, cx, y, &themes, Theme::ALL.iter().position(|&t| t == cfg.theme).unwrap_or(0));
        y += row;
        checkbox(ID_AUTOSTART, "Start with Windows", x, y, col_w, cfg.start_with_windows);
        y += row;
        let left_bottom = y;

        // --- Right column: audio and what happens on save
        let x = margin + col_w + s(36);
        let cx = x + label_w;
        let mut y = s(14);
        heading("Audio", x, y);
        y += row;
        checkbox(ID_SYSTEM_AUDIO, "System audio", x, y, label_w - s(8), cfg.system_audio);
        let mut outputs = vec!["Default output".to_string()];
        outputs.extend(self.outputs.iter().map(|d| d.name.clone()));
        let sel = self.outputs.iter().position(|d| d.id == cfg.system_audio_device).map_or(0, |i| i + 1);
        combo(ID_SYSTEM_DEVICE, cx, y, &outputs, sel);
        y += row;
        checkbox(ID_MIC, "Microphone", x, y, label_w - s(8), cfg.microphone);
        let mut mics = vec!["Default microphone".to_string()];
        mics.extend(self.mics.iter().map(|m| m.name.clone()));
        let sel = self.mics.iter().position(|m| m.id == cfg.microphone_device).map_or(0, |i| i + 1);
        combo(ID_MIC_DEVICE, cx, y, &mics, sel);
        y += row;
        label("Mic volume (%)", x, y);
        edit(ID_MIC_VOLUME, &cfg.microphone_volume.to_string(), (ES_NUMBER | ES_AUTOHSCROLL) as u32, (cx, y, s(60), row_h));
        y += row;
        checkbox(ID_SEPARATE, "Save each source on its own track too", x, y, col_w, cfg.separate_audio_tracks);
        y += row;
        label("App tracks", x, y);
        button(ID_ADD_APP, "Add app...", BS_PUSHBUTTON, (x, y + row, s(96), row_h + s(2)));
        let tracks = cfg.app_tracks.join("\r\n");
        let style = (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32 | WS_VSCROLL.0;
        edit(ID_APP_TRACKS, &tracks, style, (cx, y, ctl_w, 3 * row - s(8)));
        y += 3 * row - s(4);
        let hint = if audio::app_capture_supported() {
            "One per line, like \"Discord: discord.exe\". Track 1 always has everything."
        } else {
            "App tracks need Windows 11."
        };
        static_text(ID_APP_HINT, hint, (cx, y, ctl_w, 2 * row_h));
        y += 2 * row_h + s(6);

        heading("When a clip is saved", x, y);
        y += row;
        checkbox(ID_SOUND, "Play a sound", x, y, col_w, cfg.save_sound);
        y += row;
        label("Sound file", x, y);
        let sound = edit(ID_SOUND_FILE, &cfg.save_sound_file, ES_AUTOHSCROLL as u32, (cx, y, ctl_w - s(84), row_h));
        let cue = wide("Built-in chime");
        unsafe { SendMessageW(sound, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(cue.as_ptr() as isize))) };
        button(ID_BROWSE_SOUND, "Browse...", BS_PUSHBUTTON, (cx + ctl_w - s(76), y - s(1), s(76), row_h + s(2)));
        y += row;
        checkbox(ID_NOTIFY, "Show a notification", x, y, col_w, cfg.notifications);
        y += row;

        // --- Buttons (IDOK/IDCANCEL so Enter and Esc work via IsDialogMessage)
        let mut y = y.max(left_bottom) + s(14);
        let (bw, bh) = (s(88), s(28));
        button(IDOK.0, "Save", BS_DEFPUSHBUTTON, (width - margin - 2 * bw - s(8), y, bw, bh));
        button(IDCANCEL.0, "Cancel", BS_PUSHBUTTON, (width - margin - bw, y, bw, bh));
        y += bh + s(16);

        self.update_dependent();
        (width, y)
    }

    /// Read the form into a Config.
    fn read(&self) -> Config {
        let h = self.hwnd;
        let mut cfg = self.cfg.clone();

        (cfg.hotkey_modifiers, cfg.hotkey_key) = hotkey_box::value(item(h, ID_HOTKEY));
        (cfg.record_hotkey_modifiers, cfg.record_hotkey_key) = hotkey_box::value(item(h, ID_RECORD_HOTKEY));
        cfg.replay_seconds = LENGTHS[combo_index(h, ID_LENGTH).min(LENGTHS.len() - 1)].0;
        cfg.monitor = combo_index(h, ID_MONITOR) as u32;
        cfg.resolution = RESOLUTIONS[combo_index(h, ID_RESOLUTION).min(RESOLUTIONS.len() - 1)].0;
        cfg.fps = FRAME_RATES[combo_index(h, ID_FPS).min(FRAME_RATES.len() - 1)];
        cfg.bitrate_mbps = window_text(item(h, ID_BITRATE)).parse::<u32>().unwrap_or(cfg.bitrate_mbps).clamp(1, 150);
        cfg.capture_cursor = checked(h, ID_CURSOR);
        cfg.keep_stretched = checked(h, ID_STRETCH);
        cfg.system_audio = checked(h, ID_SYSTEM_AUDIO);
        cfg.system_audio_device = match combo_index(h, ID_SYSTEM_DEVICE) {
            0 => String::new(),
            i => self.outputs.get(i - 1).map(|d| d.id.clone()).unwrap_or_default(),
        };
        cfg.microphone = checked(h, ID_MIC);
        cfg.microphone_device = match combo_index(h, ID_MIC_DEVICE) {
            0 => String::new(),
            i => self.mics.get(i - 1).map(|m| m.id.clone()).unwrap_or_default(),
        };
        cfg.microphone_volume =
            window_text(item(h, ID_MIC_VOLUME)).parse::<u32>().unwrap_or(cfg.microphone_volume).min(400);
        cfg.separate_audio_tracks = checked(h, ID_SEPARATE);
        cfg.app_tracks =
            window_text(item(h, ID_APP_TRACKS)).lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
        let folder = window_text(item(h, ID_FOLDER));
        if !folder.trim().is_empty() {
            cfg.save_folder = folder.trim().to_string();
        }
        cfg.save_sound = checked(h, ID_SOUND);
        cfg.save_sound_file = window_text(item(h, ID_SOUND_FILE)).trim().to_string();
        cfg.notifications = checked(h, ID_NOTIFY);
        cfg.theme = Theme::ALL[combo_index(h, ID_THEME).min(Theme::ALL.len() - 1)];
        cfg.start_with_windows = checked(h, ID_AUTOSTART);
        cfg
    }

    /// Refresh the RAM estimate and enable/disable controls that depend on
    /// checkboxes.
    fn update_dependent(&self) {
        let h = self.hwnd;
        let cfg = self.read();
        set_text(item(h, ID_MEMORY), &format!("uses ~{} MB of RAM", cfg.estimated_memory_mb()));
        let apps = cfg.separate_audio_tracks && audio::app_capture_supported();
        let enable = [
            (ID_SYSTEM_DEVICE, cfg.system_audio),
            (ID_MIC_DEVICE, cfg.microphone),
            (ID_MIC_VOLUME, cfg.microphone),
            (ID_APP_TRACKS, apps),
            (ID_ADD_APP, apps),
            (ID_APP_HINT, apps),
            (ID_SOUND_FILE, cfg.save_sound),
            (ID_BROWSE_SOUND, cfg.save_sound),
        ];
        for (id, on) in enable {
            unsafe {
                let _ = EnableWindow(item(h, id), on);
            }
        }
    }
}

/// A file or folder picker. Returns the chosen path.
pub(crate) fn browse(owner: HWND, current: &str, folders: bool) -> Option<String> {
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dialog.GetOptions().ok()?;
        let kind = if folders { FOS_PICKFOLDERS } else { FOS_FILEMUSTEXIST };
        dialog.SetOptions(opts | kind | FOS_FORCEFILESYSTEM).ok()?;
        if !folders {
            let filter = [COMDLG_FILTERSPEC { pszName: w!("Sounds (*.wav)"), pszSpec: w!("*.wav") }];
            let _ = dialog.SetFileTypes(&filter);
        }
        let start = if folders { current } else { current.rsplit_once('\\').map_or("", |(dir, _)| dir) };
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide(start).as_ptr()), None) {
            let _ = dialog.SetFolder(&item);
        }
        dialog.Show(Some(owner)).ok()?;
        let result = dialog.GetResult().ok()?;
        let path = result.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as _));
        s
    }
}

/// Pop up the apps currently playing audio under the "Add app..." button and
/// add the chosen one to the app track list.
fn add_app(hwnd: HWND) {
    let apps = audio::apps_with_audio();
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        if apps.is_empty() {
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, w!("No apps are playing sound right now"));
        }
        for (i, exe) in apps.iter().enumerate() {
            let t = wide(exe);
            let _ = AppendMenuW(menu, MF_STRING, i + 1, PCWSTR(t.as_ptr()));
        }
        let mut r = RECT::default();
        let _ = GetWindowRect(item(hwnd, ID_ADD_APP), &mut r);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN, r.left, r.bottom, None, hwnd, None);
        let _ = DestroyMenu(menu);
        let Some(exe) = apps.get((cmd.0 as usize).wrapping_sub(1)) else { return };
        let list = item(hwnd, ID_APP_TRACKS);
        let mut text = window_text(list).trim_end().to_string();
        if !text.is_empty() {
            text.push_str("\r\n");
        }
        text.push_str(&format!("{}: {exe}", crate::rt::path::file_stem(exe)));
        set_text(list, &text);
    }
}

/// Checkboxes ignore the text colour they're given, so in dark mode they're
/// drawn here: the themed box, then the label in the palette's colours.
fn draw_checkbox(nm: &NMCUSTOMDRAW, p: &Palette, font: HFONT) -> LRESULT {
    unsafe {
        let (ctl, hdc, rc) = (nm.hdr.hwndFrom, nm.hdc, nm.rc);
        FillRect(hdc, &rc, p.background);
        let state = nm.uItemState;
        let on = SendMessageW(ctl, BM_GETCHECK, None, None).0 == BST_CHECKED.0 as isize;
        let disabled = state.0 & CDIS_DISABLED.0 != 0;
        let look = if disabled {
            3
        } else if state.0 & CDIS_SELECTED.0 != 0 {
            2
        } else if state.0 & CDIS_HOT.0 != 0 {
            1
        } else {
            0
        };
        let part_state = if on { CBS_CHECKEDNORMAL.0 } else { CBS_UNCHECKEDNORMAL.0 } + look;

        let theme = OpenThemeData(Some(ctl), w!("Button"));
        let dpi = GetDpiForWindow(ctl) as i32;
        let size = GetThemePartSize(theme, Some(hdc), BP_CHECKBOX.0, part_state, None, TS_DRAW)
            .unwrap_or(SIZE { cx: 13 * dpi / 96, cy: 13 * dpi / 96 });
        let top = rc.top + (rc.bottom - rc.top - size.cy) / 2;
        let glyph = RECT { left: rc.left, top, right: rc.left + size.cx, bottom: top + size.cy };
        let _ = DrawThemeBackground(theme, hdc, BP_CHECKBOX.0, part_state, &glyph, None);
        let _ = CloseThemeData(theme);

        let mut text: Vec<u16> = window_text(ctl).encode_utf16().collect();
        let mut text_rect = RECT { left: glyph.right + 5 * dpi / 96, ..rc };
        let old_font = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, if disabled { p.dim_text } else { p.text });
        DrawTextW(hdc, &mut text, &mut text_rect, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        if state.0 & CDIS_FOCUS.0 != 0 && state.0 & CDIS_SHOWKEYBOARDCUES.0 != 0 {
            let mut focus = text_rect;
            DrawTextW(hdc, &mut text, &mut focus, DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT);
            let h = focus.bottom - focus.top;
            focus.top = rc.top + (rc.bottom - rc.top - h) / 2 - 1;
            focus.bottom = focus.top + h + 2;
            focus.left -= 1;
            focus.right += 1;
            let _ = DrawFocusRect(hdc, &focus);
        }
        SelectObject(hdc, old_font);
        LRESULT(CDRF_SKIPDEFAULT as isize)
    }
}

/// Colours for static and edit controls: the palette, with dimmed text for
/// hints and disabled controls.
fn control_colors(msg: u32, hdc: HDC, ctl: HWND) -> Option<LRESULT> {
    STATE.with(|s| {
        let s = s.try_borrow().ok()?;
        let s = s.as_ref()?;
        let p = &s.palette;
        let id = unsafe { GetDlgCtrlID(ctl) };
        let field = msg != WM_CTLCOLORSTATIC || FIELDS.contains(&id);
        if !p.dark && field {
            return None; // the system's own look
        }
        let dim = matches!(id, ID_MEMORY | ID_APP_HINT) || !unsafe { IsWindowEnabled(ctl) }.as_bool();
        unsafe {
            SetTextColor(hdc, if dim { p.dim_text } else { p.text });
            SetBkColor(hdc, if field { p.field_color } else { p.background_color });
            if !field {
                SetBkMode(hdc, TRANSPARENT);
            }
        }
        Some(LRESULT(if field { p.field.0 } else { p.background.0 } as isize))
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as i32;
            let code = ((wparam.0 >> 16) & 0xffff) as u32;
            match id {
                x if x == IDOK.0 => {
                    let cfg = STATE.with(|s| s.borrow().as_ref().map(|s| s.read()));
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                    if let Some(cfg) = cfg {
                        app::apply_config(cfg);
                    }
                }
                x if x == IDCANCEL.0 => unsafe {
                    let _ = DestroyWindow(hwnd);
                },
                // Modal dialogs and menus below: no STATE borrow held.
                ID_BROWSE => {
                    if let Some(path) = browse(hwnd, &window_text(item(hwnd, ID_FOLDER)), true) {
                        set_text(item(hwnd, ID_FOLDER), &path);
                    }
                }
                ID_BROWSE_SOUND => {
                    if let Some(path) = browse(hwnd, &window_text(item(hwnd, ID_SOUND_FILE)), false) {
                        set_text(item(hwnd, ID_SOUND_FILE), &path);
                        crate::util::play_saved_sound(&path); // a preview
                    }
                }
                ID_ADD_APP => add_app(hwnd),
                ID_BITRATE | ID_LENGTH | ID_SYSTEM_AUDIO | ID_MIC | ID_SEPARATE | ID_APP_TRACKS | ID_SOUND
                    if matches!(code, EN_CHANGE | CBN_SELCHANGE | BN_CLICKED) =>
                {
                    STATE.with(|s| {
                        if let Ok(s) = s.try_borrow() {
                            if let Some(s) = s.as_ref() {
                                s.update_dependent();
                            }
                        }
                    });
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_NOTIFY => {
            let nm = unsafe { &*(lparam.0 as *const NMCUSTOMDRAW) };
            if nm.hdr.code == NM_CUSTOMDRAW && nm.dwDrawStage == CDDS_PREPAINT {
                let style = unsafe { GetWindowLongW(nm.hdr.hwndFrom, GWL_STYLE) } as u32;
                let is_checkbox = style & BS_TYPEMASK as u32 == BS_AUTOCHECKBOX as u32;
                let drawn = STATE.with(|s| {
                    let s = s.try_borrow().ok()?;
                    let s = s.as_ref().filter(|s| s.palette.dark && is_checkbox)?;
                    Some(draw_checkbox(nm, &s.palette, s.font))
                });
                if let Some(r) = drawn {
                    return r;
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            control_colors(msg, HDC(wparam.0 as _), HWND(lparam.0 as _))
                .unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) })
        }
        WM_ERASEBKGND => {
            let brush = STATE.with(|s| s.try_borrow().ok().and_then(|s| s.as_ref().map(|s| s.palette.background)));
            let Some(brush) = brush else { return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) } };
            unsafe {
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                FillRect(HDC(wparam.0 as _), &rc, brush);
            }
            LRESULT(1)
        }
        WM_DESTROY => {
            if let Some(s) = STATE.with(|s| s.borrow_mut().take()) {
                unsafe {
                    let _ = DeleteObject(s.font.into());
                    let _ = DeleteObject(s.heading_font.into());
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// An edit box that records a key combination instead of text. Unlike the
/// stock hotkey control it takes the Windows key, can be cleared (Backspace
/// or Delete) and follows the dark theme. The combination lives in the
/// window's user data as `modifiers << 16 | key`.
mod hotkey_box {
    use super::*;

    const SUBCLASS_ID: usize = 1;

    pub fn attach(ctl: HWND, modifiers: u32, key: u32) {
        unsafe {
            let _ = SetWindowSubclass(ctl, Some(proc), SUBCLASS_ID, 0);
        }
        set(ctl, modifiers, key);
    }

    pub fn value(ctl: HWND) -> (u32, u32) {
        let v = unsafe { GetWindowLongPtrW(ctl, GWLP_USERDATA) } as u32;
        (v >> 16, v & 0xffff)
    }

    fn set(ctl: HWND, modifiers: u32, key: u32) {
        unsafe { SetWindowLongPtrW(ctl, GWLP_USERDATA, (modifiers << 16 | key) as isize) };
        show(ctl, &hotkey_name(modifiers, key));
    }

    fn show(ctl: HWND, text: &str) {
        set_text(ctl, text);
        unsafe {
            SendMessageW(ctl, EM_SETSEL, Some(WPARAM(usize::MAX)), Some(LPARAM(-1)));
        }
    }

    fn held_modifiers() -> u32 {
        let down = |vk: u16| unsafe { GetKeyState(vk as i32) } < 0;
        let mut m = 0;
        if down(VK_CONTROL.0) {
            m |= MOD_CONTROL.0;
        }
        if down(VK_MENU.0) {
            m |= MOD_ALT.0;
        }
        if down(VK_SHIFT.0) {
            m |= MOD_SHIFT.0;
        }
        if down(VK_LWIN.0) || down(VK_RWIN.0) {
            m |= MOD_WIN.0;
        }
        m
    }

    fn is_modifier(vk: u16) -> bool {
        [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN, VK_LCONTROL, VK_RCONTROL, VK_LMENU, VK_RMENU, VK_LSHIFT, VK_RSHIFT]
            .iter()
            .any(|k| k.0 == vk)
    }

    unsafe extern "system" fn proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        let vk = wparam.0 as u16;
        match msg {
            WM_GETDLGCODE => {
                // Tab, Esc and Enter still belong to the window; every other key is ours.
                let m = lparam.0 as *const MSG;
                let key = (!m.is_null()).then(|| unsafe { &*m }).filter(|m| m.message == WM_KEYDOWN);
                if key.is_some_and(|m| [VK_TAB, VK_ESCAPE, VK_RETURN].iter().any(|k| m.wParam.0 as u16 == k.0)) {
                    return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
                }
                LRESULT((DLGC_WANTALLKEYS | DLGC_WANTCHARS) as isize)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let modifiers = held_modifiers();
                if is_modifier(vk) {
                    // Show what's held so far, e.g. "Ctrl+Alt+".
                    let partial = hotkey_name(modifiers, 0x41);
                    show(hwnd, &partial[..partial.len() - 1]);
                } else if modifiers == 0 && (vk == VK_BACK.0 || vk == VK_DELETE.0) {
                    set(hwnd, 0, 0);
                } else {
                    set(hwnd, modifiers, vk as u32);
                }
                LRESULT(0)
            }
            WM_KEYUP | WM_SYSKEYUP => {
                if vk == VK_SNAPSHOT.0 {
                    // Print Screen only ever arrives as a key-up.
                    set(hwnd, held_modifiers(), vk as u32);
                } else if is_modifier(vk) && held_modifiers() == 0 {
                    let (m, k) = value(hwnd);
                    set(hwnd, m, k);
                }
                LRESULT(0)
            }
            WM_CHAR | WM_SYSCHAR | WM_DEADCHAR | WM_SYSDEADCHAR | WM_PASTE | WM_CUT | WM_CLEAR | WM_CONTEXTMENU => {
                LRESULT(0)
            }
            WM_NCDESTROY => unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(proc), SUBCLASS_ID);
                DefSubclassProc(hwnd, msg, wparam, lparam)
            },
            _ => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
        }
    }
}
