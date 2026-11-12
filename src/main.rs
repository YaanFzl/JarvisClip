#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

extern crate alloc;

mod app;
mod audio;
mod capture;
mod clock;
mod config;
mod cursor;
mod encoder;
mod icon;
mod logging;
mod mux;
mod pipeline;
mod prelude;
mod record;
mod ring;
mod rt;
mod settings;
mod theme;
mod trim;
mod util;
mod ui;
mod voice;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, LPARAM, WPARAM};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::{ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};
use windows::core::w;

use config::Config;

/// CI sets JARVIS_CLIP_VERSION to the release being built (Cargo.toml's
/// version isn't bumped for every automatic release).
pub const VERSION: &str = match option_env!("JARVIS_CLIP_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

fn main() {
    unsafe {
        // The manifest already sets this; harmless if it fails.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // Single instance: a second launch just opens the running one's settings.
        let _mutex = CreateMutexW(None, true, w!("Local\\JarvisClip.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(hwnd) = FindWindowW(app::WINDOW_CLASS, None) {
                let _ = PostMessageW(Some(hwnd), app::WM_SHOW_SETTINGS, WPARAM(0), LPARAM(0));
            }
            return;
        }

        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            log!("Media Foundation unavailable: {e}");
            return;
        }
        let _ = InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES,
        });

        let dir = Config::dir();
        let _ = rt::fs::create_dir_all(&dir);
        // No log yet: first start (the installer may already have written a config).
        let first_run = !rt::fs::exists(&rt::path::join(&dir, "jarvis-clip.log"));
        logging::init(&rt::path::join(&dir, "jarvis-clip.log"));
        log!("jarvis-clip {VERSION} starting");

        app::run(first_run);

        let _ = MFShutdown();
        log!("bye");
    }
}
