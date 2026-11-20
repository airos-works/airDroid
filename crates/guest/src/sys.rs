//! Thin wrappers over the Linux calls the daemon needs as PID 1.

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

pub fn cstr<P: AsRef<Path>>(p: P) -> CString {
    CString::new(p.as_ref().as_os_str().as_bytes()).expect("path without NUL")
}

fn check(ret: libc::c_int, what: &str) -> io::Result<()> {
    if ret < 0 {
        let e = io::Error::last_os_error();
        Err(io::Error::new(e.kind(), format!("{what}: {e}")))
    } else {
        Ok(())
    }
}

pub fn mount(src: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: Option<&str>) -> io::Result<()> {
    let s = cstr(src);
    let t = cstr(target);
    let f = cstr(fstype);
    let d = data.map(cstr);
    let ret = unsafe {
        libc::mount(
            s.as_ptr(),
            t.as_ptr(),
            if fstype.is_empty() { std::ptr::null() } else { f.as_ptr() },
            flags,
            d.as_ref().map_or(std::ptr::null(), |d| d.as_ptr() as *const libc::c_void),
        )
    };
    check(ret, &format!("mount {src} on {target} ({fstype})"))
}

pub fn bind(src: &str, target: &str, readonly: bool) -> io::Result<()> {
    mount(src, target, "", libc::MS_BIND | libc::MS_REC, None)?;
    if readonly {
        mount("", target, "", libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY, None)?;
    }
    Ok(())
}

pub fn umount_lazy(target: &str) -> io::Result<()> {
    let t = cstr(target);
    check(unsafe { libc::umount2(t.as_ptr(), libc::MNT_DETACH) }, &format!("umount {target}"))
}

pub fn mkdir_p(path: &str, mode: u32) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    chmod(path, mode)
}

pub fn chmod(path: &str, mode: u32) -> io::Result<()> {
    let p = cstr(path);
    check(unsafe { libc::chmod(p.as_ptr(), mode) }, &format!("chmod {path}"))
}

pub fn chown(path: &str, uid: u32, gid: u32) -> io::Result<()> {
    let p = cstr(path);
    check(unsafe { libc::chown(p.as_ptr(), uid, gid) }, &format!("chown {path}"))
}

/// Character device node (replaces an existing file).
pub fn mknod_char(path: &str, major: u32, minor: u32, mode: u32) -> io::Result<()> {
    let _ = std::fs::remove_file(path);
    let p = cstr(path);
    let dev = libc::makedev(major, minor);
    check(unsafe { libc::mknod(p.as_ptr(), libc::S_IFCHR | mode, dev) }, &format!("mknod {path}"))?;
    chmod(path, mode)
}

/// An empty regular file to bind a node or a file onto.
pub fn touch(path: &str) -> io::Result<()> {
    if !Path::new(path).exists() {
        std::fs::write(path, b"")?;
    }
    Ok(())
}

pub fn write_file(path: &str, data: &str) -> io::Result<()> {
    std::fs::write(path, data).map_err(|e| io::Error::new(e.kind(), format!("write {path}: {e}")))
}

pub fn symlink(target: &str, link: &str) -> io::Result<()> {
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link)
}

pub fn sethostname(name: &str) {
    unsafe {
        libc::sethostname(name.as_ptr() as *const libc::c_char, name.len());
    }
}

/// Bring the loopback interface up (no `ip` needed).
pub fn loopback_up() -> io::Result<()> {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        for (i, b) in b"lo".iter().enumerate() {
            ifr.ifr_name[i] = *b as libc::c_char;
        }
        let r = libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut ifr);
        if r == 0 {
            ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &ifr);
        }
        libc::close(fd);
    }
    Ok(())
}

pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

// ---- child reaping ---------------------------------------------------------
//
// As PID 1 the daemon inherits every orphan in the VM, so one thread reaps
// all children and records exit codes; code that started a process waits
// for its pid here instead of calling waitpid itself.

struct Reaper {
    exits: Mutex<HashMap<i32, i32>>,
    cond: Condvar,
}

fn reaper() -> &'static Reaper {
    static R: OnceLock<Reaper> = OnceLock::new();
    R.get_or_init(|| Reaper { exits: Mutex::new(HashMap::new()), cond: Condvar::new() })
}

pub fn start_reaper() {
    let _ = reaper();
    std::thread::Builder::new()
        .name("reaper".into())
        .spawn(|| loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
            if pid < 0 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                continue;
            }
            let code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else if libc::WIFSIGNALED(status) {
                128 + libc::WTERMSIG(status)
            } else {
                continue;
            };
            let r = reaper();
            let mut exits = r.exits.lock().unwrap();
            if exits.len() > 4096 {
                exits.clear(); // orphans nobody waits for
            }
            exits.insert(pid, code);
            r.cond.notify_all();
        })
        .expect("reaper thread");
}

/// Wait for a child started by this daemon; returns its exit code
/// (128+signal when killed).
pub fn wait_pid(pid: i32) -> i32 {
    let r = reaper();
    let mut exits = r.exits.lock().unwrap();
    loop {
        if let Some(code) = exits.remove(&pid) {
            return code;
        }
        exits = r.cond.wait(exits).unwrap();
    }
}

/// Non-blocking: Some(exit code) once the child has exited.
pub fn try_wait_pid(pid: i32) -> Option<i32> {
    reaper().exits.lock().unwrap().remove(&pid)
}
