//! The guest's RAM, readable by the host.
//!
//! * Linux/KVM: QEMU backs guest RAM with a shared file (/dev/shm); we map
//!   the same file and translate guest-physical addresses with QEMU's q35
//!   layout.
//! * air/OS/NVMM: NVMM replaces QEMU's RAM mappings with its own VM objects,
//!   so a shared file does not work (and NVMM faulting a ramfs-backed area
//!   panicked the kernel). airDroid's QEMU (qemu/patches/0101) makes those
//!   areas cloneable and logs every guest-physical RAM section; we replay
//!   the log and clone_area() each area.

use airdroid_proto::GpaRange;
use std::io;
use std::path::Path;

struct Segment {
    gpa: u64,
    len: u64,
    ptr: *const u8,
}

pub struct GuestMem {
    segs: Vec<Segment>,
    cleanup: Cleanup,
}
unsafe impl Send for GuestMem {}
unsafe impl Sync for GuestMem {}

enum Cleanup {
    Munmap(*mut libc::c_void, usize),
    #[cfg(target_os = "haiku")]
    Areas(Vec<libc::area_id>),
}

/// How QEMU's q35 machine splits `ram` bytes around the 32-bit PCI hole:
/// RAM below 4 GiB is file offset == GPA, the rest is at 4 GiB and up.
pub fn q35_lowmem(ram: u64) -> u64 {
    let lowmem = if ram >= 0xb000_0000 { 0x8000_0000 } else { 0xb000_0000 };
    ram.min(lowmem)
}

impl GuestMem {
    /// Maps QEMU's RAM file (memory-backend-file,share=on) of `size` bytes.
    pub fn open_file(path: &Path, size: u64, lowmem: u64) -> io::Result<GuestMem> {
        use std::os::fd::AsRawFd;
        let f = std::fs::OpenOptions::new().read(true).open(path)?;
        let base = unsafe { libc::mmap(std::ptr::null_mut(), size as usize, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0) };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let b = base as *const u8;
        let mut segs = vec![Segment { gpa: 0, len: lowmem.min(size), ptr: b }];
        if size > lowmem {
            segs.push(Segment { gpa: 1 << 32, len: size - lowmem, ptr: unsafe { b.add(lowmem as usize) } });
        }
        Ok(GuestMem { segs, cleanup: Cleanup::Munmap(base, size as usize) })
    }

    /// Clones the guest RAM areas listed in QEMU's map log (air/OS).
    #[cfg(target_os = "haiku")]
    pub fn from_nvmm_map(map_file: &Path) -> io::Result<GuestMem> {
        use std::collections::BTreeMap;
        let text = std::fs::read_to_string(map_file)?;
        // replay map/unmap: gpa -> (size, area, offset)
        let mut live: BTreeMap<u64, (u64, i32, u64)> = BTreeMap::new();
        let hex = |s: Option<&str>| s.and_then(|v| u64::from_str_radix(v, 16).ok());
        for line in text.lines() {
            let mut w = line.split_whitespace();
            match w.next() {
                Some("map") => {
                    if let (Some(gpa), Some(size), Some(area), Some(off)) = (hex(w.next()), hex(w.next()), hex(w.next()), hex(w.next())) {
                        live.insert(gpa, (size, area as i32, off));
                    }
                }
                Some("unmap") => {
                    if let Some(gpa) = hex(w.next()) {
                        live.remove(&gpa);
                    }
                }
                _ => {}
            }
        }
        if live.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "QEMU logged no guest RAM"));
        }
        let mut clones: BTreeMap<i32, *const u8> = BTreeMap::new();
        let mut areas = Vec::new();
        let mut segs = Vec::new();
        for (gpa, (size, area, off)) in live {
            let base = match clones.get(&area) {
                Some(p) => *p,
                None => {
                    let mut addr: *mut libc::c_void = std::ptr::null_mut();
                    let name = c"airDroid guest RAM";
                    let id = unsafe { libc::clone_area(name.as_ptr(), &mut addr, libc::B_ANY_ADDRESS, libc::B_READ_AREA, area) };
                    if id < 0 {
                        for a in &areas {
                            unsafe { libc::delete_area(*a) };
                        }
                        return Err(io::Error::other(format!("clone_area({area}) failed: {id:#x}")));
                    }
                    areas.push(id);
                    clones.insert(area, addr as *const u8);
                    addr as *const u8
                }
            };
            segs.push(Segment { gpa, len: size, ptr: unsafe { base.add(off as usize) } });
        }
        Ok(GuestMem { segs, cleanup: Cleanup::Areas(areas) })
    }

    /// Host bytes of a guest-physical range, if it is all in one RAM segment.
    pub fn slice(&self, r: &GpaRange) -> Option<&[u8]> {
        let end = r.gpa.checked_add(r.len as u64)?;
        let i = match self.segs.binary_search_by(|s| s.gpa.cmp(&r.gpa)) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        let s = &self.segs[i];
        if r.gpa < s.gpa || end > s.gpa + s.len {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts(s.ptr.add((r.gpa - s.gpa) as usize), r.len as usize) })
    }
}

impl Drop for GuestMem {
    fn drop(&mut self) {
        match &self.cleanup {
            Cleanup::Munmap(p, n) => unsafe {
                libc::munmap(*p, *n);
            },
            #[cfg(target_os = "haiku")]
            Cleanup::Areas(a) => {
                for id in a {
                    unsafe { libc::delete_area(*id) };
                }
            }
        }
    }
}
