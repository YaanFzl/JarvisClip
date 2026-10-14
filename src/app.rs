//! The tray app: hidden window, tray icon, global hotkeys, clip saving and
//! recordings.

use crate::prelude::*;

use alloc::sync::Arc;
use core::cell::RefCell;

use crate::rt::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::config::Config;
use crate::pipeline::Pipeline;
use crate::record::{self, Recording};
use crate::ring::Ring;
use crate::rt::{self, UiCell, fs, path};
use crate::util::{copy_wide, hotkey_name, wide};
use crate::{clock, icon, log, mux, settings, theme, util};

const WM_TRAY: u32 = WM_APP + 1;
const WM_CLIP_SAVED: u32 = WM_APP + 2;
/// Posted by a second instance to ask us to show settings.
pub const WM_SHOW_SETTINGS: u32 = WM_APP + 3;
const WM_RECORDING_SAVED: u32 = WM_APP + 4;
pub const WINDOW_CLASS: PCWSTR = w!("BufferlessTrayWindow");

const HOTKEY_SAVE: i32 = 1;
const HOTKEY_RECORD: i32 = 2;
const TIMER_STATUS: usize = 1;

const CMD_SAVE: usize = 100;
const CMD_OPEN_FOLDER: usize = 101;
const CMD_SETTINGS: usize = 102;
const CMD_QUIT: usize = 103;
const CMD_RECORD: usize = 104;

struct TrayIcons {
    ok: HICON,
    error: HICON,
    recording: HICON,
}

impl TrayIcons {
    /// Matched to the taskbar theme: white on a dark taskbar, near-black on a
    /// light one. Errors dim the mark; recording adds a red dot.
    fn new() -> Self {
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
        let rgb = if util::taskbar_is_light() { [0x1f, 0x1f, 0x1f] } else { [0xff, 0xff, 0xff] };
        Self {
            ok: util::icon_from_rgba(size, &icon::glyph(size, rgb, 1.0)),
            error: util::icon_from_rgba(size, &icon::glyph(size, rgb, 0.4)),
            recording: util::icon_from_rgba(size, &icon::glyph_recording(size, rgb)),
        }
    }
}

impl Drop for TrayIcons {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyIcon(self.ok);
            let _ = DestroyIcon(self.error);
            let _ = DestroyIcon(self.recording);
        }
    }
}

struct App {
    hwnd: HWND,
    cfg: Config,
    ring: Arc<Mutex<Ring>>,
    /// Dropped before the pipeline, so a recording gets everything up to the
    /// moment it's stopped.
    recording: Option<Recording>,
    pipeline: Option<Pipeline>,
    icons: TrayIcons,
    showing_error: bool,
    taskbar_created: u32,
    hotkey_ok: bool,
    record_hotkey_ok: bool,
    last_clip: Option<String>,
}

static APP: UiCell<RefCell<Option<App>>> = UiCell::new(RefCell::new(None));

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.borrow_mut().as_mut().map(f))
}

pub fn current_config() -> Config {
    with_app(|a| a.cfg.clone()).unwrap_or_default()
}

