//! The Android VM: QEMU's command line and process.
//!
//! air/OS runs it under NVMM with the QEMU from the `linuxvm` package (or
//! airDroid's own copy); Linux development hosts use KVM. Guest RAM is a
//! shared file so the host can read Android's frame buffers in place:
//! on air/OS that file lives in /var/shared_memory, a ramfs.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    Kvm,
    Nvmm,
    Tcg,
}

impl Accel {
    pub fn detect() -> Accel {
        // AIRDROID_ACCEL=tcg|kvm|nvmm forces one (for testing)
        match std::env::var("AIRDROID_ACCEL").as_deref() {
            Ok("tcg") => return Accel::Tcg,
            Ok("kvm") => return Accel::Kvm,
            Ok("nvmm") => return Accel::Nvmm,
            _ => {}
        }
        if Path::new("/dev/misc/nvmm").exists() {
            Accel::Nvmm
        } else if Path::new("/dev/kvm").exists() {
            Accel::Kvm
        } else {
            Accel::Tcg
        }
    }
    fn name(self) -> &'static str {
        match self {
            Accel::Kvm => "kvm",
            Accel::Nvmm => "nvmm",
            Accel::Tcg => "tcg",
        }
    }
}

/// How the host gets at guest RAM (see guestmem.rs).
#[derive(Debug, Clone)]
pub enum RamBackend {
    /// memory-backend-file,share=on at this path (Linux/KVM).
    SharedFile(PathBuf),
    /// airDroid's QEMU logs the NVMM RAM areas to this file (air/OS).
    NvmmMap(PathBuf),
}

impl RamBackend {
    pub fn for_platform(state_dir: &Path) -> RamBackend {
        if cfg!(target_os = "haiku") {
            RamBackend::NvmmMap(state_dir.join("ram.map"))
        } else {
            RamBackend::SharedFile(PathBuf::from(format!("/dev/shm/airdroid-{}.ram", std::process::id())))
        }
    }
}

#[derive(Debug, Clone)]
pub struct VmConfig {
    pub qemu: PathBuf,
    /// Directory with QEMU's firmware (bios-256k.bin, ...), if not built in.
    pub firmware: Option<PathBuf>,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub system_img: PathBuf,
    pub vendor_img: PathBuf,
    pub userdata: PathBuf,
    /// Where the console log, QEMU log and sockets go.
    pub state_dir: PathBuf,
    pub ram: RamBackend,
    pub ram_mb: u64,
    pub cpus: u32,
    pub accel: Accel,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub multi_windows: bool,
    /// "none" (Android renders with SwiftShader), "virgl" (virtio-gpu with
    /// virglrenderer on the host's GPU through EGL) or "venus" (virgl for
    /// OpenGL ES, and Vulkan through Venus on the host's Vulkan driver).
    pub gpu: String,
    /// Extra environment for QEMU (which Mesa its EGL uses).
    pub env: Vec<(String, String)>,
    /// A host folder Android sees as /sdcard/airOS (virtio-9p, tag
    /// "airdroid_share").
    pub share: Option<PathBuf>,
    /// The host app reads GPU frames through QEMU (`present_socket`):
    /// only for a host that does (the air/OS app), as the guest then no
    /// longer copies them into its RAM.
    pub host_present: bool,
    /// The host's time zone (an Olson ID) for Android to follow.
    pub timezone: Option<String>,
    /// The host's language (a BCP 47 tag, "en-AU") for Android to start in.
    pub locale: Option<String>,
}

