//! The control window. A small page is served on 127.0.0.1 (random port, a
//! random token in every URL, Host header checked) and shown in an Edge app
//! window: no tabs, no address bar, its own profile in `ui-profile` next to the
//! exe. Edge only runs while the window is open, so the app itself stays light.
//!
//! The server thread only speaks HTTP. Everything else is answered on the UI
//! thread (SendMessage to the tray window, see app.rs), so the page sees the
//! same state as the tray.

use crate::log;
use crate::prelude::*;
use crate::rt::{path, thread};
use crate::util::wide;

use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{SW_SHOWNORMAL, SendMessageW, WM_APP};
use windows::core::{PCWSTR, w};

/// Sent by the server thread; lParam = *mut Request, answered in place.
pub const WM_UI_REQUEST: u32 = WM_APP + 6;

const PAGE: &str = include_str!("ui.html");

pub struct Request {
    pub method: String,
    /// After the token, e.g. "api/state" or "clip/name.mp4"; percent-decoded.
    pub path: String,
    /// Raw; read it with `query_value`.
    pub query: String,
    pub body: String,
    pub response: Response,
}

pub enum Response {
    NotFound,
    /// The app was busy (a menu or dialog is open); the page retries.
    Busy,
    Json(String),
    /// Stream this file (with Range support, for <video>).
    File(String),
    /// Slow work, run on the server thread so the UI thread stays free.
    /// Returns the JSON reply.
    Work(Box<dyn FnOnce() -> String + Send>),
}

/// Open the control window (starting the server the first time).
pub fn open(hwnd: HWND) {
    let Some(url) = base_url(hwnd) else { return };
    let profile = path::join(&crate::config::Config::dir(), "ui-profile");
    let args = format!(
        "--app={url} --user-data-dir=\"{profile}\" --window-size=1240,880 --no-first-run --no-default-browser-check"
    );
    unsafe {
        let args = wide(&args);
        for exe in chromium_browsers() {
            let exe = wide(&exe);
            if ShellExecuteW(None, w!("open"), PCWSTR(exe.as_ptr()), PCWSTR(args.as_ptr()), None, SW_SHOWNORMAL).0 as isize > 32 {
                return;
            }
        }
        log!("ui: no Chrome, Edge or Brave found, opening the default browser");
        let url = wide(&url);
        ShellExecuteW(None, w!("open"), PCWSTR(url.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}

/// Installed Chromium browsers (they all have the --app window mode).
fn chromium_browsers() -> Vec<String> {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let roots = [env("ProgramFiles"), env("ProgramFiles(x86)"), env("LOCALAPPDATA")];
    let apps = [
        r"Google\Chrome\Application\chrome.exe",
        r"Microsoft\Edge\Application\msedge.exe",
        r"BraveSoftware\Brave-Browser\Application\brave.exe",
    ];
    apps.iter()
        .flat_map(|app| roots.iter().filter(|r| !r.is_empty()).map(move |root| path::join(root, app)))
        .filter(|p| crate::rt::fs::exists(p))
        .collect()
}

fn base_url(hwnd: HWND) -> Option<String> {
    static BASE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let hwnd = hwnd.0 as usize;
    BASE.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| log!("ui: can't listen: {e}")).ok()?;
        let port = listener.local_addr().ok()?.port();
        let token = token();
        let url = format!("http://127.0.0.1:{port}/{token}/");
        log!("ui: serving {url}");
        let _ = thread::spawn("ui", move || {
            let (host, prefix) = (format!("127.0.0.1:{port}"), format!("/{token}/"));
            for stream in listener.incoming().flatten() {
                // A thread per connection: a playing video holds its connection
                // open, and must not stall the page's status requests.
                let (host, prefix) = (host.clone(), prefix.clone());
                let _ = thread::spawn("ui-conn", move || {
                    let _ = handle(stream, &host, &prefix, hwnd); // errors = the page went away
                });
            }
        });
        Some(url)
    })
    .clone()
}

/// 128 random bits from the OS-seeded hasher keys.
fn token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let random = || std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("{:016x}{:016x}", random(), random())
}