pub fn run(first_run: bool) {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        RegisterClassW(&class);
        // A real (hidden) top-level window rather than a message-only one, so we
        // receive the TaskbarCreated broadcast when Explorer restarts.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            WINDOW_CLASS,
            w!("Bufferless"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .unwrap();

        let cfg = Config::load();
        if cfg.start_with_windows {
            // Re-point the startup entry at this exe in case it was moved or
            // replaced by a newer build somewhere else.
            util::set_autostart(true);
        }
        if first_run {
            let _ = cfg.save();
        }
        theme::set_menu_theme(theme::is_dark(cfg.theme));
        let ring = Arc::new(Mutex::new(Ring::new(cfg.replay_seconds)));
        let pipeline = Pipeline::start(&cfg, ring.clone());
        let app = App {
            hwnd,
            ring,
            recording: None,
            pipeline: Some(pipeline),
            icons: TrayIcons::new(),
            showing_error: false,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            hotkey_ok: false,
            record_hotkey_ok: false,
            last_clip: None,
            cfg,
        };
        APP.with(|a| *a.borrow_mut() = Some(app));

        with_app(|a| {
            a.add_tray_icon();
            a.register_hotkeys();
            if first_run && a.hotkey_ok {
                a.notify(
                    "Bufferless is running",
                    &format!(
                        "Press {} to save the last {} seconds. Right-click the tray icon for settings.",
                        hotkey_name(a.cfg.hotkey_modifiers, a.cfg.hotkey_key),
                        a.cfg.replay_seconds
                    ),
                );
            }
        });
        SetTimer(Some(hwnd), TIMER_STATUS, 2000, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if let Some(dlg) = settings::window() {
                if IsDialogMessageW(dlg, &msg).as_bool() {
                    continue;
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Dropping the app finishes any recording, then stops the pipeline
        // and joins its threads.
        if let Some(app) = APP.with(|a| a.borrow_mut().take()) {
            app.remove_tray_icon();
            if app.recording.is_some() {
                log!("finishing the recording before quitting");
            }
        }
    }
}

impl App {
    fn tray_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: self.hwnd, uID: 1, ..Default::default() }
    }

    fn current_icon(&self) -> HICON {
        if self.recording.is_some() {
            self.icons.recording
        } else if self.showing_error {
            self.icons.error
        } else {
            self.icons.ok
        }
    }

    fn add_tray_icon(&self) {
        let mut nid = self.tray_data();
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = self.current_icon();
        copy_wide(&mut nid.szTip, &self.tooltip());
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
    }

    fn remove_tray_icon(&self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.tray_data());
        }
    }

    fn tooltip(&self) -> String {
        let status = self.pipeline.as_ref().map(|p| p.status()).unwrap_or_default();
        let (buffered, bytes) = {
            let ring = self.ring.lock();
            (ring.seconds(), ring.bytes())
        };
        // The ring keeps a few extra seconds so clips can start on a keyframe,
        // but a clip is never longer than the replay length.
        let buffered = buffered.min(self.cfg.replay_seconds);
        let mut tip =
            format!("Bufferless\n{status}\n{buffered}s of {}s buffered ({} MB)", self.cfg.replay_seconds, bytes / 1_000_000);
        if let Some(r) = &self.recording {
            let s = r.seconds();
            tip = format!("{tip}\nRecording to a file: {}:{:02}", s / 60, s % 60);
        }
        tip
    }

    fn refresh_tray(&mut self) {
        let status = self.pipeline.as_ref().map(|p| p.status()).unwrap_or_default();
        self.showing_error = status.contains("error");
        let mut nid = self.tray_data();
        nid.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.hIcon = self.current_icon();
        copy_wide(&mut nid.szTip, &self.tooltip());
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn notify(&self, title: &str, text: &str) {
        let mut nid = self.tray_data();
        nid.uFlags = NIF_INFO;
        copy_wide(&mut nid.szInfoTitle, title);
        copy_wide(&mut nid.szInfo, text);
        nid.dwInfoFlags = NIIF_NONE | NIIF_NOSOUND;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    /// Notifications for things going as planned, which can be turned off.
    /// Problems are always shown.
    fn notify_success(&self, title: &str, text: &str) {
        if self.cfg.notifications {
            self.notify(title, text);
        }
    }

    fn register_hotkeys(&mut self) {
        let register = |id: i32, modifiers: u32, key: u32| unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), id);
            key == 0 || RegisterHotKey(Some(self.hwnd), id, HOT_KEY_MODIFIERS(modifiers) | MOD_NOREPEAT, key).is_ok()
        };
        self.hotkey_ok = register(HOTKEY_SAVE, self.cfg.hotkey_modifiers, self.cfg.hotkey_key);
        self.record_hotkey_ok = register(HOTKEY_RECORD, self.cfg.record_hotkey_modifiers, self.cfg.record_hotkey_key);
        let mut taken = Vec::new();
        if !self.hotkey_ok {
            taken.push(hotkey_name(self.cfg.hotkey_modifiers, self.cfg.hotkey_key));
        }
        if !self.record_hotkey_ok {
            taken.push(hotkey_name(self.cfg.record_hotkey_modifiers, self.cfg.record_hotkey_key));
        }
        if !taken.is_empty() {
            let names = taken.join(" and ");
            log!("hotkey {names} is taken");
            let verb = if taken.len() > 1 { "are" } else { "is" };
            self.notify(
                "Hotkey unavailable",
                &format!("{names} {verb} already used by another app. Pick a different one in Settings."),
            );
        }
    }

    fn save_clip(&mut self) {
        let Some(clip) = self.ring.lock().snapshot(self.cfg.replay_seconds) else {
            util::play_error_sound();
            self.notify("Nothing to save yet", "The replay buffer is still empty.");
            return;
        };
        let folder = self.cfg.save_folder.clone();
        let stem = format!("{} {}", util::foreground_app_name(), util::timestamp());
        let hwnd = self.hwnd.0 as usize;
        rt::thread::spawn("save", move || {
            let start = clock::now();
            let result = fs::create_dir_all(&folder)
                .and_then(|_| {
                    // Two saves within the same second shouldn't overwrite each other.
                    let path = record::unused_path(&folder, &stem);
                    mux::write_mp4(&path, &clip).map(|size| (path, size))
                })
                .map_err(|e| e.to_string());
            match &result {
                Ok((p, size)) => log!("saved {p} ({size} bytes) in {} ms", (clock::now() - start) / 10_000),
                Err(e) => log!("save failed: {e}"),
            }
            post_result(hwnd, WM_CLIP_SAVED, result);
        });
    }

    fn play_saved_sound(&self) {
        if self.cfg.save_sound {
            util::play_saved_sound(&self.cfg.save_sound_file);
        }
    }

    fn on_clip_saved(&mut self, result: Result<(String, u64), String>) {
        match result {
            Ok((path, size)) => {
                self.play_saved_sound();
                self.notify_success("Clip saved", &format!("{} ({})", path::file_name(&path), megabytes(size)));
                self.last_clip = Some(path);
            }
            Err(e) => {
                util::play_error_sound();
                self.notify("Couldn't save clip", &e);
            }
        }
    }

    fn toggle_recording(&mut self) {
        match &self.recording {
            Some(r) if r.stopping() => {} // still finishing the file
            Some(r) => {
                log!("recording stopping");
                r.stop();
            }
            None => self.start_recording(),
        }
        self.refresh_tray();
    }

    fn start_recording(&mut self) {
        let folder = self.cfg.save_folder.clone();
        if let Err(e) = fs::create_dir_all(&folder) {
            util::play_error_sound();
            self.notify("Couldn't start recording", &e.to_string());
            return;
        }
        let stem = format!("{} {}", util::foreground_app_name(), util::timestamp());
        let path = record::unused_path(&folder, &stem);
        let hwnd = self.hwnd.0 as usize;
        self.recording =
            Some(Recording::start(self.ring.clone(), path, move |outcome| post_result(hwnd, WM_RECORDING_SAVED, outcome)));
        if self.cfg.save_sound {
            util::play_recording_sound();
        }
        let key = hotkey_name(self.cfg.record_hotkey_modifiers, self.cfg.record_hotkey_key);
        let how = if self.cfg.record_hotkey_key != 0 { format!("Press {key} again") } else { "Use the tray menu".into() };
        self.notify_success("Recording", &format!("{how} to stop and save it."));
    }

    fn on_recording_saved(&mut self, outcome: record::Outcome) {
        // The thread is done, so this doesn't block.
        self.recording = None;
        match outcome {
            Ok(saved) => {
                self.play_saved_sound();
                let (m, s) = (saved.seconds / 60, saved.seconds % 60);
                let summary = format!("{} ({m}:{s:02}, {})", path::file_name(&saved.path), megabytes(saved.size));
                if saved.cut_short {
                    // Always worth knowing about, even with notifications off.
                    self.notify("Recording stopped", &format!("The display or capture settings changed. {summary}"));
                } else {
                    self.notify_success("Recording saved", &summary);
                }
                self.last_clip = Some(saved.path);
            }
            Err(e) => {
                util::play_error_sound();
                self.notify("Couldn't save the recording", &e);
            }
        }
        self.refresh_tray();
    }
}

