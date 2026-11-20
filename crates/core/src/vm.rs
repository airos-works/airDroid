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
        a
    }

    /// Starts QEMU. The caller connects to `socket()` afterwards.
    pub fn spawn(&self) -> std::io::Result<Child> {
        std::fs::create_dir_all(&self.state_dir)?;
        let _ = std::fs::remove_file(self.socket());
        let _ = std::fs::remove_file(self.console_log());
        let log = std::fs::File::create(self.qemu_log())?;
        let mut cmd = Command::new(&self.qemu);
        if let RamBackend::NvmmMap(map) = &self.ram {
            let _ = std::fs::remove_file(map);
            cmd.env("QEMU_NVMM_MAP_FILE", map);
        }
        cmd.args(self.args()).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        let child = cmd.spawn()?;
        let _ = std::fs::write(self.state_dir.join("qemu.pid"), format!("{}\n", child.id()));
        Ok(child)
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
