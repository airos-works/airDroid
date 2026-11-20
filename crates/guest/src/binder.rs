//! A minimal binder client: enough to call Android services from the VM
//! (outside the container) the way Waydroid's Python tools do with gbinder.
//! Synchronous transactions only; parcels in Android 13's format.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

// <linux/android/binder.h>
const fn iow(nr: u64, size: u64) -> u64 {
    (1 << 30) | (size << 16) | ((b'b' as u64) << 8) | nr
}
const fn iowr(nr: u64, size: u64) -> u64 {
    (3 << 30) | (size << 16) | ((b'b' as u64) << 8) | nr
}
const fn io_(c: u8, nr: u64, size: u64, dir: u64) -> u32 {
    ((dir << 30) | (size << 16) | ((c as u64) << 8) | nr) as u32
}
const BINDER_WRITE_READ: u64 = iowr(1, 48);
const BINDER_VERSION: u64 = iowr(9, 4);
const _BINDER_SET_MAX_THREADS: u64 = iow(5, 4);

const TXN_DATA_SIZE: u64 = 64; // struct binder_transaction_data
const BC_TRANSACTION: u32 = io_(b'c', 0, TXN_DATA_SIZE, 1);
const BC_FREE_BUFFER: u32 = io_(b'c', 3, 8, 1);
const BC_REPLY: u32 = io_(b'c', 1, TXN_DATA_SIZE, 1);
const BC_ACQUIRE: u32 = io_(b'c', 5, 4, 1);
const BC_RELEASE: u32 = io_(b'c', 6, 4, 1);
const BC_INCREFS_DONE: u32 = io_(b'c', 8, 16, 1);
const BC_ACQUIRE_DONE: u32 = io_(b'c', 9, 16, 1);
const BC_ENTER_LOOPER: u32 = io_(b'c', 12, 0, 0);
const BR_TRANSACTION: u32 = io_(b'r', 2, TXN_DATA_SIZE, 2);
const TF_ONE_WAY: u32 = 0x01;
const FLAT_BINDER_FLAG_ACCEPTS_FDS: u32 = 0x100;
/// Android 13 ("aidl3") stability of a system binder.
const STABILITY_SYSTEM: i32 = 0x0c;
const BR_ERROR: u32 = io_(b'r', 0, 4, 2);
const BR_OK: u32 = io_(b'r', 1, 0, 0);
const BR_REPLY: u32 = io_(b'r', 3, TXN_DATA_SIZE, 2);
const BR_DEAD_REPLY: u32 = io_(b'r', 5, 0, 0);
const BR_TRANSACTION_COMPLETE: u32 = io_(b'r', 6, 0, 0);
const BR_NOOP: u32 = io_(b'r', 12, 0, 0);
const BR_SPAWN_LOOPER: u32 = io_(b'r', 13, 0, 0);
const BR_FAILED_REPLY: u32 = io_(b'r', 17, 0, 0);
const BR_INCREFS: u32 = io_(b'r', 7, 16, 2);
const BR_ACQUIRE: u32 = io_(b'r', 8, 16, 2);
const BR_RELEASE: u32 = io_(b'r', 9, 16, 2);
const BR_DECREFS: u32 = io_(b'r', 10, 16, 2);

const TF_ACCEPT_FDS: u32 = 0x10;
const BINDER_TYPE_HANDLE: u32 = 0x73682a85; // B_PACK_CHARS('s','h','*',0x85)
const BINDER_TYPE_BINDER: u32 = 0x73622a85;

/// Parcel::writeInterfaceToken's header for system processes ('SYST').
const PARCEL_HEADER_SYST: i32 = 0x53595354;
const STRICT_MODE_PENALTY_GATHER: i32 = 1 << 31;

const MAP_SIZE: usize = 1 << 20;

pub struct Binder {
    fd: OwnedFd,
    map: *mut libc::c_void,
}
unsafe impl Send for Binder {}

impl Drop for Binder {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.map, MAP_SIZE) };
    }
}

/// An outgoing parcel.
#[derive(Default)]
pub struct Parcel {
    pub data: Vec<u8>,
    /// offsets of binder objects in `data`
    pub objects: Vec<u64>,
}

