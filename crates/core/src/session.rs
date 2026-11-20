//! The connection to the guest daemon over QEMU's virtio-serial socket.

use airdroid_proto::{self as proto, ToGuest, ToHost};
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Session {
    writer: Arc<Mutex<UnixStream>>,
    next_req: Arc<AtomicU32>,
}

impl Session {
    /// Connects (retrying while QEMU starts) and starts a reader thread that
    /// forwards every message to the returned channel.
    pub fn connect(socket: &Path, timeout: Duration) -> io::Result<(Session, Receiver<ToHost>)> {
        let start = Instant::now();
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(s) => break s,
                Err(e) if start.elapsed() < timeout => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e),
            }
        };
        let mut reader = stream.try_clone()?;
        let (tx, rx): (Sender<ToHost>, Receiver<ToHost>) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("airdroid-rx".into())
            .spawn(move || loop {
                match proto::read_msg::<_, ToHost>(&mut reader) {
                    Ok(m) => {
                        if tx.send(m).is_err() {
                            return;
                        }
                    }
                    Err(_) => return, // QEMU went away; the channel closes
                }
            })?;
        let s = Session { writer: Arc::new(Mutex::new(stream)), next_req: Arc::new(AtomicU32::new(1)) };
        s.send(&ToGuest::Hello { version: proto::PROTOCOL_VERSION })?;
        Ok((s, rx))
    }

    pub fn send(&self, msg: &ToGuest) -> io::Result<()> {
        let buf = proto::encode(msg);
        let mut w = self.writer.lock().unwrap();
        io::Write::write_all(&mut *w, &buf)
    }

    pub fn next_req(&self) -> u32 {
        self.next_req.fetch_add(1, Ordering::Relaxed)
    }

    /// Copies a host file into the guest at `guest_path` (under /data/);
    /// returns the request id whose final Reply reports the result.
    pub fn put_file(&self, local: &Path, guest_path: &str) -> io::Result<u32> {
        use std::io::Read;
        let req = self.next_req();
        let mut f = std::fs::File::open(local)?;
        let len = f.metadata()?.len();
        let mut off = 0u64;
        let mut buf = vec![0u8; proto::FILE_CHUNK];
        loop {
            let n = f.read(&mut buf)?;
            let last = off + n as u64 >= len || n == 0;
            self.send(&ToGuest::PutFile { req, path: guest_path.to_string(), offset: off, data: buf[..n].to_vec(), last })?;
            off += n as u64;
            if last {
                return Ok(req);
            }
        }
    }
}
