//! The Android container: what Waydroid asks LXC for, done directly.
//!
//! Layout inside the VM (root is the initramfs):
//!
//! ```text
//!   /mnt/system, /mnt/vendor   the read-only images (virtio disks)
//!   /var/lib/airdroid          the data disk: data/, overlay_rw/, overlay_work/
//!   /android                   overlay(system) = the container's root
//!   /android/vendor            overlay(vendor)
//!   /android/data              bind of /var/lib/airdroid/data
//!   /android/dev               tmpfs with the nodes Waydroid binds in
//!   /android/run/xdg           the Wayland (and pulse) sockets
//! ```
//!
//! The container gets its own mount, PID, UTS and IPC namespaces; it shares
//! the VM's network namespace, so Android's netd owns eth0 (the VM exists
//! only for Android).

use crate::sys;
use crate::logf;
use std::io;
use std::path::Path;

pub const ROOT: &str = "/android";
pub const DATA_DISK: &str = "/var/lib/airdroid";
pub const XDG_RUNTIME: &str = "/android/run/xdg";
/// Same as Waydroid's container_xdg_runtime_dir.
pub const CONTAINER_XDG_RUNTIME: &str = "/run/xdg";
pub const WAYLAND_DISPLAY: &str = "wayland-0";

/// Binder devices created in binderfs and bound into the container.
pub const BINDER_NODES: [(&str, &str); 3] = [
    ("anbox-binder", "binder"),
    ("anbox-vndbinder", "vndbinder"),
    ("anbox-hwbinder", "hwbinder"),
];

/// Settings the host passes on the kernel command line (airdroid.KEY=VALUE).
#[derive(Debug, Clone)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub multi_windows: bool,
    /// "none" (SwiftShader) or "virgl".
    pub gpu: String,
    /// The host's time zone (Olson ID), or empty.
    pub tz: String,
}

impl Config {
    pub fn from_cmdline(cmdline: &str) -> Config {
        let mut c = Config { width: 1280, height: 800, dpi: 0, multi_windows: true, gpu: "none".into(), tz: String::new() };
        for word in cmdline.split_whitespace() {
            let Some(kv) = word.strip_prefix("airdroid.") else { continue };
            let Some((k, v)) = kv.split_once('=') else { continue };
            match k {
                "width" => c.width = v.parse().unwrap_or(c.width),
                "height" => c.height = v.parse().unwrap_or(c.height),
                "dpi" => c.dpi = v.parse().unwrap_or(0),
                "multiwin" => c.multi_windows = v == "1",
                "gpu" => c.gpu = v.to_string(),
                "tz" => c.tz = v.to_string(),
                _ => {}
            }
        }
        c
    }
}

/// /dev/vdX whose virtio serial (QEMU `serial=`) is `name`.
pub fn disk_by_serial(name: &str) -> Option<String> {
    for entry in std::fs::read_dir("/sys/block").ok()?.flatten() {
        let serial = std::fs::read_to_string(entry.path().join("serial")).unwrap_or_default();
        if serial.trim() == name {
            return Some(format!("/dev/{}", entry.file_name().to_string_lossy()));
        }
    }
    None
}

fn has_ext4(dev: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(dev) else { return false };
    let mut magic = [0u8; 2];
    f.seek(SeekFrom::Start(1024 + 0x38)).is_ok() && f.read_exact(&mut magic).is_ok() && magic == [0x53, 0xEF]
}

/// e2fsck's exit code: a set of flags.
const FSCK_UNCORRECTED: i32 = 4;

fn e2fsck(dev: &str, args: &[&str]) -> i32 {
    match sys::spawn(std::process::Command::new("/sbin/e2fsck").args(args).arg(dev)) {
        Ok(mut child) => sys::wait_child(&mut child),
        Err(e) => {
            logf!("e2fsck {dev}: {e}");
            0
        }
    }
}

/// Whether the file system was in use when the VM last stopped: its
/// journal still has to be replayed (EXT4_FEATURE_INCOMPAT_RECOVER).
fn was_in_use(dev: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(dev) else { return false };
    let mut incompat = [0u8; 4];
    f.seek(SeekFrom::Start(1024 + 0x60)).is_ok()
        && f.read_exact(&mut incompat).is_ok()
        && u32::from_le_bytes(incompat) & 0x4 != 0
}