impl VmConfig {
    /// QEMU's socket for the guest daemon's port. Not every file system
    /// takes Unix sockets (air/OS's live-session overlay does not): the
    /// first directory where binding one works.
    pub fn socket(&self) -> PathBuf {
        socket_dir(&self.state_dir).join("airdroid.sock")
    }
    /// QEMU's socket for reading GPU frames straight into the app's window
    /// bitmaps (qemu/patches/0103): on air/OS with a GPU (virgl, venus) and
    /// `host_present`, unless AIRDROID_PRESENT=guest (the guest copies
    /// frames into its RAM first).
    pub fn present_socket(&self) -> Option<PathBuf> {
        let on = cfg!(target_os = "haiku")
            && self.host_present
            && (self.gpu == "virgl" || self.gpu == "venus")
            && std::env::var("AIRDROID_PRESENT").map_or(true, |v| v != "guest");
        on.then(|| socket_dir(&self.state_dir).join("present.sock"))
    }
    pub fn console_log(&self) -> PathBuf {
        self.state_dir.join("console.log")
    }
    pub fn qemu_log(&self) -> PathBuf {
        self.state_dir.join("qemu.log")
    }
    pub fn ram_bytes(&self) -> u64 {
        self.ram_mb << 20
    }

    pub fn args(&self) -> Vec<String> {
        let p = |p: &Path| p.to_string_lossy().into_owned();
        let cmdline = format!(
            "console=ttyS0 panic=-1 quiet loglevel=4 tsc=reliable rdinit=/init airdroid.width={} airdroid.height={} airdroid.dpi={} airdroid.multiwin={} airdroid.gpu={}",
            self.width,
            self.height,
            self.dpi,
            if self.multi_windows { 1 } else { 0 },
            self.gpu
        ) + if self.present_socket().is_some() { " airdroid.present=host" } else { "" }
            + &self
                .timezone
                .as_deref()
                .filter(|z| !z.is_empty() && z.chars().all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c)))
                .map(|z| format!(" airdroid.tz={z}"))
                .unwrap_or_default()
            + &self
                .locale
                .as_deref()
                .filter(|l| !l.is_empty() && l.len() <= 35 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
                .map(|l| format!(" airdroid.locale={l}"))
                .unwrap_or_default();
        let mut a: Vec<String> = Vec::new();
        let mut push = |v: &[&str]| a.extend(v.iter().map(|s| s.to_string()));
        if let Some(fw) = &self.firmware {
            push(&["-L", &p(fw)]);
        }
        push(&["-nodefaults", "-no-user-config", "-no-reboot"]);
        if self.gpu == "virgl" || self.gpu == "venus" {
            // QEMU's GL context: EGL without a window (AIRDROID_QEMU_DISPLAY
            // overrides it, e.g. "sdl,gl=on" on a host without a render node)
            let display = std::env::var("AIRDROID_QEMU_DISPLAY").unwrap_or_else(|_| "egl-headless".into());
            push(&["-display", &display]);
            if self.gpu == "venus" {
                // Vulkan too: Venus needs blob resources mapped through a
                // host memory window
                push(&["-device", "virtio-gpu-gl-pci,blob=on,hostmem=4G,venus=on"]);
            } else {
                push(&["-device", "virtio-gpu-gl-pci"]);
            }
        } else {
            push(&["-display", "none"]);
        }
        let machine = match self.ram {
            RamBackend::SharedFile(_) => "q35,memory-backend=ram0",
            RamBackend::NvmmMap(_) => "q35",
        };
        push(&["-machine", machine, "-accel", self.accel.name()]);
        push(&["-cpu", if self.accel == Accel::Tcg { "max" } else { "host" }]);
        push(&["-smp", &self.cpus.to_string(), "-m", &format!("{}M", self.ram_mb)]);
        if let RamBackend::SharedFile(f) = &self.ram {
            push(&["-object", &format!("memory-backend-file,id=ram0,size={}M,mem-path={},share=on", self.ram_mb, p(f))]);
        }
        push(&["-kernel", &p(&self.kernel), "-initrd", &p(&self.initrd), "-append", &cmdline]);
        push(&["-chardev", &format!("file,id=console,path={}", p(&self.console_log())), "-serial", "chardev:console"]);
        push(&["-device", "virtio-serial-pci"]);
        push(&["-chardev", &format!("socket,id=airdroid,path={},server=on,wait=off", p(&self.socket()))]);
        push(&["-device", &format!("virtserialport,chardev=airdroid,name={}", airdroid_proto::PORT_NAME)]);
        for (id, file, ro, fmt) in [
            ("system", &self.system_img, true, "raw"),
            ("vendor", &self.vendor_img, true, "raw"),
            ("userdata", &self.userdata, false, "qcow2"),
        ] {
            let mut d = format!("file={},if=none,id={id},format={fmt},cache=writeback,aio=threads", p(file));
            if ro {
                d.push_str(",readonly=on");
            } else {
                d.push_str("");
            }
            push(&["-drive", &d, "-device", &format!("virtio-blk-pci,drive={id},serial={id}")]);
        }
        push(&["-netdev", "user,id=net0", "-device", "virtio-net-pci,netdev=net0,romfile="]);
        push(&["-device", "virtio-rng-pci", "-rtc", "base=utc,clock=host"]);
        if let Some(dir) = &self.share {
            // air/OS: passthrough (the app is root there, and the owners
            // Android sets mean nothing to air/OS); elsewhere a user can't
            // chown, so ownership and modes live in extended attributes
            let model = if cfg!(target_os = "haiku") { "passthrough" } else { "mapped-xattr" };
            push(&["-fsdev", &format!("local,id=share,path={},security_model={model},multidevs=remap", p(dir))]);
            push(&["-device", "virtio-9p-pci,fsdev=share,mount_tag=airdroid_share"]);
        }
        a
    }

    /// Starts QEMU. The caller connects to `socket()` afterwards.
    pub fn spawn(&self) -> std::io::Result<Child> {
        self.end_leftover();
        std::fs::create_dir_all(&self.state_dir)?;
        let _ = std::fs::remove_file(self.socket());
        let _ = std::fs::remove_file(self.console_log());
        raise_fd_limit();
        let log = std::fs::File::create(self.qemu_log())?;
        let mut cmd = Command::new(&self.qemu);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        // Venus runs in virglrenderer's render server, next to QEMU
        let mut qemu_end = None;
        if self.gpu == "venus" {
            if let Some(dir) = self.qemu.parent() {
                let server = dir.join("virgl_render_server");
                if cfg!(target_os = "haiku") {
                    // We start it and give QEMU its socket: QEMU has a GPU
                    // device by the time virglrenderer would fork it, and
                    // such a fork takes an air/OS machine with an NVIDIA
                    // GPU down.
                    match spawn_render_server(&server, &log) {
                        Ok(fd) => {
                            cmd.env("AIRDROID_RENDER_SERVER_FD", fd.to_string());
                            qemu_end = Some(fd);
                        }
                        Err(e) => eprintln!("airDroid: {}: {e}", server.display()),
                    }
                } else {
                    cmd.env("RENDER_SERVER_EXEC_PATH", server);
                }
            }
        }
        if let Some(s) = self.present_socket() {
            let _ = std::fs::remove_file(&s);
            cmd.env("AIRDROID_PRESENT_SOCKET", s);
        }
        if let RamBackend::NvmmMap(map) = &self.ram {
            let _ = std::fs::remove_file(map);
            cmd.env("QEMU_NVMM_MAP_FILE", map);
        }
        cmd.args(self.args()).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        let child = cmd.spawn();
        if let Some(fd) = qemu_end {
            // QEMU has it now (or did not start: the server then exits)
            unsafe { libc::close(fd) };
        }
        let child = child?;
        let _ = std::fs::write(self.state_dir.join("qemu.pid"), format!("{}\n", child.id()));
        Ok(child)
    }
}