fn handle(mut s: TcpStream, host: &str, prefix: &str, hwnd: usize) -> std::io::Result<()> {
    s.set_read_timeout(Some(core::time::Duration::from_secs(5)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 64 * 1024 {
            return reply(&mut s, 431, "text/plain", b"");
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut request_line = head.split("\r\n").next().unwrap_or("").split(' ');
    let (method, target) = (request_line.next().unwrap_or(""), request_line.next().unwrap_or(""));
    let header = |name: &str| {
        head.split("\r\n").skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };
    // Only our own origin: blocks DNS rebinding and cross-site posts.
    let origin = format!("http://{host}");
    if header("host").as_deref() != Some(host) || header("origin").is_some_and(|o| o != origin) {
        return reply(&mut s, 403, "text/plain", b"");
    }
    let Some(rest) = target.strip_prefix(prefix) else {
        return reply(&mut s, 404, "text/plain", b"");
    };
    let len = header("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).min(1 << 20);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);

    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    if path.is_empty() {
        return reply(&mut s, 200, "text/html; charset=utf-8", PAGE.as_bytes());
    }
    let mut req = Request {
        method: method.into(),
        path: percent_decode(path),
        query: query.into(),
        body: String::from_utf8_lossy(&body).into_owned(),
        response: Response::NotFound,
    };
    unsafe {
        SendMessageW(HWND(hwnd as _), WM_UI_REQUEST, Some(WPARAM(0)), Some(LPARAM(&mut req as *mut Request as isize)));
    }
    match req.response {
        Response::NotFound => reply(&mut s, 404, "text/plain", b"not found"),
        Response::Busy => reply(&mut s, 503, "text/plain", b"busy"),
        Response::Json(json) => reply(&mut s, 200, "application/json", json.as_bytes()),
        Response::File(file) => send_file(&mut s, &file, header("range")),
        Response::Work(work) => reply(&mut s, 200, "application/json", work().as_bytes()),
    }
}

fn reply(s: &mut TcpStream, status: u16, kind: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(
        s,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    s.write_all(body)
}

/// A file, or the byte range the browser asked for.
fn send_file(s: &mut TcpStream, file: &str, range: Option<String>) -> std::io::Result<()> {
    let Ok(mut f) = std::fs::File::open(file) else { return reply(s, 404, "text/plain", b"") };
    let size = f.metadata()?.len();
    if size == 0 {
        return reply(s, 200, "video/mp4", b"");
    }
    let wanted = range.as_deref().and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.split_once('-'));
    let (start, end) = match wanted {
        Some(("", suffix)) => (size.saturating_sub(suffix.parse().unwrap_or(0)), size - 1),
        Some((a, b)) => (a.parse().unwrap_or(0), b.parse().unwrap_or(size - 1).min(size - 1)),
        None => (0, size - 1),
    };
    if start > end {
        return reply(s, 416, "text/plain", b"");
    }
    let status = if wanted.is_some() { "206 Partial Content" } else { "200 OK" };
    write!(
        s,
        "HTTP/1.1 {status}\r\nContent-Type: video/mp4\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{size}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        end - start + 1
    )?;
    f.seek(SeekFrom::Start(start))?;
    std::io::copy(&mut f.take(end - start + 1), s)?;
    Ok(())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = || core::str::from_utf8(bytes.get(i + 1..i + 3)?).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex()) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A JSON string literal.
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The decoded value of `key` in a raw query string like "name=a%20b.mp4&x=1".
pub fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| percent_decode(v)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_escapes() {
        assert_eq!(percent_decode("clip/My%20Game%202026.mp4"), "clip/My Game 2026.mp4");
        assert_eq!(percent_decode("a%zz+b"), "a%zz b");
        assert_eq!(json_str("C:\\x \"y\"\n"), "\"C:\\\\x \\\"y\\\"\\n\"");
        assert_eq!(query_value("for=save&name=a%26b.mp4", "name").as_deref(), Some("a&b.mp4"));
        assert_eq!(query_value("for=save", "name"), None);
    }
}