/// "12.3 MB". Integer math on purpose: float formatting costs ~5 KB in the
/// tiny build.
fn megabytes(bytes: u64) -> String {
    let tenths = (bytes + 50_000) / 100_000;
    format!("{}.{} MB", tenths / 10, tenths % 10)
}

/// Hand a result from a worker thread to the UI thread.
fn post_result<T: 'static>(hwnd: usize, msg: u32, result: T) {
    let boxed = Box::into_raw(Box::new(result));
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as _)), msg, WPARAM(0), LPARAM(boxed as isize));
    }
}

/// Open the clips folder, optionally with a file selected. Called without the
/// app borrowed, since ShellExecute can pump messages.
fn open_folder(folder: &str, select: Option<&String>) {
    let _ = fs::create_dir_all(folder);
    let (file, params) = match select {
        Some(p) => (wide("explorer.exe"), wide(&format!("/select,\"{p}\""))),
        None => (wide(folder), vec![0]),
    };
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(file.as_ptr()), PCWSTR(params.as_ptr()), None, SW_SHOWNORMAL);
    }
}

impl App {
    /// Apply settings from the settings window.
    fn apply(&mut self, new: Config) {
        let old = core::mem::replace(&mut self.cfg, new);
        if let Err(e) = self.cfg.save() {
            log!("couldn't save config: {e}");
        }
        if old.capture_settings_differ(&self.cfg) {
            log!("capture settings changed, restarting pipeline");
            // The recording keeps what it has so far and ends there.
            if let Some(r) = &self.recording {
                r.stop();
            }
            self.pipeline = None; // joins the old threads first
            self.ring.lock().clear();
            self.pipeline = Some(Pipeline::start(&self.cfg, self.ring.clone()));
        } else if old.replay_seconds != self.cfg.replay_seconds {
            self.ring.lock().set_keep(self.cfg.replay_seconds);
        }
        let hotkeys = |c: &Config| (c.hotkey_modifiers, c.hotkey_key, c.record_hotkey_modifiers, c.record_hotkey_key);
        if hotkeys(&old) != hotkeys(&self.cfg) || !self.hotkey_ok || !self.record_hotkey_ok {
            self.register_hotkeys();
        }
        if old.start_with_windows != self.cfg.start_with_windows {
            util::set_autostart(self.cfg.start_with_windows);
        }
        if old.theme != self.cfg.theme {
            theme::set_menu_theme(theme::is_dark(self.cfg.theme));
        }
        self.refresh_tray();
    }
}

