//! The virtio-serial port to the airDroid host.

use airdroid_proto::{self as proto, ToHost};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};

/// Finds the port's /dev node by its name (no udev in the VM).
pub fn find_port(name: &str) -> Option<String> {
    for entry in std::fs::read_dir("/sys/class/virtio-ports").ok()?.flatten() {
        let n = std::fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if n.trim() == name {
            return Some(format!("/dev/{}", entry.file_name().to_string_lossy()));
        }
    }
    None
}

/// Messages go out through a writer thread: a write to the port waits
/// (in the kernel, spinning) until QEMU has taken it, and QEMU can be busy
/// for seconds (a shader compile in virglrenderer). The compositor must
/// not wait with it: Android's hardware composer aborts when its Wayland
/// requests are not read for about a second.
#[derive(Clone)]
pub struct HostLink {
    tx: mpsc::Sender<Vec<u8>>,
    pending: Arc<(Mutex<usize>, Condvar)>,
}

static LINK: OnceLock<HostLink> = OnceLock::new();

impl HostLink {
    /// Opens the port; returns the link and a reader for the host's messages.
    pub fn open() -> io::Result<(HostLink, File)> {
        let path = find_port(proto::PORT_NAME)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no airDroid virtio-serial port"))?;
        let mut f = OpenOptions::new().read(true).write(true).open(&path)?;
        let reader = f.try_clone()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let pending = Arc::new((Mutex::new(0usize), Condvar::new()));
        let done = pending.clone();
        std::thread::Builder::new().name("host-tx".into()).spawn(move || {
            for buf in rx {
                // virtio-serial blocks while the host side is not connected;
                // that is the behaviour we want (the host is the only
                // consumer), and the order of the messages is kept.
                if let Err(e) = f.write_all(&buf) {
                    console(&format!("airdroid: host link write: {e}"));
                }
                *done.0.lock().unwrap() -= 1;
                done.1.notify_all();
            }
        })?;
        let link = HostLink { tx, pending };
        let _ = LINK.set(link.clone());
        Ok((link, reader))
    }

    pub fn send(&self, msg: &ToHost) {
        *self.pending.0.lock().unwrap() += 1;
        if self.tx.send(proto::encode(msg)).is_err() {
            *self.pending.0.lock().unwrap() -= 1;
        }
    }

    /// Waits (at most `timeout`) until everything sent so far is written.
    pub fn flush(&self, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        let mut n = self.pending.0.lock().unwrap();
        while *n > 0 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            n = self.pending.1.wait_timeout(n, left).unwrap().0;
        }
    }
}

pub fn link() -> Option<&'static HostLink> {
    LINK.get()
}

/// Write to the VM console (the host keeps it as console.log).
pub fn console(text: &str) {
    if let Ok(mut c) = OpenOptions::new().write(true).open("/dev/console") {
        let _ = writeln!(c, "{text}");
    } else {
        eprintln!("{text}");
    }
}

/// Log to the console and, once connected, to the host.
pub fn log(text: &str) {
    console(&format!("airdroid: {text}"));
    if let Some(l) = link() {
        l.send(&ToHost::Log { text: text.to_string() });
    }
}

#[macro_export]
macro_rules! logf {
    ($($arg:tt)*) => { $crate::hostlink::log(&format!($($arg)*)) };
}
