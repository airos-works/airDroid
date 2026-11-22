//! Safe wrappers over the Be API glue (native/host.cpp). Off air/OS the
//! functions are no-ops so the engine still builds and tests elsewhere.
#![allow(unsafe_code)]

use std::ffi::CString;

/// Keep in step with `AdEvent` in native/host.cpp.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct AdEvent {
    pub kind: i32,
    pub window: u32,
    pub buttons: i32,
    pub clicks: i32,
    pub modifiers: u32,
    pub key: i32,
    pub repeat: i32,
    pub x: f32,
    pub y: f32,
    pub dx: f32,
    pub dy: f32,
    pub text_len: u32,
}

pub mod kind {
    pub const MOUSE_MOVED: i32 = 1;
    pub const MOUSE_DOWN: i32 = 2;
    pub const MOUSE_UP: i32 = 3;
    pub const MOUSE_EXITED: i32 = 4;
    pub const WHEEL: i32 = 5;
    pub const KEY_DOWN: i32 = 6;
    pub const KEY_UP: i32 = 7;
    pub const MODIFIERS: i32 = 8;
    pub const WINDOW_CLOSE: i32 = 9;
    pub const WINDOW_RESIZED: i32 = 10;
    pub const WINDOW_ACTIVATED: i32 = 11;
    pub const WINDOW_DEACTIVATED: i32 = 12;
    pub const REFS: i32 = 13;
    pub const ARGV: i32 = 14;
    pub const QUIT: i32 = 15;
    pub const ACTION: i32 = 16;
    pub const APP_CLICKED: i32 = 17;
    pub const CLIPBOARD: i32 = 18;
    pub const TASK_BOUNDS: i32 = 19;
}

pub mod action {
    pub const SHOW_FULL_UI: i32 = 1;
    pub const INSTALL_APK: i32 = 2;
    pub const STOP: i32 = 3;
    pub const START: i32 = 4;
    pub const REMOVE_APP: i32 = 5;
    pub const SETUP: i32 = 6;
    pub const TOGGLE_MULTIWINDOW: i32 = 7;
    pub const SETTINGS: i32 = 8;
    pub const SAVE_SETTINGS: i32 = 9;
    pub const NEW_DATA: i32 = 10;
}

/// An event with its text payload (paths, arguments, clipboard text).
#[derive(Debug, Clone)]
pub struct Event {
    pub ev: AdEvent,
    pub text: String,
}

fn c(s: &str) -> CString {
    CString::new(s.replace('\0', " ")).unwrap()
}

#[cfg(target_os = "haiku")]
mod ffi {
    use super::AdEvent;
    extern "C" {
        pub fn ad_host_init(signature: *const libc::c_char) -> i32;
        pub fn ad_host_run();
        pub fn ad_host_quit();
        pub fn ad_host_wait(timeout_us: i64) -> i32;
        pub fn ad_host_wake();
        pub fn ad_host_next_event(ev: *mut AdEvent) -> i32;
        pub fn ad_host_event_text(buf: *mut u8, len: usize) -> usize;
        pub fn ad_window_open(id: u32, title: *const libc::c_char, x: i32, y: i32, w: i32, h: i32, borderless: i32);
        pub fn ad_window_move(id: u32, x: i32, y: i32);
        pub fn ad_window_close(id: u32);
        pub fn ad_window_set_title(id: u32, title: *const libc::c_char);
        pub fn ad_window_resize(id: u32, w: i32, h: i32);
        pub fn ad_window_frame_begin(id: u32, width: i32, height: i32, area: *mut i32) -> *mut u32;
        pub fn ad_window_frame_end(id: u32);
        pub fn ad_window_present(id: u32, pixels: *const u32, w: i32, h: i32);
        pub fn ad_window_activate(id: u32);
        pub fn ad_window_hide_cursor(id: u32, hidden: i32);
        pub fn ad_main_show();
        pub fn ad_main_set_status(text: *const libc::c_char, progress: f32);
        pub fn ad_main_set_multiwindow(on: i32);
        pub fn ad_main_set_apps(
            count: i32,
            packages: *const *const libc::c_char,
            names: *const *const libc::c_char,
            icons: *const *const u8,
            icon_lens: *const usize,
        );
        pub fn ad_open_file_panel();
        pub fn ad_alert(title: *const libc::c_char, text: *const libc::c_char);
        pub fn ad_clipboard_set(text: *const libc::c_char);
        pub fn ad_make_launcher(path: *const libc::c_char, app: *const libc::c_char, package: *const libc::c_char, png: *const u8, len: usize) -> i32;
        pub fn ad_app_path(buf: *mut libc::c_char, len: usize) -> i32;
        pub fn ad_timezone(buf: *mut libc::c_char, len: usize) -> i32;
        pub fn ad_screen_size(w: *mut i32, h: *mut i32) -> f32;
        pub fn ad_register_types(signature: *const libc::c_char);
        pub fn ad_audio_open(id: u32, rate: u32, channels: u32);
        pub fn ad_audio_write(id: u32, data: *const u8, len: usize);
        pub fn ad_audio_take_consumed(id: u32) -> u64;
        pub fn ad_audio_close(id: u32);
        pub fn ad_mic_open(id: u32, rate: u32, channels: u32) -> i32;
        pub fn ad_mic_take(id: u32, out: *mut i16, max: usize) -> usize;
        pub fn ad_mic_close(id: u32);
        pub fn ad_mic_restart() -> i32;
        pub fn ad_settings_show(ram_mb: u32, cpus: u32, gpu: u32, dpi: u32, touch: i32, share: *const libc::c_char);
        pub fn ad_notify(
            id: u32,
            app: *const libc::c_char,
            package: *const libc::c_char,
            title: *const libc::c_char,
            body: *const libc::c_char,
            image: *const u8,
            width: i32,
            height: i32,
            rowstride: i32,
            has_alpha: i32,
            png: *const u8,
            png_len: usize,
        );
    }
}

