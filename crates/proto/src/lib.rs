//! The wire protocol between the airDroid host (the air/OS app) and the
//! daemon that runs as PID 1 inside the Android VM (`airdroid-guest`).
//!
//! Transport: one virtio-serial port (`works.airos.airdroid.0`), which QEMU
//! exposes on the host as a Unix socket. Every message is a frame:
//!
//! ```text
//!   u32 little-endian length | bincode(ToHost or ToGuest)
//! ```
//!
//! Pixels never travel over this channel. QEMU backs guest RAM with a file
//! the host maps too (`/var/shared_memory/...` on air/OS, a ramfs), so a
//! buffer is described once by the guest-physical page ranges that hold it
//! ([`ToHost::BufferNew`]) and afterwards only referenced by id. The guest
//! pins those pages for as long as the buffer exists.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Bumped on any incompatible change; host and guest refuse to talk across
/// versions (they ship together, in one package).
pub const PROTOCOL_VERSION: u32 = 1;

/// virtio-serial port name the guest looks for under /sys/class/virtio-ports.
pub const PORT_NAME: &str = "works.airos.airdroid.0";

/// Largest frame either side accepts; file uploads are chunked below this.
pub const MAX_FRAME: usize = 16 << 20;
/// Chunk size for [`ToGuest::PutFile`].
pub const FILE_CHUNK: usize = 1 << 20;

/// wl_shm formats the guest forwards (values are wl_shm's / DRM fourcc).
pub mod format {
    pub const ARGB8888: u32 = 0;
    pub const XRGB8888: u32 = 1;
    pub const ABGR8888: u32 = 0x3432_4241;
    pub const XBGR8888: u32 = 0x3432_4258;
    pub const RGB565: u32 = 0x3631_5652;

    /// Bytes per pixel, or None for a format the host cannot draw.
    pub fn bytes_per_pixel(f: u32) -> Option<u32> {
        match f {
            ARGB8888 | XRGB8888 | ABGR8888 | XBGR8888 => Some(4),
            RGB565 => Some(2),
            _ => None,
        }
    }

    pub fn has_alpha(f: u32) -> bool {
        matches!(f, ARGB8888 | ABGR8888)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Rect { x, y, w, h }
    }
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.w).min(o.x + o.w);
        let y1 = (self.y + self.h).min(o.y + o.h);
        Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
    }
    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = (self.x + self.w).max(o.x + o.w);
        let y1 = (self.y + self.h).max(o.y + o.h);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}

/// A run of guest-physical memory.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpaRange {
    pub gpa: u64,
    pub len: u32,
}

/// One surface of a window, in paint order (bottom first).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Layer {
    /// Buffer id from [`ToHost::BufferNew`].
    pub buffer: u32,
    /// Source crop in buffer pixels (viewport source, or the whole buffer).
    pub src: Rect,
    /// Destination in window coordinates (surface position and size).
    pub dst: Rect,
    /// The layer covers its destination fully (opaque format, alpha 1).
    pub opaque: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootStage {
    /// The daemon runs; nothing mounted yet.
    Init,
    /// The data disk is blank and being formatted (first start).
    FormattingData,
    /// Images mounted, the container starting.
    StartingAndroid,
    /// Android's `sys.boot_completed` is 1.
    Booted,
    /// The container exited.
    Stopped,
    Failed,
}

/// What a launcher entry needs about an installed Android app.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    pub package: String,
    pub name: String,
    /// `package/class` of the launcher activity.
    pub activity: String,
    /// PNG of the launcher icon, when the guest could get one.
    pub icon_png: Option<Vec<u8>>,
}

/// A notification's picture: GLib image-data layout (RGB or RGBA rows).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NotifyImage {
    pub width: u32,
    pub height: u32,
    pub rowstride: u32,
    pub has_alpha: bool,
    pub data: Vec<u8>,
}

