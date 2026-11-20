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
}

impl Config {
    pub fn from_cmdline(cmdline: &str) -> Config {
        let mut c = Config { width: 1280, height: 800, dpi: 0, multi_windows: true, gpu: "none".into() };
        for word in cmdline.split_whitespace() {
            let Some(kv) = word.strip_prefix("airdroid.") else { continue };
            let Some((k, v)) = kv.split_once('=') else { continue };
            match k {
                "width" => c.width = v.parse().unwrap_or(c.width),
                "height" => c.height = v.parse().unwrap_or(c.height),
                "dpi" => c.dpi = v.parse().unwrap_or(0),
                "multiwin" => c.multi_windows = v == "1",
                "gpu" => c.gpu = v.to_string(),
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

/// Mounts the data disk, formatting it on first start.
pub fn mount_data_disk(on_format: impl FnOnce()) -> io::Result<()> {
    let dev = disk_by_serial("userdata")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no userdata disk"))?;
    if !has_ext4(&dev) {
        on_format();
        logf!("formatting {dev} (first start)");
        let child = std::process::Command::new("/sbin/mke2fs")
            .args(["-q", "-F", "-t", "ext4", "-L", "airdroid-data", "-E", "lazy_itable_init=1,lazy_journal_init=1", &dev])
            .spawn()?;
        // the PID 1 reaper collects every child: wait through it
        let code = sys::wait_pid(child.id() as i32);
        if code != 0 {
            return Err(io::Error::other(format!("mke2fs {dev} exited {code}")));
        }
    }
    sys::mkdir_p(DATA_DISK, 0o755)?;
    sys::mount(&dev, DATA_DISK, "ext4", libc::MS_NOATIME, Some("discard"))?;
    for d in ["data", "overlay_rw/system", "overlay_rw/vendor", "overlay_work/system", "overlay_work/vendor"] {
        sys::mkdir_p(&format!("{DATA_DISK}/{d}"), 0o755)?;
    }
    Ok(())
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
        "virgl" => {
            set("ro.hardware.gralloc", "gbm");
            set("ro.hardware.egl", "mesa");
            set("gralloc.gbm.device", "/dev/dri/renderD128");
            set("ro.hardware.vulkan", "virtio");
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