#[cfg(target_os = "haiku")]
mod imp {
    use super::*;

    pub fn init(signature: &str) -> bool {
        unsafe { ffi::ad_host_init(c(signature).as_ptr()) == 0 }
    }
    pub fn run() {
        unsafe { ffi::ad_host_run() }
    }
    pub fn quit() {
        unsafe { ffi::ad_host_quit() }
    }
    pub fn wake() {
        unsafe { ffi::ad_host_wake() }
    }
    /// Blocks up to `timeout_us` (-1: forever) for the next event.
    pub fn next_event(timeout_us: i64) -> Option<Event> {
        unsafe {
            if ffi::ad_host_wait(timeout_us) == 0 {
                return None;
            }
            let mut ev = AdEvent::default();
            if ffi::ad_host_next_event(&mut ev) == 0 {
                return None;
            }
            let mut text = String::new();
            if ev.text_len > 0 {
                let mut buf = vec![0u8; ev.text_len as usize];
                ffi::ad_host_event_text(buf.as_mut_ptr(), buf.len());
                text = String::from_utf8_lossy(&buf).into_owned();
            }
            Some(Event { ev, text })
        }
    }
    /// `at`: screen position, or None to place it; `borderless`: Android
    /// draws the caption.
    pub fn window_open(id: u32, title: &str, at: Option<(i32, i32)>, w: u32, h: u32, borderless: bool) {
        let (x, y) = at.unwrap_or((-1, -1));
        unsafe { ffi::ad_window_open(id, c(title).as_ptr(), x, y, w as i32, h as i32, borderless as i32) }
    }
    pub fn window_move(id: u32, x: i32, y: i32) {
        unsafe { ffi::ad_window_move(id, x, y) }
    }
    pub fn window_close(id: u32) {
        unsafe { ffi::ad_window_close(id) }
    }
    pub fn window_set_title(id: u32, title: &str) {
        unsafe { ffi::ad_window_set_title(id, c(title).as_ptr()) }
    }
    pub fn window_resize(id: u32, w: u32, h: u32) {
        unsafe { ffi::ad_window_resize(id, w as i32, h as i32) }
    }
    /// Composes a frame straight into the window's back bitmap (`f` gets
    /// its w*h pixels) and shows it; false when there is no bitmap.
    /// Lets `f` write window `id`'s next frame, `w` x `h` packed B_RGB32
    /// rows, straight into its back bitmap; `f` also gets the cloneable area
    /// the rows start at (or -1). False if the window has no such bitmap.
    pub fn window_frame(id: u32, w: u32, h: u32, f: impl FnOnce(&mut [u32], i32)) -> bool {
        let mut area = -1;
        let p = unsafe { ffi::ad_window_frame_begin(id, w as i32, h as i32, &mut area) };
        if p.is_null() {
            return false;
        }
        f(unsafe { std::slice::from_raw_parts_mut(p, w as usize * h as usize) }, area);
        unsafe { ffi::ad_window_frame_end(id) };
        true
    }
    pub fn window_present(id: u32, pixels: &[u32], w: u32, h: u32) {
        debug_assert!(pixels.len() >= (w * h) as usize);
        unsafe { ffi::ad_window_present(id, pixels.as_ptr(), w as i32, h as i32) }
    }
    pub fn window_activate(id: u32) {
        unsafe { ffi::ad_window_activate(id) }
    }
    pub fn window_hide_cursor(id: u32, hidden: bool) {
        unsafe { ffi::ad_window_hide_cursor(id, hidden as i32) }
    }
    pub fn main_show() {
        unsafe { ffi::ad_main_show() }
    }
    /// `progress` in 0..=1, or negative to hide the bar.
    pub fn main_set_status(text: &str, progress: f32) {
        unsafe { ffi::ad_main_set_status(c(text).as_ptr(), progress) }
    }
    pub fn main_set_multiwindow(on: bool) {
        unsafe { ffi::ad_main_set_multiwindow(on as i32) }
    }
    pub fn main_set_apps(apps: &[(String, String, Option<Vec<u8>>)]) {
        let pk: Vec<CString> = apps.iter().map(|a| c(&a.0)).collect();
        let nm: Vec<CString> = apps.iter().map(|a| c(&a.1)).collect();
        let pkp: Vec<*const libc::c_char> = pk.iter().map(|s| s.as_ptr()).collect();
        let nmp: Vec<*const libc::c_char> = nm.iter().map(|s| s.as_ptr()).collect();
        let icp: Vec<*const u8> = apps.iter().map(|a| a.2.as_ref().map_or(std::ptr::null(), |v| v.as_ptr())).collect();
        let icl: Vec<usize> = apps.iter().map(|a| a.2.as_ref().map_or(0, |v| v.len())).collect();
        unsafe { ffi::ad_main_set_apps(apps.len() as i32, pkp.as_ptr(), nmp.as_ptr(), icp.as_ptr(), icl.as_ptr()) }
    }
    pub fn open_file_panel() {
        unsafe { ffi::ad_open_file_panel() }
    }
    pub fn alert(title: &str, text: &str) {
        unsafe { ffi::ad_alert(c(title).as_ptr(), c(text).as_ptr()) }
    }
    pub fn clipboard_set(text: &str) {
        unsafe { ffi::ad_clipboard_set(c(text).as_ptr()) }
    }
    pub fn make_launcher(path: &std::path::Path, app: &str, package: &str, png: Option<&[u8]>) -> bool {
        let p = c(&path.to_string_lossy());
        let (ptr, len) = png.map_or((std::ptr::null(), 0), |v| (v.as_ptr(), v.len()));
        unsafe { ffi::ad_make_launcher(p.as_ptr(), c(app).as_ptr(), c(package).as_ptr(), ptr, len) == 0 }
    }
    /// air/OS's time zone, an Olson ID.
    pub fn timezone() -> Option<String> {
        let mut buf = vec![0 as libc::c_char; 128];
        unsafe {
            if ffi::ad_timezone(buf.as_mut_ptr(), buf.len()) != 0 {
                return None;
            }
            Some(std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
        }
    }
    pub fn app_path() -> Option<String> {
        let mut buf = vec![0 as libc::c_char; 1024];
        unsafe {
            if ffi::ad_app_path(buf.as_mut_ptr(), buf.len()) != 0 {
                return None;
            }
            Some(std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
        }
    }
    pub fn register_types(signature: &str) {
        unsafe { ffi::ad_register_types(c(signature).as_ptr()) }
    }
    pub fn audio_open(id: u32, rate: u32, channels: u32) {
        unsafe { ffi::ad_audio_open(id, rate, channels) }
    }
    pub fn audio_write(id: u32, data: &[u8]) {
        unsafe { ffi::ad_audio_write(id, data.as_ptr(), data.len()) }
    }
    /// Bytes played since the last call.
    pub fn audio_take_consumed(id: u32) -> u64 {
        unsafe { ffi::ad_audio_take_consumed(id) }
    }
    pub fn audio_close(id: u32) {
        unsafe { ffi::ad_audio_close(id) }
    }
    /// The system's audio input for a record stream; false without one.
    pub fn mic_open(id: u32, rate: u32, channels: u32) -> bool {
        unsafe { ffi::ad_mic_open(id, rate, channels) != 0 }
    }
    /// Captured samples (s16) waiting, as little-endian bytes.
    pub fn mic_take(id: u32) -> Vec<u8> {
        let mut buf = vec![0i16; 16384];
        let n = unsafe { ffi::ad_mic_take(id, buf.as_mut_ptr(), buf.len()) };
        buf[..n].iter().flat_map(|s| s.to_le_bytes()).collect()
    }
    pub fn mic_close(id: u32) {
        unsafe { ffi::ad_mic_close(id) }
    }
    /// Starts the audio input again (something else stopped it).
    pub fn mic_restart() -> bool {
        unsafe { ffi::ad_mic_restart() != 0 }
    }
    pub fn settings_show(s: &crate::settings::Settings) {
        let gpu = match s.gpu.as_str() {
            "virgl" => 1,
            "none" => 2,
            "venus" => 3,
            _ => 0,
        };
        unsafe { ffi::ad_settings_show(s.ram_mb as u32, s.cpus, gpu, s.dpi, s.mouse_as_touch as i32, c(&s.shared_folder).as_ptr()) }
    }
    pub fn notify(id: u32, app: &str, package: &str, title: &str, body: &str, image: Option<&airdroid_core::proto::NotifyImage>, png: Option<&[u8]>) {
        let (ip, w, h, rs, a) = match image {
            Some(i) if i.data.len() >= (i.rowstride as usize) * (i.height as usize) => (i.data.as_ptr(), i.width as i32, i.height as i32, i.rowstride as i32, i.has_alpha as i32),
            _ => (std::ptr::null(), 0, 0, 0, 0),
        };
        let (pp, pl) = png.map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len()));
        unsafe {
            ffi::ad_notify(id, c(app).as_ptr(), c(package).as_ptr(), c(title).as_ptr(), c(body).as_ptr(), ip, w, h, rs, a, pp, pl)
        }
    }
    pub fn screen_size() -> (u32, u32) {
        let (mut w, mut h) = (0, 0);
        unsafe { ffi::ad_screen_size(&mut w, &mut h) };
        (w.max(640) as u32, h.max(480) as u32)
    }
}

