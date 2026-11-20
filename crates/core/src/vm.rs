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
    /// The RAM file (shared with the host).
    pub ram_file: PathBuf,
    pub ram_mb: u64,
    pub cpus: u32,
    pub accel: Accel,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub multi_windows: bool,
    pub gpu: String,
}

impl VmConfig {
    pub fn socket(&self) -> PathBuf {
        self.state_dir.join("airdroid.sock")
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
        );
        let mut a: Vec<String> = Vec::new();
        let mut push = |v: &[&str]| a.extend(v.iter().map(|s| s.to_string()));
        if let Some(fw) = &self.firmware {
            push(&["-L", &p(fw)]);
        }
        push(&["-nodefaults", "-no-user-config", "-nographic", "-no-reboot"]);
        push(&["-machine", "q35,memory-backend=ram0", "-accel", self.accel.name()]);
        push(&["-cpu", if self.accel == Accel::Tcg { "max" } else { "host" }]);
        push(&["-smp", &self.cpus.to_string(), "-m", &format!("{}M", self.ram_mb)]);
        push(&[
            "-object",
            &format!("memory-backend-file,id=ram0,size={}M,mem-path={},share=on", self.ram_mb, p(&self.ram_file)),
        ]);
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
                d.push_str(",discard=unmap");
            }
            push(&["-drive", &d, "-device", &format!("virtio-blk-pci,drive={id},serial={id}")]);
        }
        push(&["-netdev", "user,id=net0", "-device", "virtio-net-pci,netdev=net0,romfile="]);
        push(&["-device", "virtio-rng-pci", "-rtc", "base=utc,clock=host"]);
        a
    }

    /// Starts QEMU. The caller connects to `socket()` afterwards.
    pub fn spawn(&self) -> std::io::Result<Child> {
        std::fs::create_dir_all(&self.state_dir)?;
        let _ = std::fs::remove_file(self.socket());
        let _ = std::fs::remove_file(self.console_log());
        let log = std::fs::File::create(self.qemu_log())?;
        let mut cmd = Command::new(&self.qemu);
        cmd.args(self.args()).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        let child = cmd.spawn()?;
        let _ = std::fs::write(self.state_dir.join("qemu.pid"), format!("{}\n", child.id()));
        Ok(child)
    }
}