/// Checks the data disk before it is mounted, as Android does with its
/// own userdata. Shut down properly it is clean and this costs a few
/// milliseconds. If the VM went away with Android running (the host
/// crashed or lost power, QEMU was killed) the journal is replayed and
/// everything is checked once: writes Android thought were on the disk
/// may not be, which the journal cannot know and a file system marked
/// clean does not show (about a second for a small disk). "-p" repairs
/// what is safe to repair without asking, and what that leaves is
/// answered with yes rather than left to keep Android from starting.
fn check_data_disk(dev: &str) {
    let started = std::time::Instant::now();
    let unclean = was_in_use(dev);
    let mut code = e2fsck(dev, if unclean { &["-f", "-p"] } else { &["-p"] });
    if code >= FSCK_UNCORRECTED {
        logf!("data disk: errors a plain check does not repair (e2fsck exit {code}), repairing");
        code = e2fsck(dev, &["-f", "-y"]);
    }
    let ms = started.elapsed().as_millis();
    let how = if unclean { "checked in full after an unclean stop" } else { "clean" };
    match code {
        0 => logf!("data disk: {how} ({ms} ms)"),
        1..=3 => logf!("data disk: errors repaired (e2fsck exit {code}, {ms} ms)"),
        _ => logf!("data disk: e2fsck could not repair it (exit {code}, {ms} ms)"),
    }
}

/// Mounts the data disk, formatting it on first start.
pub fn mount_data_disk(on_format: impl FnOnce()) -> io::Result<()> {
    let dev = disk_by_serial("userdata")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no userdata disk"))?;
    if has_ext4(&dev) {
        check_data_disk(&dev);
    } else {
        on_format();
        logf!("formatting {dev} (first start)");
        let mut child = sys::spawn(std::process::Command::new("/sbin/mke2fs").args(["-q", "-F", "-t", "ext4", "-L", "airdroid-data", &dev]))?;
        let code = sys::wait_child(&mut child);
        if code != 0 {
            return Err(io::Error::other(format!("mke2fs {dev} exited {code}")));
        }
    }
    sys::mkdir_p(DATA_DISK, 0o755)?;
    sys::mount(&dev, DATA_DISK, "ext4", libc::MS_NOATIME, None)?;
    for d in ["data", "overlay_rw/system", "overlay_rw/vendor", "overlay_work/system", "overlay_work/vendor"] {
        sys::mkdir_p(&format!("{DATA_DISK}/{d}"), 0o755)?;
    }
    let started = std::time::Instant::now();
    let (seen, repaired) = clear_zeroed_state(std::path::Path::new(&format!("{DATA_DISK}/data")));
    if repaired > 0 {
        logf!("data disk: {repaired} of {seen} state files held only zeros ({} ms)", started.elapsed().as_millis());
    }
    Ok(())
}

/// Whether a file has something in it and all of it is zero bytes.
fn all_zeros(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut buf = [0u8; 65536];
    let mut any = false;
    loop {
        match f.read(&mut buf) {
            Ok(0) => return any,
            Ok(n) => {
                if buf[..n].iter().any(|&b| b != 0) {
                    return false;
                }
                any = true;
            }
            Err(_) => return false,
        }
    }
}

fn xml_files(dir: &std::path::Path, depth: u32, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if depth > 0 {
                xml_files(&path, depth - 1, out);
            }
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "xml") {
            out.push(path);
        }
    }
}

