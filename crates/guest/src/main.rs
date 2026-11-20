//! airdroid-guest: PID 1 of the airDroid VM.
//!
//! Boots the Android container the way Waydroid's container manager does on
//! Linux (binderfs, image mounts, waydroid.prop, the LXC node list), runs the
//! Wayland compositor the Android hwcomposer draws to, and serves the host
//! over a virtio-serial port: frames and windows out, input, commands, app
//! management and files in.

mod apps;
mod compositor;
mod container;
mod exec;
mod hostlink;
mod sys;

use airdroid_proto::{self as proto, BootStage, ToGuest, ToHost};
use hostlink::HostLink;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn status(link: &HostLink, stage: BootStage, detail: &str) {
    logf!("{stage:?}: {detail}");
    link.send(&ToHost::Status { stage, detail: detail.to_string() });
}

fn early_init() {
    let _ = sys::mount("proc", "/proc", "proc", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None);
    let _ = sys::mount("sysfs", "/sys", "sysfs", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None);
    let _ = sys::mount("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID, Some("mode=0755"));
    let _ = sys::mkdir_p("/dev/pts", 0o755);
    let _ = sys::mount("devpts", "/dev/pts", "devpts", libc::MS_NOSUID | libc::MS_NOEXEC, Some("ptmxmode=0666,mode=0620"));
    let _ = sys::mkdir_p("/dev/shm", 0o1777);
    let _ = sys::mount("tmpfs", "/dev/shm", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, None);
    for d in ["/run", "/tmp", "/var/lib", "/mnt"] {
        let _ = sys::mkdir_p(d, 0o755);
    }
    let _ = sys::mount("tmpfs", "/run", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, Some("mode=0755"));
    let _ = sys::mount("tmpfs", "/tmp", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, Some("mode=1777"));
    let _ = sys::mount("debugfs", "/sys/kernel/debug", "debugfs", 0, None);
    let _ = sys::mkdir_p("/sys/fs/cgroup", 0o755);
    sys::sethostname("airdroid");
    let _ = sys::loopback_up();
    // frame buffers the host reads by physical address must not move
    let _ = std::fs::write("/proc/sys/vm/compact_unevictable_allowed", "0");
    let _ = std::fs::write("/proc/sys/kernel/panic", "-1");
    let _ = std::fs::write("/proc/sys/kernel/printk", "4 4 1 7");
    // Android wants these to be writable by its own processes
    let _ = std::fs::write("/proc/sys/kernel/perf_event_paranoid", "3");
    unsafe { libc::setsid() };
}

fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_string()
}

