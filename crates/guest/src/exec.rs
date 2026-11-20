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
    command_as(argv, android, None)
}

/// Android's shell user (AID_SHELL): what `adb shell` runs as. The activity
/// manager's shell commands refuse root callers without a word.
pub const AID_SHELL: u32 = 2000;

fn command_as(argv: &[String], android: bool, uid: Option<u32>) -> io::Result<Command> {
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
            cmd.pre_exec(move || {
                container::enter_namespaces_pre_exec(pid)?;
                if let Some(uid) = uid {
                    // AID_SHELL's groups as adb shell has them: log, inet, sdcard_rw, ...
                    let groups: [libc::gid_t; 6] = [1004, 1007, 1011, 1015, 3003, 3009];
                    if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                        || libc::setgid(uid) != 0
                        || libc::setuid(uid) != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
    Ok(cmd)
}

/// Spawns the command with piped stdio. For Android commands a throwaway
/// thread joins the container's PID namespace and only spawns: a thread
/// that did setns(CLONE_NEWPID) cannot create threads any more.
fn spawn_piped(argv: &[String], android: bool, uid: Option<u32>) -> io::Result<Child> {
    let argv = argv.to_vec();
    std::thread::spawn(move || -> io::Result<Child> {
        let mut cmd = command_as(&argv, android, uid)?;
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        sys::spawn(&mut cmd)
    })
    .join()
    .map_err(|_| io::Error::other("spawn thread panicked"))?
}

/// Runs a command to completion; returns (exit code, stdout, stderr).
pub fn run(argv: &[&str], android: bool, stdin: &[u8]) -> io::Result<(i32, Vec<u8>, Vec<u8>)> {
    run_as(argv, android, None, stdin)
}

/// Like `run`, as user `uid` (Android commands only).
pub fn run_as(argv: &[&str], android: bool, uid: Option<u32>, stdin: &[u8]) -> io::Result<(i32, Vec<u8>, Vec<u8>)> {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let stdin = stdin.to_vec();
    (move || -> io::Result<(i32, Vec<u8>, Vec<u8>)> {
        let mut child = spawn_piped(&argv, android, uid)?;
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
        let code = sys::wait_child(&mut child);
        Ok((code, out, errv))
    })()
}

/// Convenience: stdout of an Android command as a trimmed string.
pub fn android_output(argv: &[&str]) -> io::Result<String> {
    android_output_as(argv, None)
}

pub fn android_output_as(argv: &[&str], uid: Option<u32>) -> io::Result<String> {
    let (code, out, err) = run_as(argv, true, uid, &[])?;
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
            let mut child = spawn_piped(&argv, android, None)?;
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
            let code = sys::wait_child(&mut child);
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