/// Android's state files (XML, text or binary) that hold nothing but zero
/// bytes: what a crash of the host can leave of a file written just
/// before it (its size made it to the disk, its contents did not). Android
/// treats most of them as damaged and starts over with defaults, but not
/// all: with a zeroed users/0.xml there is no user, no app is installed for
/// anyone, and the system server dies at every start ("There must be
/// exactly one installer").
///
/// No valid file is all zeros, so nothing is lost by taking them away:
/// Android's own copy is put in their place where it keeps one
/// (settings_*.xml.fallback), the others it writes anew. A zeroed user
/// record takes the list of users with it, from which Android would
/// otherwise still expect that user; it then sets up the first user again,
/// whose apps and data stay.
///
/// Returns how many files were looked at and how many repaired.
pub fn clear_zeroed_state(data: &std::path::Path) -> (usize, usize) {
    let mut files = Vec::new();
    for (dir, depth) in [("system", 2), ("system_de", 2), ("system_ce", 2), ("misc_de", 4)] {
        xml_files(&data.join(dir), depth, &mut files);
    }
    let users = data.join("system/users");
    let mut repaired = 0;
    for path in &files {
        // 4 MiB: none of these files is anywhere near that
        if !std::fs::metadata(path).is_ok_and(|m| m.len() > 0 && m.len() <= 4 << 20) || !all_zeros(path) {
            continue;
        }
        let name = path.strip_prefix(data).unwrap_or(path).display().to_string();
        let fallback = std::path::PathBuf::from(format!("{}.fallback", path.display()));
        if fallback.exists() && !all_zeros(&fallback) && std::fs::metadata(&fallback).is_ok_and(|m| m.len() > 0) {
            // copy, keeping the owner and mode of the file Android made
            match std::fs::read(&fallback).and_then(|bytes| std::fs::write(path, bytes)) {
                Ok(()) => logf!("data disk: {name} held only zeros, put back from its fallback copy"),
                Err(e) => logf!("data disk: {name} held only zeros, its fallback copy: {e}"),
            }
        } else {
            match std::fs::remove_file(path) {
                Ok(()) => logf!("data disk: {name} held only zeros, removed"),
                Err(e) => logf!("data disk: {name} held only zeros: {e}"),
            }
            if path.parent() == Some(users.as_path()) && path.file_name().is_some_and(|n| n != "userlist.xml") {
                let _ = std::fs::remove_file(users.join("userlist.xml"));
                logf!("data disk: the list of users goes with it, Android sets the first user up again");
            }
        }
        repaired += 1;
    }
    (files.len(), repaired)
}

fn mount_image(serial: &str, at: &str) -> io::Result<()> {
    let dev = disk_by_serial(serial)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no {serial} disk")))?;
    sys::mkdir_p(at, 0o755)?;
    sys::mount(&dev, at, "ext4", libc::MS_RDONLY, Some("noload"))
}

/// Overlay with a writable upper layer on the data disk, like Waydroid's
/// mount_overlays (lets airDroid and the user add files to system/vendor).
/// `extra` is a read-only layer shipped in the initramfs, above the image.
fn mount_overlay(lower: &str, extra: &str, name: &str, at: &str) -> io::Result<()> {
    let mut lowers = String::new();
    if Path::new(extra).is_dir() {
        lowers.push_str(extra);
        lowers.push(':');
    }
    lowers.push_str(lower);
    let opts = format!(
        "lowerdir={lowers},upperdir={DATA_DISK}/overlay_rw/{name},workdir={DATA_DISK}/overlay_work/{name},xino=off,index=off,redirect_dir=off,metacopy=off"
    );
    sys::mount("overlay", at, "overlay", 0, Some(&opts))
}

/// Builds /android (needs the data disk mounted).
pub fn prepare_rootfs(cfg: &Config, props: &str) -> io::Result<()> {
    mount_image("system", "/mnt/system")?;
    mount_image("vendor", "/mnt/vendor")?;
    sys::mkdir_p(ROOT, 0o755)?;
    if let Err(e) = mount_overlay("/mnt/system", "/usr/share/airdroid/overlay/system", "system", ROOT) {
        logf!("system overlay failed ({e}); using the image read-only");
        sys::bind("/mnt/system", ROOT, true)?;
    }
    if let Err(e) = mount_overlay("/mnt/vendor", "/usr/share/airdroid/overlay/vendor", "vendor", &format!("{ROOT}/vendor")) {
        logf!("vendor overlay failed ({e}); using the image read-only");
        sys::bind("/mnt/vendor", &format!("{ROOT}/vendor"), true)?;
    }

    // waydroid.prop: Waydroid binds its generated file over the vendor one
    sys::mkdir_p("/run/airdroid", 0o755)?;
    sys::write_file("/run/airdroid/waydroid.prop", props)?;
    sys::chmod("/run/airdroid/waydroid.prop", 0o644)?;
    sys::bind("/run/airdroid/waydroid.prop", &format!("{ROOT}/vendor/waydroid.prop"), true)?;

    // tmpfs where LXC puts them (config_nodes)
    for d in ["dev", "tmp", "var", "run", "mnt_extra"] {
        let p = format!("{ROOT}/{d}");
        let opts = if d == "dev" { "mode=755" } else { "mode=755" };
        let flags = if d == "dev" { libc::MS_NOSUID } else { libc::MS_NODEV };
        sys::mount("tmpfs", &p, "tmpfs", flags, Some(opts))?;
    }
    populate_dev(cfg)?;

    sys::bind(&format!("{DATA_DISK}/data"), &format!("{ROOT}/data"), false)?;

    // XDG runtime dir for the HWC's Wayland socket and the audio HAL's pulse
    sys::mkdir_p(XDG_RUNTIME, 0o777)?;
    sys::mkdir_p(&format!("{XDG_RUNTIME}/pulse"), 0o777)?;
    Ok(())
}