impl VmConfig {
    /// Ends the QEMU an earlier airDroid left behind: the app was killed or
    /// crashed, and its VM runs on, with Android's data disk open. A second
    /// VM on that disk would destroy what is on it.
    fn end_leftover(&self) {
        let path = self.state_dir.join("qemu.pid");
        let Some(pid) = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<i32>().ok()) else {
            return;
        };
        if pid > 1 && is_qemu(pid) {
            eprintln!("airDroid: ending the VM an earlier airDroid left running (process {pid})");
            let gone = |signal: i32, tenths: u32| {
                unsafe { libc::kill(pid, signal) };
                for _ in 0..tenths {
                    if !is_qemu(pid) {
                        return true;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                !is_qemu(pid)
            };
            if !gone(libc::SIGTERM, 50) && !gone(libc::SIGKILL, 30) {
                eprintln!("airDroid: process {pid} does not end");
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Whether process `pid` is a QEMU (the number alone may be another
/// program's by now).
#[cfg(target_os = "haiku")]
fn is_qemu(pid: i32) -> bool {
    let mut info: libc::team_info = unsafe { std::mem::zeroed() };
    if unsafe { libc::get_team_info(pid, &mut info) } != 0 {
        return false;
    }
    let args: Vec<u8> = info.args.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&args).contains("qemu-system")
}

#[cfg(not(target_os = "haiku"))]
fn is_qemu(pid: i32) -> bool {
    // (a zombie has no command line: it holds nothing open)
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains("qemu-system"))
}

/// QEMU and the render server (which inherit our limit) need more file
/// descriptors than air/OS's 256 by default: one per GPU resource and blob,
/// per Venus context a few, and the NVIDIA driver one per memory mapping.
/// Without them Vulkan devices and blobs fail to be created once a few apps
/// run.
fn raise_fd_limit() {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0 && lim.rlim_cur < lim.rlim_max {
        lim.rlim_cur = lim.rlim_max;
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) };
    }
}

/// Whether this machine's Vulkan driver can carry Android's window buffers
/// through Venus, asked of the render server ("--vk-check": the extensions
/// the guest's driver wants before it offers Android a swapchain, and host
/// memory import); the reason when it cannot. With such a driver Vulkan
/// apps would get no window at all, where Android's own lavapipe draws
/// them on the CPU.
pub fn venus_check(server: &Path) -> Result<(), String> {
    if !cfg!(target_os = "haiku") {
        return Ok(());
    }
    let mut child = Command::new(server)
        .arg("--vk-check")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", server.display()))?;
    // a driver that hangs must not hang the start
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the Vulkan driver did not answer".into());
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    if status.success() {
        return Ok(());
    }
    let mut out = String::new();
    if let Some(mut o) = child.stdout.take() {
        use std::io::Read;
        let _ = o.read_to_string(&mut out);
    }
    let said: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    Err(if said.is_empty() { format!("the check failed ({status})") } else { said.join("; ") })
}

/// Starts virglrenderer's render server on one end of a socket pair
/// (SOCK_SEQPACKET, what its protocol needs) and returns the other end, which
/// the next child (QEMU) inherits. The server exits when that end closes.
fn spawn_render_server(server: &Path, log: &std::fs::File) -> std::io::Result<i32> {
    let mut fds = [0i32; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (ours, theirs) = (fds[0], fds[1]);
    let cloexec = |fd: i32, on: bool| unsafe {
        libc::fcntl(fd, libc::F_SETFD, if on { libc::FD_CLOEXEC } else { 0 });
    };
    // the server gets only its end
    cloexec(ours, true);
    let spawned = Command::new(server)
        .arg("--socket-fd")
        .arg(theirs.to_string())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
        .spawn();
    unsafe { libc::close(theirs) };
    match spawned {
        Ok(mut child) => {
            // nobody else waits for it
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            cloexec(ours, false);
            Ok(ours)
        }
        Err(e) => {
            unsafe { libc::close(ours) };
            Err(e)
        }
    }
}

impl VmConfig {
    /// Maps guest RAM; call once the guest runs (QEMU has set RAM up by then).
    pub fn open_guest_mem(&self) -> std::io::Result<crate::guestmem::GuestMem> {
        use crate::guestmem::{q35_lowmem, GuestMem};
        match &self.ram {
            RamBackend::SharedFile(f) => {
                let m = GuestMem::open_file(f, self.ram_bytes(), q35_lowmem(self.ram_bytes()));
                // QEMU and we keep it mapped; nothing else needs the name
                let _ = std::fs::remove_file(f);
                m
            }
            #[cfg(target_os = "haiku")]
            RamBackend::NvmmMap(map) => GuestMem::from_nvmm_map(map),
            #[cfg(not(target_os = "haiku"))]
            RamBackend::NvmmMap(_) => Err(std::io::Error::other("NVMM RAM sharing needs air/OS")),
        }
    }
}

/// The first of `preferred`, /tmp and /var/shared_memory that can hold a
/// Unix socket (checked once per process).
pub fn socket_dir(preferred: &Path) -> PathBuf {
    use std::sync::OnceLock;
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
        let candidates = [
            preferred.to_path_buf(),
            std::env::temp_dir().join(format!("airDroid-{user}")),
            PathBuf::from(format!("/var/shared_memory/airDroid-{user}")),
        ];
        for d in &candidates {
            let _ = std::fs::create_dir_all(d);
            let probe = d.join(format!(".probe-{}", std::process::id()));
            let _ = std::fs::remove_file(&probe);
            if std::os::unix::net::UnixListener::bind(&probe).is_ok() {
                let _ = std::fs::remove_file(&probe);
                return d.clone();
            }
        }
        preferred.to_path_buf()
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(timezone: Option<&str>) -> VmConfig {
        VmConfig {
            qemu: "qemu".into(),
            firmware: None,
            kernel: "k".into(),
            initrd: "i".into(),
            system_img: "s".into(),
            vendor_img: "v".into(),
            userdata: "u".into(),
            state_dir: "/tmp/airdroid-test".into(),
            ram: RamBackend::SharedFile("/tmp/airdroid-test/ram".into()),
            ram_mb: 2048,
            cpus: 2,
            accel: Accel::Tcg,
            width: 1280,
            height: 800,
            dpi: 0,
            multi_windows: true,
            gpu: "none".into(),
            env: Vec::new(),
            share: None,
            host_present: false,
            timezone: timezone.map(str::to_string),
            locale: None,
        }
    }
    fn cmdline(c: &VmConfig) -> String {
        let a = c.args();
        let i = a.iter().position(|x| x == "-append").unwrap();
        a[i + 1].clone()
    }

    #[test]
    fn language_goes_on_the_command_line() {
        let with = |locale: &str| cmdline(&VmConfig { locale: Some(locale.into()), ..config(Some("Australia/Sydney")) });
        assert!(with("en-AU").ends_with(" airdroid.tz=Australia/Sydney airdroid.locale=en-AU"));
        assert!(with("zh-Hans-CN").ends_with(" airdroid.locale=zh-Hans-CN"));
        // nothing that could add kernel arguments
        assert!(!with("en init=/bin/sh").contains("airdroid.locale"));
        assert!(!with("en_AU").contains("airdroid.locale"));
        assert!(!with("").contains("airdroid.locale"));
    }

    #[test]
    fn time_zone_goes_on_the_command_line() {
        assert!(cmdline(&config(Some("Australia/Sydney"))).ends_with(" airdroid.tz=Australia/Sydney"));
        assert!(cmdline(&config(Some("America/Port-au-Prince"))).contains(" airdroid.tz=America/Port-au-Prince"));
        assert!(cmdline(&config(Some("Etc/GMT+11"))).contains(" airdroid.tz=Etc/GMT+11"));
        // nothing that could add kernel arguments
        assert!(!cmdline(&config(Some("UTC init=/bin/sh"))).contains("airdroid.tz"));
        assert!(!cmdline(&config(Some(""))).contains("airdroid.tz"));
        assert!(!cmdline(&config(None)).contains("airdroid.tz"));
    }
}