impl Parcel {
    pub fn interface(name: &str) -> Parcel {
        let mut p = Parcel::default();
        p.i32(STRICT_MODE_PENALTY_GATHER);
        p.i32(-1); // work source: unset
        p.i32(PARCEL_HEADER_SYST);
        p.string16(Some(name));
        p
    }
    pub fn i32(&mut self, v: i32) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }
    /// A local binder object (`cookie` identifies it in incoming calls).
    pub fn local_binder(&mut self, cookie: u64) {
        self.objects.push(self.data.len() as u64);
        self.data.extend_from_slice(&BINDER_TYPE_BINDER.to_le_bytes());
        self.data.extend_from_slice(&FLAT_BINDER_FLAG_ACCEPTS_FDS.to_le_bytes());
        self.data.extend_from_slice(&cookie.to_le_bytes()); // binder (node ptr)
        self.data.extend_from_slice(&cookie.to_le_bytes()); // cookie
        self.i32(STABILITY_SYSTEM);
    }
    pub fn string16(&mut self, s: Option<&str>) {
        let Some(s) = s else {
            self.i32(-1);
            return;
        };
        let units: Vec<u16> = s.encode_utf16().collect();
        self.i32(units.len() as i32);
        for u in &units {
            self.data.extend_from_slice(&u.to_le_bytes());
        }
        self.data.extend_from_slice(&[0, 0]);
        while self.data.len() % 4 != 0 {
            self.data.push(0);
        }
    }
}

/// A reply parcel (copied out of the binder buffer).
pub struct Reply {
    pub data: Vec<u8>,
    pos: usize,
    /// Handles of binder objects in the reply (acquired; released on drop).
    pub handles: Vec<u32>,
}

impl Reply {
    /// Skips the RPC header of an incoming call (aidl3: strict mode flags,
    /// work source, 'SYST', interface name); returns the interface.
    pub fn rpc_header(&mut self) -> io::Result<String> {
        self.i32()?;
        self.i32()?;
        self.i32()?;
        Ok(self.string16()?.unwrap_or_default())
    }
    pub fn bool(&mut self) -> io::Result<bool> {
        Ok(self.i32()? != 0)
    }
    pub fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.i32()?;
        if n < 0 {
            return Ok(Vec::new());
        }
        let n = n as usize;
        let b = self.data.get(self.pos..self.pos + n).ok_or_else(|| io::Error::other("parcel too short"))?.to_vec();
        self.pos = (self.pos + n + 3) & !3;
        Ok(b)
    }
    /// Skips a binder object (flat_binder_object + stability).
    pub fn skip_object(&mut self) -> io::Result<()> {
        if self.pos + 28 > self.data.len() {
            return Err(io::Error::other("parcel too short"));
        }
        self.pos += 28;
        Ok(())
    }
    pub fn i32(&mut self) -> io::Result<i32> {
        let b = self.data.get(self.pos..self.pos + 4).ok_or_else(|| io::Error::other("parcel too short"))?;
        self.pos += 4;
        Ok(i32::from_le_bytes(b.try_into().unwrap()))
    }
    pub fn string16(&mut self) -> io::Result<Option<String>> {
        let len = self.i32()?;
        if len < 0 {
            return Ok(None);
        }
        let n = len as usize;
        let bytes = self.data.get(self.pos..self.pos + n * 2).ok_or_else(|| io::Error::other("parcel too short"))?;
        let units: Vec<u16> = bytes.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        self.pos += (n + 1) * 2;
        self.pos = (self.pos + 3) & !3;
        Ok(Some(String::from_utf16_lossy(&units)))
    }
    /// The status every AIDL reply starts with (exception code; 0 = ok).
    pub fn status(&mut self) -> io::Result<()> {
        let ex = self.i32()?;
        if ex != 0 {
            return Err(io::Error::other(format!("remote exception {ex}")));
        }
        Ok(())
    }
    /// A strong binder (flat_binder_object + Android 12+'s stability word):
    /// its handle, or None for null.
    pub fn strong_binder(&mut self) -> io::Result<Option<u32>> {
        let b = self.data.get(self.pos..self.pos + 24).ok_or_else(|| io::Error::other("parcel too short"))?.to_vec();
        self.pos += 24;
        let _stability = self.i32();
        let kind = u32::from_le_bytes(b[0..4].try_into().unwrap());
        let handle = u32::from_le_bytes(b[8..12].try_into().unwrap());
        match kind {
            BINDER_TYPE_HANDLE => Ok(Some(handle)),
            BINDER_TYPE_BINDER => Ok(None),
            _ => Err(io::Error::other(format!("unexpected binder object type {kind:#x}"))),
        }
    }
}

#[repr(C)]
struct WriteRead {
    write_size: u64,
    write_consumed: u64,
    write_buffer: u64,
    read_size: u64,
    read_consumed: u64,
    read_buffer: u64,
}