fn populate_dev(cfg: &Config) -> io::Result<()> {
    let dev = format!("{ROOT}/dev");
    for (name, major, minor, mode) in [
        ("null", 1, 3, 0o666),
        ("zero", 1, 5, 0o666),
        ("full", 1, 7, 0o666),
        ("tty", 5, 0, 0o666),
        ("fuse", 10, 229, 0o666),
        ("uhid", 10, 239, 0o660),
        ("tun", 10, 200, 0o660),
    ] {
        sys::mknod_char(&format!("{dev}/{name}"), major, minor, mode)?;
    }
    for (host, inside) in BINDER_NODES {
        let target = format!("{dev}/{inside}");
        sys::touch(&target)?;
        sys::bind(&format!("/dev/binderfs/{host}"), &target, false)?;
    }
    sys::mkdir_p(&format!("{dev}/pts"), 0o755)?;
    sys::mount("devpts", &format!("{dev}/pts"), "devpts", libc::MS_NOSUID | libc::MS_NOEXEC, Some("newinstance,ptmxmode=0666,mode=0620,gid=5"))?;
    sys::symlink("pts/ptmx", &format!("{dev}/ptmx"))?;
    if cfg.gpu != "none" {
        if let Ok(entries) = std::fs::read_dir("/dev/dri") {
            // Android's graphics stack runs as several users
            for e in std::fs::read_dir("/dev/dri").into_iter().flatten().flatten() {
                let _ = sys::chmod(&e.path().to_string_lossy(), 0o666);
            }
            sys::mkdir_p(&format!("{dev}/dri"), 0o755)?;
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let target = format!("{dev}/dri/{name}");
                sys::touch(&target)?;
                sys::bind(&format!("/dev/dri/{name}"), &target, false)?;
            }
        }
    }
    Ok(())
}

/// binderfs with the three devices Android needs.
pub fn setup_binder() -> io::Result<()> {
    sys::mkdir_p("/dev/binderfs", 0o755)?;
    sys::mount("binder", "/dev/binderfs", "binder", 0, Some("max=1048576"))?;
    let ctl = std::fs::OpenOptions::new().read(true).write(true).open("/dev/binderfs/binder-control")?;
    for (name, _) in BINDER_NODES {
        // struct binderfs_device { char name[256]; __u32 major; __u32 minor; }
        let mut dev = [0u8; 264];
        dev[..name.len()].copy_from_slice(name.as_bytes());
        // BINDER_CTL_ADD = _IOWR('b', 1, struct binderfs_device)
        let req: libc::c_ulong = (3 << 30) | (264 << 16) | ((b'b' as libc::c_ulong) << 8) | 1;
        let r = unsafe { libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(&ctl), req as _, dev.as_mut_ptr()) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EEXIST) {
                return Err(io::Error::new(e.kind(), format!("binderfs add {name}: {e}")));
            }
        }
        sys::chmod(&format!("/dev/binderfs/{name}"), 0o666)?;
    }
    Ok(())
}