#[cfg(not(target_os = "haiku"))]
#[allow(unused_variables)]
mod imp {
    use super::*;
    pub fn init(signature: &str) -> bool {
        let _ = c(signature);
        true
    }
    pub fn run() {}
    pub fn quit() {}
    pub fn wake() {}
    pub fn next_event(timeout_us: i64) -> Option<Event> {
        std::thread::sleep(std::time::Duration::from_millis(100));
        None
    }
    pub fn window_open(id: u32, title: &str, at: Option<(i32, i32)>, w: u32, h: u32, borderless: bool) {}
    pub fn window_move(id: u32, x: i32, y: i32) {}
    pub fn window_close(id: u32) {}
    pub fn window_set_title(id: u32, title: &str) {}
    pub fn window_resize(id: u32, w: u32, h: u32) {}
    pub fn window_frame(id: u32, w: u32, h: u32, f: impl FnOnce(&mut [u32], i32)) -> bool {
        false
    }
    pub fn window_present(id: u32, pixels: &[u32], w: u32, h: u32) {}
    pub fn window_activate(id: u32) {}
    pub fn window_hide_cursor(id: u32, hidden: bool) {}
    pub fn main_show() {}
    pub fn main_set_status(text: &str, progress: f32) {
        eprintln!("status: {text}");
    }
    pub fn main_set_multiwindow(on: bool) {}
    pub fn main_set_apps(apps: &[(String, String, Option<Vec<u8>>)]) {}
    pub fn open_file_panel() {}
    pub fn alert(title: &str, text: &str) {
        eprintln!("{title}: {text}");
    }
    pub fn clipboard_set(text: &str) {}
    pub fn make_launcher(path: &std::path::Path, app: &str, package: &str, png: Option<&[u8]>) -> bool {
        false
    }
    /// The host's time zone: /etc/localtime's target under zoneinfo/.
    pub fn timezone() -> Option<String> {
        let t = std::fs::read_link("/etc/localtime").ok()?;
        let t = t.to_string_lossy();
        t.split_once("zoneinfo/").map(|(_, z)| z.to_string())
    }
    pub fn app_path() -> Option<String> {
        std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned())
    }
    pub fn register_types(signature: &str) {}
    pub fn audio_open(id: u32, rate: u32, channels: u32) {}
    pub fn audio_write(id: u32, data: &[u8]) {}
    pub fn audio_take_consumed(id: u32) -> u64 {
        0
    }
    pub fn audio_close(id: u32) {}
    pub fn mic_open(id: u32, rate: u32, channels: u32) -> bool {
        false
    }
    pub fn mic_take(id: u32) -> Vec<u8> {
        Vec::new()
    }
    pub fn mic_close(id: u32) {}
    pub fn mic_restart() -> bool {
        false
    }
    pub fn settings_show(s: &crate::settings::Settings) {}
    pub fn notify(id: u32, app: &str, package: &str, title: &str, body: &str, image: Option<&airdroid_core::proto::NotifyImage>, png: Option<&[u8]>) {
        eprintln!("notification {id} {app}: {title}: {body}");
    }
    pub fn screen_size() -> (u32, u32) {
        (1920, 1080)
    }
}

pub use imp::*;