/// Messages from the VM to the host.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ToHost {
    Hello {
        version: u32,
        kernel: String,
    },
    Status {
        stage: BootStage,
        detail: String,
    },
    /// A line of the guest daemon's own log.
    Log {
        text: String,
    },
    /// A shared-memory buffer the host may read from now on. `stride` and
    /// `height` describe the pixels starting at the first range's first byte.
    BufferNew {
        id: u32,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
        ranges: Vec<GpaRange>,
    },
    BufferDestroy {
        id: u32,
    },
    /// A top-level window (an Android task, or the whole UI).
    WindowNew {
        id: u32,
        title: String,
        app_id: String,
        width: u32,
        height: u32,
    },
    WindowUpdate {
        id: u32,
        title: String,
        app_id: String,
        maximized: bool,
    },
    WindowDestroy {
        id: u32,
    },
    /// A new picture for a window. The host answers with
    /// [`ToGuest::FrameDone`] once it no longer needs the buffers' contents.
    Frame {
        window: u32,
        serial: u32,
        width: u32,
        height: u32,
        layers: Vec<Layer>,
        /// Changed area in window coordinates; empty means everything.
        damage: Vec<Rect>,
    },
    /// Pointer image for a window; None hides the pointer.
    Cursor {
        window: u32,
        buffer: Option<u32>,
        hot_x: i32,
        hot_y: i32,
    },
    ExecOutput {
        req: u32,
        stderr: bool,
        data: Vec<u8>,
    },
    ExecDone {
        req: u32,
        code: i32,
    },
    /// Generic answer to a request ([`ToGuest::PutFile`], app actions).
    Reply {
        req: u32,
        ok: bool,
        message: String,
    },
    Apps {
        apps: Vec<AppInfo>,
    },
    Clipboard {
        text: String,
    },
    /// An Android notification for the desktop (Waydroid's INotifications).
    Notify {
        id: u32,
        app_name: String,
        package: String,
        title: String,
        body: String,
        image: Option<NotifyImage>,
    },
    NotifyClose {
        id: u32,
    },
    /// The guest's audio stream (PCM s16le interleaved).
    AudioOpen {
        stream: u32,
        rate: u32,
        channels: u32,
    },
    AudioData {
        stream: u32,
        data: Vec<u8>,
    },
    AudioClose {
        stream: u32,
    },
    /// Android records (a PulseAudio record stream): the host captures its
    /// microphone as PCM s16le interleaved at this rate and channel count
    /// and sends it as ToGuest::AudioIn (silence when it has no input).
    AudioInOpen {
        stream: u32,
        rate: u32,
        channels: u32,
    },
    AudioInClose {
        stream: u32,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchKind {
    Down,
    Motion,
    Up,
    Cancel,
}

/// Messages from the host to the VM.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ToGuest {
    Hello {
        version: u32,
    },
    /// The output Android renders for (a screen, or the full-UI window).
    Output {
        width: u32,
        height: u32,
        refresh_mhz: u32,
        dpi: u32,
    },
    /// The host has consumed `serial` for `window` at `time_ns`
    /// (CLOCK_MONOTONIC of the host; only differences matter).
    FrameDone {
        window: u32,
        serial: u32,
        time_ns: u64,
    },
    PointerMotion {
        window: u32,
        x: f32,
        y: f32,
    },
    /// `button` is a Linux BTN_* code (BTN_LEFT = 0x110).
    PointerButton {
        window: u32,
        button: u32,
        pressed: bool,
    },
    /// Scroll in pixels; `discrete` in wheel clicks when known.
    PointerAxis {
        window: u32,
        dx: f32,
        dy: f32,
        discrete_x: i32,
        discrete_y: i32,
    },
    PointerLeave {
        window: u32,
    },
    Touch {
        window: u32,
        id: i32,
        kind: TouchKind,
        x: f32,
        y: f32,
    },
    /// `code` is a Linux KEY_* code.
    Key {
        window: u32,
        code: u32,
        pressed: bool,
    },
    /// Keyboard focus moved to `window` (None: no airDroid window active).
    Focus {
        window: Option<u32>,
    },
    WindowResize {
        id: u32,
        width: u32,
        height: u32,
    },
    WindowClose {
        id: u32,
    },
    /// Multi-window: the user resized the app's window on the desktop;
    /// its task gets these bounds (display coordinates).
    TaskBounds {
        id: u32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    /// Run a command. `android`: inside the Android container with Android's
    /// environment (like `waydroid shell`), else in the VM's own userland.
    Exec {
        req: u32,
        argv: Vec<String>,
        android: bool,
        /// Run as this user (Android commands; 2000 is adb's shell user).
        uid: Option<u32>,
        stdin: Vec<u8>,
    },
    /// Write a file in the guest (path relative to Android's /data when it
    /// starts with /data/). Chunks arrive in order; `last` closes the file.
    PutFile {
        req: u32,
        path: String,
        offset: u64,
        data: Vec<u8>,
        last: bool,
    },
    /// Install the APK previously put at `path`.
    InstallApk {
        req: u32,
        path: String,
    },
    RemoveApp {
        req: u32,
        package: String,
    },
    /// Open an app in its own window (multi-window) or the full UI.
    LaunchApp {
        req: u32,
        package: String,
    },
    ShowFullUi {
        req: u32,
    },
    /// Open a URI / send an intent (`action`, `uri`).
    LaunchIntent {
        req: u32,
        action: String,
        uri: String,
    },
    ListApps,
    Clipboard {
        text: String,
    },
    SetProp {
        req: u32,
        key: String,
        value: String,
    },
    /// The host played `bytes` of audio stream `stream`: the guest may send
    /// that much more (audio is paced by the host's playback).
    AudioCredit {
        stream: u32,
        bytes: u64,
    },
    /// Captured audio for a record stream (see ToHost::AudioInOpen).
    AudioIn {
        stream: u32,
        data: Vec<u8>,
    },
    Shutdown,
}

/// Write one frame.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = bincode::serialize(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf)?;
    w.flush()
}