/// The props Waydroid writes to waydroid.prop (make_base_props + make_prop),
/// for a host without Android HALs.
pub fn make_props(cfg: &Config, extra: &[(String, String)]) -> String {
    let mut props: Vec<(String, String)> = Vec::new();
    let mut set = |k: &str, v: &str| props.push((k.to_string(), v.to_string()));
    set("sys.use_memfd", "true");
    set("ro.adb.secure", "1");
    set("ro.debuggable", "0");
    match cfg.gpu.as_str() {
        "virgl" | "venus" => {
            // Mesa's virgl driver on virtio-gpu (what Waydroid sets for a
            // DRI node); Vulkan through Venus (Mesa's virtio driver) when the
            // host has it, else lavapipe
            set("ro.hardware.gralloc", "gbm");
            set("ro.hardware.egl", "mesa");
            set("gralloc.gbm.device", "/dev/dri/renderD128");
            set("ro.hardware.vulkan", if cfg.gpu == "venus" { "virtio" } else { "lvp" });
        }
        _ => {
            set("ro.hardware.gralloc", "default");
            set("ro.hardware.egl", "swiftshader");
        }
    }
    set("debug.stagefright.ccodec", "0");
    set("ro.opengles.version", "196610");
    set("waydroid.updater.disabled", "true");
    set("waydroid.tools_version", concat!("airdroid-", env!("CARGO_PKG_VERSION")));
    set("ro.vndk.lite", "true");
    set("waydroid.host.user", "airos");
    set("waydroid.host.uid", "0");
    set("waydroid.host.gid", "0");
    set("waydroid.host_data_path", "/var/lib/airdroid/data");
    set("waydroid.background_start", "true");
    set("waydroid.xdg_runtime_dir", CONTAINER_XDG_RUNTIME);
    set("waydroid.pulse_runtime_path", &format!("{CONTAINER_XDG_RUNTIME}/pulse"));
    set("waydroid.wayland_display", WAYLAND_DISPLAY);
    set("waydroid.stub_sensors_hal", "1");
    set("ro.product.waydroid.brand", "airOS");
    set("ro.product.waydroid.manufacturer", "airOS");
    set("ro.product.waydroid.model", "airDroid");
    if cfg.dpi != 0 {
        set("ro.sf.lcd_density", &cfg.dpi.to_string());
    }
    if !cfg.tz.is_empty() {
        // a new data disk starts in the host's zone (afterwards Android's
        // saved one wins: apps::follow_timezone)
        set("persist.sys.timezone", &cfg.tz);
    }
    for (k, v) in extra {
        props.retain(|(pk, _)| pk != k);
        props.push((k.clone(), v.clone()));
    }
    let mut out = String::new();
    for (k, v) in props {
        out.push_str(&k);
        out.push('=');
        out.push_str(&v);
        out.push('\n');
    }
    out
}