impl Binder {
    pub fn open(path: &str) -> io::Result<Binder> {
        let c = crate::sys::cstr(path);
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut version: i32 = 0;
        if unsafe { libc::ioctl(fd.as_raw_fd(), BINDER_VERSION as _, &mut version) } < 0 || version != 8 {
            return Err(io::Error::other(format!("binder protocol {version}, want 8")));
        }
        let map = unsafe { libc::mmap(std::ptr::null_mut(), MAP_SIZE, libc::PROT_READ, libc::MAP_PRIVATE | libc::MAP_NORESERVE, fd.as_raw_fd(), 0) };
        if map == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Binder { fd, map })
    }

    fn write_read(&self, write: &[u8], read: &mut [u8]) -> io::Result<usize> {
        let mut wr = WriteRead {
            write_size: write.len() as u64,
            write_consumed: 0,
            write_buffer: write.as_ptr() as u64,
            read_size: read.len() as u64,
            read_consumed: 0,
            read_buffer: read.as_mut_ptr() as u64,
        };
        loop {
            let r = unsafe { libc::ioctl(self.fd.as_raw_fd(), BINDER_WRITE_READ as _, &mut wr) };
            if r >= 0 {
                return Ok(wr.read_consumed as usize);
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(e);
            }
        }
    }

    fn command(&self, cmd: u32, arg: &[u8]) -> io::Result<()> {
        let mut w = Vec::with_capacity(4 + arg.len());
        w.extend_from_slice(&cmd.to_le_bytes());
        w.extend_from_slice(arg);
        self.write_read(&w, &mut [])?;
        Ok(())
    }

    /// A synchronous transaction to `handle`; the reply's binder objects
    /// are acquired (they outlive the buffer, which is freed here).
    pub fn transact(&self, handle: u32, code: u32, p: &Parcel) -> io::Result<Reply> {
        let mut w = Vec::with_capacity(4 + TXN_DATA_SIZE as usize);
        w.extend_from_slice(&BC_TRANSACTION.to_le_bytes());
        w.extend_from_slice(&(handle as u64).to_le_bytes()); // target
        w.extend_from_slice(&0u64.to_le_bytes()); // cookie
        w.extend_from_slice(&code.to_le_bytes());
        w.extend_from_slice(&TF_ACCEPT_FDS.to_le_bytes());
        w.extend_from_slice(&0i32.to_le_bytes()); // sender pid
        w.extend_from_slice(&0u32.to_le_bytes()); // sender euid
        w.extend_from_slice(&(p.data.len() as u64).to_le_bytes());
        w.extend_from_slice(&((p.objects.len() * 8) as u64).to_le_bytes()); // offsets size
        w.extend_from_slice(&(p.data.as_ptr() as u64).to_le_bytes());
        w.extend_from_slice(&(p.objects.as_ptr() as u64).to_le_bytes()); // offsets
        let mut read = vec![0u8; 4096];
        let mut first = true;
        loop {
            let n = if first { self.write_read(&w, &mut read)? } else { self.write_read(&[], &mut read)? };
            first = false;
            let mut i = 0;
            while i + 4 <= n {
                let cmd = u32::from_le_bytes(read[i..i + 4].try_into().unwrap());
                i += 4;
                match cmd {
                    BR_NOOP | BR_TRANSACTION_COMPLETE | BR_SPAWN_LOOPER | BR_OK => {}
                    BR_INCREFS | BR_ACQUIRE | BR_RELEASE | BR_DECREFS => i += 16,
                    BR_ERROR => {
                        let e = i32::from_le_bytes(read[i..i + 4].try_into().unwrap());
                        return Err(io::Error::other(format!("binder error {e}")));
                    }
                    BR_DEAD_REPLY => return Err(io::Error::other("binder: dead reply")),
                    BR_FAILED_REPLY => return Err(io::Error::other("binder: failed reply")),
                    BR_REPLY => {
                        // struct binder_transaction_data: target, cookie, code,
                        // flags, sender pid/euid, then at 32: data_size,
                        // offsets_size, data.ptr.buffer, data.ptr.offsets
                        let t = &read[i..i + TXN_DATA_SIZE as usize];
                        let u64_at = |o: usize| u64::from_le_bytes(t[o..o + 8].try_into().unwrap());
                        let (size, offs_size, buf, offs) = (u64_at(32) as usize, u64_at(40) as usize, u64_at(48), u64_at(56));
                        let data = unsafe { std::slice::from_raw_parts(buf as *const u8, size) }.to_vec();
                        // acquire the handles of binder objects before the
                        // buffer (which holds their references) is freed
                        let mut handles = Vec::new();
                        for k in 0..offs_size / 8 {
                            let o = unsafe { std::ptr::read_unaligned((offs as *const u64).add(k)) } as usize;
                            if o + 12 <= data.len() && u32::from_le_bytes(data[o..o + 4].try_into().unwrap()) == BINDER_TYPE_HANDLE {
                                let h = u32::from_le_bytes(data[o + 8..o + 12].try_into().unwrap());
                                self.command(BC_ACQUIRE, &h.to_le_bytes())?;
                                handles.push(h);
                            }
                        }
                        self.command(BC_FREE_BUFFER, &buf.to_le_bytes())?;
                        return Ok(Reply { data, pos: 0, handles });
                    }
                    other => return Err(io::Error::other(format!("binder: unexpected return {other:#x}"))),
                }
            }
        }
    }

    /// IServiceManager.addService(name, our object `cookie`).
    pub fn add_service(&self, name: &str, cookie: u64) -> io::Result<()> {
        let mut p = Parcel::interface("android.os.IServiceManager");
        p.string16(Some(name));
        p.local_binder(cookie);
        p.i32(0); // allowIsolated
        p.i32(8); // DUMP_FLAG_PRIORITY_DEFAULT
        let mut r = self.transact(0, 3, &p)?;
        r.status()
    }

    /// Serves incoming transactions on the calling thread forever:
    /// `handle(cookie, code, request)` returns the reply parcel (None for
    /// an error). Reference counting commands are acknowledged.
    pub fn serve(&self, mut handle: impl FnMut(u64, u32, &mut Reply) -> Option<Parcel>) -> io::Result<()> {
        self.command(BC_ENTER_LOOPER, &[])?;
        let mut read = vec![0u8; 8192];
        loop {
            let n = self.write_read(&[], &mut read)?;
            let mut i = 0;
            while i + 4 <= n {
                let cmd = u32::from_le_bytes(read[i..i + 4].try_into().unwrap());
                i += 4;
                match cmd {
                    BR_NOOP | BR_TRANSACTION_COMPLETE | BR_SPAWN_LOOPER | BR_OK => {}
                    BR_INCREFS | BR_ACQUIRE => {
                        let ptrs = read[i..i + 16].to_vec();
                        i += 16;
                        self.command(if cmd == BR_INCREFS { BC_INCREFS_DONE } else { BC_ACQUIRE_DONE }, &ptrs)?;
                    }
                    BR_RELEASE | BR_DECREFS => i += 16,
                    BR_ERROR => i += 4,
                    BR_TRANSACTION => {
                        let t = read[i..i + TXN_DATA_SIZE as usize].to_vec();
                        i += TXN_DATA_SIZE as usize;
                        let u64_at = |o: usize| u64::from_le_bytes(t[o..o + 8].try_into().unwrap());
                        let u32_at = |o: usize| u32::from_le_bytes(t[o..o + 4].try_into().unwrap());
                        let cookie = u64_at(8);
                        let code = u32_at(16);
                        let flags = u32_at(20);
                        let (size, buf) = (u64_at(32) as usize, u64_at(48));
                        let data = unsafe { std::slice::from_raw_parts(buf as *const u8, size) }.to_vec();
                        self.command(BC_FREE_BUFFER, &buf.to_le_bytes())?;
                        let mut req = Reply { data, pos: 0, handles: Vec::new() };
                        let reply = handle(cookie, code, &mut req);
                        if flags & TF_ONE_WAY == 0 {
                            let p = reply.unwrap_or_else(|| {
                                let mut p = Parcel::default();
                                p.i32(-1); // EX_SECURITY-ish: refuse
                                p
                            });
                            let mut w = Vec::with_capacity(4 + TXN_DATA_SIZE as usize);
                            w.extend_from_slice(&BC_REPLY.to_le_bytes());
                            w.extend_from_slice(&0u64.to_le_bytes());
                            w.extend_from_slice(&0u64.to_le_bytes());
                            w.extend_from_slice(&0u32.to_le_bytes());
                            w.extend_from_slice(&TF_ACCEPT_FDS.to_le_bytes());
                            w.extend_from_slice(&0i32.to_le_bytes());
                            w.extend_from_slice(&0u32.to_le_bytes());
                            w.extend_from_slice(&(p.data.len() as u64).to_le_bytes());
                            w.extend_from_slice(&0u64.to_le_bytes());
                            w.extend_from_slice(&(p.data.as_ptr() as u64).to_le_bytes());
                            w.extend_from_slice(&0u64.to_le_bytes());
                            self.write_read(&w, &mut [])?;
                        }
                    }
                    _ => {
                        // a command we do not decode: its size is in its code
                        i += ((cmd >> 16) & 0x3fff) as usize;
                    }
                }
            }
        }
    }

    pub fn release(&self, handle: u32) {
        let _ = self.command(BC_RELEASE, &handle.to_le_bytes());
    }

    /// IServiceManager.checkService(name): the service's handle.
    pub fn check_service(&self, name: &str) -> io::Result<Option<u32>> {
        let mut p = Parcel::interface("android.os.IServiceManager");
        p.string16(Some(name));
        // AIDL IServiceManager (Android 13): getService = 1, checkService = 2
        let mut r = self.transact(0, 2, &p)?;
        r.status()?;
        let h = r.strong_binder()?;
        // keep only the handle we return acquired
        for x in &r.handles {
            if Some(*x) != h {
                self.release(*x);
            }
        }
        Ok(h)
    }
}