/// Read one frame (blocking).
pub fn read_msg<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    bincode::deserialize(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Incremental decoder for non-blocking readers: feed bytes, take messages.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn next<T: for<'de> Deserialize<'de>>(&mut self) -> io::Result<Option<T>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let msg = bincode::deserialize(&self.buf[4..4 + len])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        self.buf.drain(..4 + len);
        msg.map(Some)
    }
}

/// Encode a frame into a byte vector (for non-blocking writers).
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = bincode::serialize(msg).expect("protocol messages serialize");
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_blocking_and_incremental() {
        let msgs = vec![
            ToHost::Hello { version: PROTOCOL_VERSION, kernel: "6.18".into() },
            ToHost::Frame {
                window: 3,
                serial: 9,
                width: 1280,
                height: 800,
                layers: vec![Layer { buffer: 1, src: Rect::new(0, 0, 1280, 800), dst: Rect::new(0, 0, 1280, 800), opaque: true }],
                damage: vec![Rect::new(1, 2, 3, 4)],
            },
        ];
        let mut wire = Vec::new();
        for m in &msgs {
            write_msg(&mut wire, m).unwrap();
        }
        let mut r = &wire[..];
        for m in &msgs {
            let got: ToHost = read_msg(&mut r).unwrap();
            assert_eq!(&got, m);
        }
        let mut d = Decoder::default();
        for chunk in wire.chunks(5) {
            d.feed(chunk);
        }
        for m in &msgs {
            assert_eq!(d.next::<ToHost>().unwrap().as_ref(), Some(m));
        }
        assert!(d.next::<ToHost>().unwrap().is_none());
    }

    #[test]
    fn rects() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), Rect::new(5, 5, 5, 5));
        assert_eq!(a.union(&b), Rect::new(0, 0, 15, 15));
        assert!(Rect::new(20, 20, 1, 1).intersect(&a).is_empty());
    }
}