fn poweroff() -> ! {
    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// Paths the host may write with PutFile: Android's /data or /tmp.
fn guest_path(path: &str) -> Option<String> {
    if path.contains("..") {
        return None;
    }
    if let Some(rest) = path.strip_prefix("/data/") {
        return Some(format!("{}/data/{rest}", container::DATA_DISK));
    }
    if path.starts_with("/tmp/") {
        return Some(path.to_string());
    }
    None
}

fn put_file(req: u32, path: &str, offset: u64, data: &[u8], last: bool) -> Result<bool, String> {
    use std::io::{Seek, SeekFrom};
    let real = guest_path(path).ok_or_else(|| format!("refusing to write {path}"))?;
    if let Some(dir) = std::path::Path::new(&real).parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let _ = sys::chmod(&dir.to_string_lossy(), 0o777);
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(offset == 0)
        .open(&real)
        .map_err(|e| format!("{real}: {e}"))?;
    f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    f.write_all(data).map_err(|e| e.to_string())?;
    if last {
        let _ = sys::chmod(&real, 0o644);
        let _ = req;
    }
    Ok(last)
}

fn handle(link: &HostLink, inbox: &Arc<compositor::Inbox>, msg: ToGuest) {
    match msg {
        ToGuest::Hello { version } => {
            if version != proto::PROTOCOL_VERSION {
                logf!("host speaks protocol {version}, we speak {}", proto::PROTOCOL_VERSION);
            }
        }
        ToGuest::Exec { req, argv, android, stdin } => exec::spawn_for_host(link.clone(), req, argv, android, stdin),
        ToGuest::PutFile { req, path, offset, data, last } => match put_file(req, &path, offset, &data, last) {
            Ok(true) => link.send(&ToHost::Reply { req, ok: true, message: path }),
            Ok(false) => {}
            Err(e) => link.send(&ToHost::Reply { req, ok: false, message: e }),
        },
        ToGuest::InstallApk { req, path } => apps::spawn(link.clone(), req, apps::Action::Install(path)),
        ToGuest::RemoveApp { req, package } => apps::spawn(link.clone(), req, apps::Action::Remove(package)),
        ToGuest::LaunchApp { req, package } => apps::spawn(link.clone(), req, apps::Action::Launch(package)),
        ToGuest::ShowFullUi { req } => apps::spawn(link.clone(), req, apps::Action::FullUi),
        ToGuest::LaunchIntent { req, action, uri } => apps::spawn(link.clone(), req, apps::Action::Intent(action, uri)),
        ToGuest::SetProp { req, key, value } => apps::spawn(link.clone(), req, apps::Action::SetProp(key, value)),
        ToGuest::ListApps => apps::spawn(link.clone(), 0, apps::Action::List),
        ToGuest::Shutdown => {
            logf!("shutdown requested");
            std::thread::spawn(|| {
                if exec::android_running() {
                    let _ = exec::run(&["/system/bin/reboot", "-p", "shutdown,container"], true, &[]);
                    let pid = exec::ANDROID_INIT.load(Ordering::SeqCst);
                    for _ in 0..100 {
                        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
                poweroff();
            });
        }
        other => inbox.push(other),
    }
}

fn main() {
    let pid1 = std::process::id() == 1;
    if pid1 {
        early_init();
    }
    sys::start_reaper();
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let cfg = container::Config::from_cmdline(&cmdline);
    hostlink::console(&format!("airdroid: starting ({cfg:?})"));

    let (link, mut reader) = loop {
        match HostLink::open() {
            Ok(v) => break v,
            Err(e) => {
                hostlink::console(&format!("airdroid: waiting for the host port: {e}"));
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    };
    link.send(&ToHost::Hello { version: proto::PROTOCOL_VERSION, kernel: kernel_release() });

    let inbox = compositor::new_inbox();
    {
        let link = link.clone();
        let inbox = inbox.clone();
        std::thread::Builder::new()
            .name("host-rx".into())
            .spawn(move || loop {
                match proto::read_msg::<_, ToGuest>(&mut reader) {
                    Ok(m) => handle(&link, &inbox, m),
                    Err(e) => {
                        hostlink::console(&format!("airdroid: host link read: {e}"));
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            })
            .expect("host-rx thread");
    }

    if let Err(e) = boot(&link, &cfg, &inbox) {
        status(&link, BootStage::Failed, &e.to_string());
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
}

fn boot(link: &HostLink, cfg: &container::Config, inbox: &Arc<compositor::Inbox>) -> std::io::Result<()> {
    status(link, BootStage::Init, &format!("kernel {}", kernel_release()));
    container::setup_binder()?;
    container::mount_data_disk(|| status(link, BootStage::FormattingData, "preparing Android's data disk"))?;

    // user overrides kept on the data disk (like waydroid.cfg [properties])
    let mut extra = Vec::new();
    if let Ok(text) = std::fs::read_to_string(format!("{}/props.override", container::DATA_DISK)) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if !k.trim().is_empty() && !k.starts_with('#') {
                    extra.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
        }
    }
    let props = container::make_props(cfg, &extra);
    if let Err(e) = container::set_persistent_props(&[(
        "persist.waydroid.multi_windows",
        if cfg.multi_windows { "true" } else { "false" },
    )]) {
        logf!("cannot set persistent properties: {e}");
    }
    container::prepare_rootfs(cfg, &props)?;

    // the compositor must listen before the hwcomposer starts
    let socket = format!("{}/{}", container::XDG_RUNTIME, container::WAYLAND_DISPLAY);
    {
        let link = link.clone();
        let inbox = inbox.clone();
        let out = compositor::OutputCfg { width: cfg.width, height: cfg.height, refresh_mhz: 60_000, dpi: cfg.dpi };
        let socket = socket.clone();
        std::thread::Builder::new()
            .name("compositor".into())
            .spawn(move || {
                if let Err(e) = compositor::run(link.clone(), inbox, &socket, out) {
                    logf!("compositor failed: {e}");
                }
            })
            .expect("compositor thread");
    }
    for _ in 0..50 {
        if std::path::Path::new(&socket).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    status(link, BootStage::StartingAndroid, "starting Android");
    container::hold_input_fifos();
    let pid = container::start()?;
    exec::ANDROID_INIT.store(pid, Ordering::SeqCst);
    logf!("Android init is pid {pid}");

    // boot watcher
    {
        let link = link.clone();
        let multi = cfg.multi_windows;
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(500));
                if !exec::android_running() {
                    return;
                }
                if exec::getprop("sys.boot_completed") == "1" {
                    let _ = multi;
                    status(&link, BootStage::Booted, &format!("Android booted in {:.1} s", start.elapsed().as_secs_f32()));
                    apps::spawn(link.clone(), 0, apps::Action::List);
                    apps::watch_packages(link.clone());
                    return;
                }
            }
        });
    }

    let code = sys::wait_pid(pid);
    exec::ANDROID_INIT.store(0, Ordering::SeqCst);
    status(link, BootStage::Stopped, &format!("Android exited ({code})"));
    std::thread::sleep(Duration::from_millis(300));
    poweroff();
}