/// Called by the settings window when the user hits Save.
pub fn apply_config(cfg: Config) {
    with_app(|a| a.apply(cfg));
}

fn handle_command(cmd: usize) {
    match cmd {
        CMD_SAVE => {
            with_app(|a| a.save_clip());
        }
        CMD_RECORD => {
            with_app(|a| a.toggle_recording());
        }
        CMD_OPEN_FOLDER => {
            if let Some(folder) = with_app(|a| a.cfg.save_folder.clone()) {
                open_folder(&folder, None);
            }
        }
        CMD_SETTINGS => settings::open(current_config()),
        CMD_QUIT => unsafe { PostQuitMessage(0) },
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY if wparam.0 as i32 == HOTKEY_SAVE => {
            with_app(|a| a.save_clip());
            LRESULT(0)
        }
        WM_HOTKEY if wparam.0 as i32 == HOTKEY_RECORD => {
            with_app(|a| a.toggle_recording());
            LRESULT(0)
        }
        WM_TRAY => {
            // NOTIFYICON_VERSION_4: the event is in the low word of lparam.
            match (lparam.0 & 0xffff) as u32 {
                WM_CONTEXTMENU => {
                    // The menu runs a modal loop that dispatches messages back
                    // into this wndproc, so the app must not be borrowed meanwhile.
                    if let Some((hwnd, cfg, recording)) =
                        with_app(|a| (a.hwnd, a.cfg.clone(), a.recording.as_ref().map(|r| r.stopping())))
                    {
                        handle_command(show_menu(hwnd, &cfg, recording));
                    }
                }
                WM_LBUTTONDBLCLK => settings::open(current_config()),
                NIN_BALLOONUSERCLICK => {
                    if let Some((folder, clip)) = with_app(|a| (a.cfg.save_folder.clone(), a.last_clip.clone())) {
                        open_folder(&folder, clip.as_ref());
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLIP_SAVED => {
            let result = unsafe { Box::from_raw(lparam.0 as *mut Result<(String, u64), String>) };
            with_app(|a| a.on_clip_saved(*result));
            LRESULT(0)
        }
        WM_RECORDING_SAVED => {
            let outcome = unsafe { Box::from_raw(lparam.0 as *mut record::Outcome) };
            with_app(|a| a.on_recording_saved(*outcome));
            LRESULT(0)
        }
        WM_SHOW_SETTINGS => {
            settings::open(current_config());
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_STATUS => {
            with_app(|a| a.refresh_tray());
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            // Light/dark switches arrive as "ImmersiveColorSet"; just redraw.
            with_app(|a| {
                a.icons = TrayIcons::new();
                theme::set_menu_theme(theme::is_dark(a.cfg.theme));
                a.refresh_tray();
            });
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => {
            let recreated = with_app(|a| msg == a.taskbar_created && msg != 0).unwrap_or(false);
            if recreated {
                with_app(|a| a.add_tray_icon());
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

/// `recording`: None when idle, Some(stopping) while recording.
fn show_menu(hwnd: HWND, cfg: &Config, recording: Option<bool>) -> usize {
    unsafe {
        let menu = CreatePopupMenu().unwrap();
        let with_key = |label: &str, modifiers: u32, key: u32| {
            if key == 0 { label.to_string() } else { format!("{label}\t{}", hotkey_name(modifiers, key)) }
        };
        let save_label = with_key("Save clip", cfg.hotkey_modifiers, cfg.hotkey_key);
        let (record_label, record_flags) = match recording {
            None => (with_key("Start recording", cfg.record_hotkey_modifiers, cfg.record_hotkey_key), MF_STRING),
            Some(false) => (with_key("Stop recording", cfg.record_hotkey_modifiers, cfg.record_hotkey_key), MF_STRING),
            Some(true) => ("Saving recording...".to_string(), MF_STRING | MF_GRAYED),
        };
        let items = [
            (CMD_SAVE, save_label.as_str(), MF_STRING),
            (CMD_RECORD, record_label.as_str(), record_flags),
            (CMD_OPEN_FOLDER, "Open clips folder", MF_STRING),
            (CMD_SETTINGS, "Settings...", MF_STRING),
            (0, "", MF_SEPARATOR),
            (CMD_QUIT, "Quit", MF_STRING),
        ];
        for (id, label, flags) in items {
            if id == 0 {
                let _ = AppendMenuW(menu, flags, 0, None);
            } else {
                let text = wide(label);
                let _ = AppendMenuW(menu, flags, id, PCWSTR(text.as_ptr()));
            }
        }
        let _ = SetMenuDefaultItem(menu, CMD_SAVE as u32, 0);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
        cmd.0 as usize
    }
}
