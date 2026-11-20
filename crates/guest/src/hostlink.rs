//! The virtio-serial port to the airDroid host.

use airdroid_proto::{self as proto, ToHost};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex, OnceLock};

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

#[derive(Clone)]
pub struct HostLink {
    writer: Arc<Mutex<File>>,
}

static LINK: OnceLock<HostLink> = OnceLock::new();

impl HostLink {
    /// Opens the port; returns the link and a reader for the host's messages.
    pub fn open() -> io::Result<(HostLink, File)> {
        let path = find_port(proto::PORT_NAME)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no airDroid virtio-serial port"))?;
        let f = OpenOptions::new().read(true).write(true).open(&path)?;
        let reader = f.try_clone()?;
        let link = HostLink { writer: Arc::new(Mutex::new(f)) };
        let _ = LINK.set(link.clone());
        Ok((link, reader))
    }

    pub fn send(&self, msg: &ToHost) {
        let buf = proto::encode(msg);
        let mut w = self.writer.lock().unwrap();
        // virtio-serial blocks while the host side is not connected; that is
        // the behaviour we want (the host is the only consumer).
        if let Err(e) = w.write_all(&buf) {
            console(&format!("airdroid: host link write: {e}"));
        }
    }

    pub fn raw_fd(&self) -> i32 {
        self.writer.lock().unwrap().as_raw_fd()
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