/// Android's environment for commands run inside the container
/// (Waydroid's ANDROID_ENV plus the classpath variables init exports).
pub fn android_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = [
        ("PATH", "/product/bin:/apex/com.android.runtime/bin:/apex/com.android.art/bin:/system_ext/bin:/system/bin:/system/xbin:/odm/bin:/vendor/bin:/vendor/xbin"),
        ("ANDROID_ROOT", "/system"),
        ("ANDROID_DATA", "/data"),
        ("ANDROID_STORAGE", "/storage"),
        ("ANDROID_ART_ROOT", "/apex/com.android.art"),
        ("ANDROID_I18N_ROOT", "/apex/com.android.i18n"),
        ("ANDROID_TZDATA_ROOT", "/apex/com.android.tzdata"),
        ("ANDROID_RUNTIME_ROOT", "/apex/com.android.runtime"),
        ("HOME", "/"),
        ("TERM", "xterm"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    // /data/system/environ/classpath: "export NAME VALUE" lines
    if let Ok(text) = std::fs::read_to_string(format!("{ROOT}/data/system/environ/classpath")) {
        for line in text.lines() {
            let mut parts = line.splitn(3, ' ');
            if let (Some(_), Some(k), Some(v)) = (parts.next(), parts.next(), parts.next()) {
                if k.contains("CLASSPATH") || k.contains("SYSTEMSERVER") {
                    env.push((k.to_string(), v.to_string()));
                }
            }
        }
    }
    env
}

/// Starts Android's init as PID 1 of new namespaces; returns its pid in the
/// VM's namespace.
pub fn start() -> io::Result<i32> {
    let root = sys::cstr(ROOT);
    let init = sys::cstr("/init");
    let argv = [init.as_ptr(), std::ptr::null()];
    let env_strings: Vec<std::ffi::CString> = ["container=lxc", "PATH=/system/bin:/system/xbin:/vendor/bin"]
        .iter()
        .map(|s| std::ffi::CString::new(*s).unwrap())
        .collect();
    let mut envp: Vec<*const libc::c_char> = env_strings.iter().map(|s| s.as_ptr()).collect();
    envp.push(std::ptr::null());
    let proc_ = sys::cstr("proc");
    let sysfs = sys::cstr("sysfs");
    let slash = sys::cstr("/");
    let dot = sys::cstr(".");
    let proc_dir = sys::cstr("/proc");
    let sys_dir = sys::cstr("/sys");
    let hostname = b"localhost";

    let flags = libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC;
    // fork-like clone: the child gets a copy of the address space and only
    // makes raw system calls (everything it needs is prepared above).
    let pid = unsafe { libc::syscall(libc::SYS_clone, (flags | libc::SIGCHLD) as libc::c_ulong, 0usize, 0usize, 0usize, 0usize) };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            // nothing we do in here may propagate back to the VM's namespace
            libc::mount(std::ptr::null(), slash.as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null());
            if libc::chdir(root.as_ptr()) != 0 {
                libc::_exit(120);
            }
            // the initramfs root cannot be pivoted away from: move /android
            // over / in this namespace and chroot into it (switch_root)
            if libc::mount(dot.as_ptr(), slash.as_ptr(), std::ptr::null(), libc::MS_MOVE, std::ptr::null()) != 0 {
                libc::_exit(121);
            }
            if libc::chroot(dot.as_ptr()) != 0 {
                libc::_exit(122);
            }
            libc::chdir(slash.as_ptr());
            libc::mount(proc_.as_ptr(), proc_dir.as_ptr(), proc_.as_ptr(), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, std::ptr::null());
            libc::mount(sysfs.as_ptr(), sys_dir.as_ptr(), sysfs.as_ptr(), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, std::ptr::null());
            libc::sethostname(hostname.as_ptr() as *const libc::c_char, hostname.len());
            libc::setsid();
            // Android's init runs with the signal mask and dispositions reset
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            libc::execve(init.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
    }
    sys::track(pid as i32);
    Ok(pid as i32)
}

/// Joins the container's namespaces from a freshly forked child (pre_exec):
/// mount, UTS and IPC here; the PID namespace must be joined by the thread
/// that spawns (see exec.rs).
pub fn enter_namespaces_pre_exec(init_pid: i32) -> io::Result<()> {
    for ns in ["ipc", "uts", "mnt"] {
        let path = sys::cstr(format!("/proc/{init_pid}/ns/{ns}"));
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let r = unsafe { libc::setns(fd, 0) };
        unsafe { libc::close(fd) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let slash = sys::cstr("/");
    unsafe { libc::chdir(slash.as_ptr()) };
    Ok(())
}

/// Keeps the hwcomposer's input FIFOs (/dev/input/event*) open for writing.
///
/// The HWC mkfifo()s its input pipes up front but opens the write end only
/// on the first input event; Android's init opens every new /dev/input node
/// (keychords) read-only, which blocks on a FIFO without a writer, and with
/// init stuck no service starts and system_server's watchdog kills it.
/// Holding each FIFO open O_RDWR (never blocks on Linux) makes us a writer;
/// we never read, so events still reach InputFlinger.
pub fn hold_input_fifos() {
    std::thread::Builder::new()
        .name("input-fifos".into())
        .spawn(|| {
            use std::collections::HashMap;
            use std::os::unix::fs::FileTypeExt;
            let dir = format!("{ROOT}/dev/input");
            let mut held: HashMap<std::path::PathBuf, (u64, std::fs::File)> = HashMap::new();
            loop {
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    let mut seen = Vec::new();
                    for e in entries.flatten() {
                        let path = e.path();
                        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
                        if !meta.file_type().is_fifo() {
                            continue;
                        }
                        use std::os::unix::fs::MetadataExt;
                        let ino = meta.ino();
                        seen.push(path.clone());
                        if held.get(&path).map(|h| h.0) == Some(ino) {
                            continue;
                        }
                        use std::os::unix::fs::OpenOptionsExt;
                        if let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(&path) {
                            held.insert(path, (ino, f));
                        }
                    }
                    held.retain(|p, _| seen.contains(p));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        })
        .expect("input-fifos thread");
}

/// Sets persistent properties in Android's store before it boots
/// (/data/property/persistent_properties: a protobuf PersistentProperties,
/// field 1 repeated { 1: name, 2: value }). Android loads it before the
/// hwcomposer starts, so a value set with setprop after boot would only
/// count from the next boot on.
pub fn set_persistent_props(props: &[(&str, &str)]) -> io::Result<()> {
    let path = format!("{DATA_DISK}/data/property/persistent_properties");
    let old = std::fs::read(&path).unwrap_or_default();
    let mut records: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    let varint = |b: &[u8], i: &mut usize| -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *b.get(*i)?;
            *i += 1;
            v |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    };
    while i < old.len() {
        let Some(key) = varint(&old, &mut i) else { break };
        if key != (1 << 3 | 2) {
            break; // not a record: keep what we parsed
        }
        let Some(len) = varint(&old, &mut i) else { break };
        let end = i + len as usize;
        if end > old.len() {
            break;
        }
        let rec = &old[i..end];
        i = end;
        let (mut j, mut name, mut value) = (0usize, String::new(), String::new());
        while j < rec.len() {
            let Some(k) = varint(rec, &mut j) else { break };
            let Some(l) = varint(rec, &mut j) else { break };
            let s = String::from_utf8_lossy(&rec[j..(j + l as usize).min(rec.len())]).into_owned();
            j += l as usize;
            match k {
                0x0a => name = s,
                0x12 => value = s,
                _ => {}
            }
        }
        records.push((name, value));
    }
    for (k, v) in props {
        records.retain(|(n, _)| n != k);
        records.push((k.to_string(), v.to_string()));
    }
    let put_varint = |out: &mut Vec<u8>, mut v: u64| {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    };
    let mut out = Vec::new();
    for (n, v) in &records {
        let mut rec = Vec::new();
        rec.push(0x0a);
        put_varint(&mut rec, n.len() as u64);
        rec.extend_from_slice(n.as_bytes());
        rec.push(0x12);
        put_varint(&mut rec, v.len() as u64);
        rec.extend_from_slice(v.as_bytes());
        out.push(0x0a);
        put_varint(&mut out, rec.len() as u64);
        out.extend_from_slice(&rec);
    }
    let dir = format!("{DATA_DISK}/data/property");
    std::fs::create_dir_all(&dir)?;
    let tmp = format!("{path}.airdroid");
    std::fs::write(&tmp, &out)?;
    sys::chmod(&tmp, 0o600)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod zeroed_tests {
    use super::*;

    #[test]
    fn zeroed_state_files_are_cleared() {
        let dir = std::env::temp_dir().join(format!("airdroid-zeroed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let users = dir.join("system/users");
        std::fs::create_dir_all(users.join("0")).unwrap();
        std::fs::write(users.join("0.xml"), [0u8; 330]).unwrap();
        std::fs::write(users.join("userlist.xml"), b"ABX\0users").unwrap();
        std::fs::write(users.join("0/settings_global.xml"), [0u8; 9000]).unwrap();
        std::fs::write(users.join("0/settings_global.xml.fallback"), b"ABX\0settings").unwrap();
        std::fs::write(users.join("0/settings_secure.xml"), b"ABX\0fine").unwrap();
        std::fs::write(dir.join("system/netpolicy.xml"), [0u8; 149]).unwrap();
        std::fs::write(dir.join("system/empty.xml"), b"").unwrap();
        std::fs::write(dir.join("system/packages.xml"), b"ABX\0packages").unwrap();
        std::fs::write(dir.join("system/zeros.bin"), [0u8; 64]).unwrap();

        let (seen, repaired) = clear_zeroed_state(&dir);
        assert_eq!((seen, repaired), (7, 3));
        assert!(!users.join("0.xml").exists());
        assert!(!users.join("userlist.xml").exists());
        assert_eq!(std::fs::read(users.join("0/settings_global.xml")).unwrap(), b"ABX\0settings");
        assert_eq!(std::fs::read(users.join("0/settings_secure.xml")).unwrap(), b"ABX\0fine");
        assert!(!dir.join("system/netpolicy.xml").exists());
        assert!(dir.join("system/empty.xml").exists());
        assert!(dir.join("system/packages.xml").exists());
        assert!(dir.join("system/zeros.bin").exists());
        // nothing left to do the second time
        assert_eq!(clear_zeroed_state(&dir).1, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
