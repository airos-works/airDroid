//! The guest's RAM, mapped from the file QEMU backs it with.

use airdroid_proto::GpaRange;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

pub struct GuestMem {
    base: *mut u8,
    size: u64,
    /// Guest-physical address where RAM above 4 GiB starts in the file.
    lowmem: u64,
}
unsafe impl Send for GuestMem {}
unsafe impl Sync for GuestMem {}

/// How QEMU's q35 machine splits `ram` bytes around the 32-bit PCI hole:
/// RAM below 4 GiB is file offset == GPA, the rest is at 4 GiB and up.
pub fn q35_lowmem(ram: u64) -> u64 {
    let lowmem = if ram >= 0xb000_0000 { 0x8000_0000 } else { 0xb000_0000 };
    ram.min(lowmem)
}

impl GuestMem {
    pub fn open(path: &Path, size: u64, lowmem: u64) -> io::Result<GuestMem> {
        let f = OpenOptions::new().read(true).open(path)?;
        let base = unsafe { libc::mmap(std::ptr::null_mut(), size as usize, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0) };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(GuestMem { base: base as *mut u8, size, lowmem })
    }

    fn offset(&self, gpa: u64) -> Option<u64> {
        if gpa < self.lowmem {
            Some(gpa)
        } else if gpa >= 1 << 32 {
            Some(gpa - (1 << 32) + self.lowmem)
        } else {
            None
        }
    }

    /// Host pointer for a guest-physical range, if it is all RAM.
    pub fn slice(&self, r: &GpaRange) -> Option<&[u8]> {
        let off = self.offset(r.gpa)?;
        let end = off.checked_add(r.len as u64)?;
        // a range must not straddle the low/high boundary
        if end > self.size || (r.gpa < self.lowmem && r.gpa + r.len as u64 > self.lowmem) {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts(self.base.add(off as usize), r.len as usize) })
    }
}

impl Drop for GuestMem {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.size as usize) };
    }
}
