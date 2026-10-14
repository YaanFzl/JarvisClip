//! Light and dark appearance for the settings window and the tray menu.
//!
//! Win32 controls have no official dark mode. Windows' own apps get one from
//! the "DarkMode_Explorer" and "DarkMode_CFD" visual style classes and two
//! undocumented uxtheme exports, which is what this uses too. Where those are
//! missing (older Windows 10) things just stay light.

use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    COLOR_GRAYTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT, CreateSolidBrush, GetSysColor, GetSysColorBrush, HBRUSH, SYS_COLOR_INDEX,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::core::{BOOL, PCSTR, PCWSTR, w};

use crate::config::Theme;

pub fn is_dark(theme: Theme) -> bool {
    match theme {
        Theme::System => !crate::util::apps_use_light_theme(),
        Theme::Light => false,
        Theme::Dark => true,
    }
}

/// Make popup menus (the tray menu) light or dark. Uses uxtheme's
/// SetPreferredAppMode (ordinal 135, Windows 10 1903+), as Explorer does.
pub fn set_menu_theme(dark: bool) {
    if crate::util::windows_build() < 18362 {
        return;
    }
    type SetPreferredAppMode = unsafe extern "system" fn(mode: i32) -> i32;
    type FlushMenuThemes = unsafe extern "system" fn();
    const FORCE_DARK: i32 = 2;
    const FORCE_LIGHT: i32 = 3;
    unsafe {
        let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        if let Some(f) = GetProcAddress(uxtheme, PCSTR(135 as *const u8)) {
            let set: SetPreferredAppMode = core::mem::transmute(f);
            set(if dark { FORCE_DARK } else { FORCE_LIGHT });
        }
        if let Some(f) = GetProcAddress(uxtheme, PCSTR(136 as *const u8)) {
            let flush: FlushMenuThemes = core::mem::transmute(f);
            flush();
        }
    }
}

/// Colours for one window.
pub struct Palette {
    pub dark: bool,
    pub background: HBRUSH,
    /// Edit boxes and drop-down lists.
    pub field: HBRUSH,
    pub text: COLORREF,
    pub dim_text: COLORREF,
    pub background_color: COLORREF,
    pub field_color: COLORREF,
}

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

impl Palette {
    pub fn new(dark: bool) -> Self {
        unsafe {
            if dark {
                // Windows 11's dark window and control colours.
                let (background, field) = (rgb(0x20, 0x20, 0x20), rgb(0x2d, 0x2d, 0x2d));
                Self {
                    dark,
                    background: CreateSolidBrush(background),
                    field: CreateSolidBrush(field),
                    text: rgb(0xf0, 0xf0, 0xf0),
                    dim_text: rgb(0x9a, 0x9a, 0x9a),
                    background_color: background,
                    field_color: field,
                }
            } else {
                let sys = |i: SYS_COLOR_INDEX| COLORREF(GetSysColor(i));
                Self {
                    dark,
                    background: GetSysColorBrush(COLOR_WINDOW),
                    field: GetSysColorBrush(COLOR_WINDOW),
                    text: sys(COLOR_WINDOWTEXT),
                    dim_text: sys(COLOR_GRAYTEXT),
                    background_color: sys(COLOR_WINDOW),
                    field_color: sys(COLOR_WINDOW),
                }
            }
        }
    }

    /// Light or dark title bar for a top-level window (set either way: some
    /// Windows builds darken it on their own when the system is dark).
    pub fn apply_to_window(&self, hwnd: HWND) {
        let on = BOOL(self.dark as i32);
        let set = |attr: DWMWINDOWATTRIBUTE| unsafe {
            DwmSetWindowAttribute(hwnd, attr, &on as *const BOOL as _, size_of::<BOOL>() as u32)
        };
        // Builds before 20H1 used attribute 19 for the same thing.
        if set(DWMWA_USE_IMMERSIVE_DARK_MODE).is_err() {
            let _ = set(DWMWINDOWATTRIBUTE(19));
        }
    }

    /// Dark visual styles for a control: `class` is "DarkMode_Explorer" for
    /// buttons and scroll bars or "DarkMode_CFD" for edits and combo boxes.
    pub fn apply_to_control(&self, hwnd: HWND, class: PCWSTR) {
        if self.dark {
            unsafe {
                let _ = SetWindowTheme(hwnd, class, PCWSTR::null());
            }
        }
    }
}

impl Drop for Palette {
    fn drop(&mut self) {
        if self.dark {
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::DeleteObject(self.background.into());
                let _ = windows::Win32::Graphics::Gdi::DeleteObject(self.field.into());
            }
        }
    }
}
