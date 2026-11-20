//! Running commands in the VM or inside the Android container
//! (`waydroid shell`'s lxc-attach, done with setns).

use crate::container;
use crate::hostlink::HostLink;
use crate::sys;
use airdroid_proto::ToHost;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

/// PID (in the VM's namespace) of Android's init; 0 while not running.
pub static ANDROID_INIT: AtomicI32 = AtomicI32::new(0);

pub fn android_running() -> bool {
    let pid = ANDROID_INIT.load(Ordering::SeqCst);
    pid > 0 && std::path::Path::new(&format!("/proc/{pid}/ns/mnt")).exists()
}

/// Builds the command; for `android` the caller's thread joins the
/// container's PID namespace first (setns(CLONE_NEWPID) only affects
/// children of the calling thread, and a thread that did it cannot leave).
fn command(argv: &[String], android: bool) -> io::Result<Command> {
    if argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if android {
        let pid = ANDROID_INIT.load(Ordering::SeqCst);
        if pid <= 0 {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "Android is not running"));
        }
        let path = sys::cstr(format!("/proc/{pid}/ns/pid"));
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let r = unsafe { libc::setns(fd, libc::CLONE_NEWPID) };
        unsafe { libc::close(fd) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        cmd.env_clear();
        for (k, v) in container::android_env() {
            cmd.env(k, v);
        }
        unsafe {
            cmd.pre_exec(move || container::enter_namespaces_pre_exec(pid));
        }
    }
    Ok(cmd)
}

/// Spawns the command with piped stdio. For Android commands a throwaway
/// thread joins the container's PID namespace and only spawns: a thread
/// that did setns(CLONE_NEWPID) cannot create threads any more.
fn spawn_piped(argv: &[String], android: bool) -> io::Result<Child> {
    let argv = argv.to_vec();
    std::thread::spawn(move || -> io::Result<Child> {
        let mut cmd = command(&argv, android)?;
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.spawn()
    })
    .join()
    .map_err(|_| io::Error::other("spawn thread panicked"))?
}

/// Runs a command to completion; returns (exit code, stdout, stderr).
pub fn run(argv: &[&str], android: bool, stdin: &[u8]) -> io::Result<(i32, Vec<u8>, Vec<u8>)> {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let stdin = stdin.to_vec();
    (move || -> io::Result<(i32, Vec<u8>, Vec<u8>)> {
        let mut child = spawn_piped(&argv, android)?;
        let pid = child.id() as i32;
        let mut sin = child.stdin.take().unwrap();
        let w = std::thread::spawn(move || {
            let _ = sin.write_all(&stdin);
        });
        let mut err = child.stderr.take().unwrap();
        let e = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = err.read_to_end(&mut v);
            v
        });
        let mut out = Vec::new();
        child.stdout.take().unwrap().read_to_end(&mut out)?;
        let _ = w.join();
        let errv = e.join().unwrap_or_default();
        let code = sys::wait_pid(pid);
        Ok((code, out, errv))
    })()
}

/// Convenience: stdout of an Android command as a trimmed string.
pub fn android_output(argv: &[&str]) -> io::Result<String> {
    let (code, out, err) = run(argv, true, &[])?;
    if code != 0 {
        return Err(io::Error::other(format!(
            "{} exited {code}: {}",
            argv.join(" "),
            String::from_utf8_lossy(&err).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

pub fn getprop(key: &str) -> String {
    android_output(&["/system/bin/getprop", key]).unwrap_or_default()
}

pub fn setprop(key: &str, value: &str) -> io::Result<()> {
    android_output(&["/system/bin/setprop", key, value]).map(|_| ())
}

/// Host-requested command: streams output back as it comes.
pub fn spawn_for_host(link: HostLink, req: u32, argv: Vec<String>, android: bool, stdin: Vec<u8>) {
    std::thread::spawn(move || {
        let result = (|| -> io::Result<i32> {
            let mut child = spawn_piped(&argv, android)?;
            let pid = child.id() as i32;
            let mut sin = child.stdin.take().unwrap();
            std::thread::spawn(move || {
                let _ = sin.write_all(&stdin);
            });
            let pump = |mut r: Box<dyn Read + Send>, stderr: bool, link: HostLink| {
                std::thread::spawn(move || {
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        match r.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => link.send(&ToHost::ExecOutput { req, stderr, data: buf[..n].to_vec() }),
                        }
                    }
                })
            };
            let a = pump(Box::new(child.stdout.take().unwrap()), false, link.clone());
            let b = pump(Box::new(child.stderr.take().unwrap()), true, link.clone());
            let code = sys::wait_pid(pid);
            let _ = a.join();
            let _ = b.join();
            Ok(code)
        })();
        match result {
            Ok(code) => link.send(&ToHost::ExecDone { req, code }),
            Err(e) => {
                link.send(&ToHost::ExecOutput { req, stderr: true, data: format!("airdroid: {e}\n").into_bytes() });
                link.send(&ToHost::ExecDone { req, code: 127 });
            }
        }
    });
}
