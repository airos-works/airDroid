//! Where airDroid keeps things on air/OS, and where its VM pieces are.

use std::path::{Path, PathBuf};

pub struct Paths {
    /// Android images and the data disk (~/config/non-packaged/data/airDroid;
    /// ~/config/data is packagefs, read-only).
    pub data: PathBuf,
    /// Logs, sockets, the RAM map (~/config/cache/airDroid).
    pub state: PathBuf,
    /// settings.json (~/config/settings/airDroid).
    pub settings: PathBuf,
    /// Deskbar launchers for Android apps.
    pub launchers: PathBuf,
}

impl Paths {
    pub fn new() -> Paths {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/boot/home".into()));
        let config = home.join("config");
        Paths {
            data: config.join("non-packaged/data/airDroid"),
            state: config.join("cache/airDroid"),
            settings: config.join("settings/airDroid"),
            launchers: config.join("settings/deskbar/menu/Applications/Android apps"),
        }
    }

    pub fn images(&self) -> PathBuf {
        self.data.join("images")
    }

    pub fn userdata(&self) -> PathBuf {
        self.data.join("userdata.qcow2")
    }
}

/// The VM's files shipped with airDroid.
#[derive(Debug, Clone)]
pub struct Resources {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub qemu: PathBuf,
    pub firmware: Option<PathBuf>,
    /// Whether `qemu` is airDroid's (shares guest RAM); linuxvm's is not.
    pub qemu_shares_ram: bool,
}

fn first(c: &[PathBuf]) -> Option<PathBuf> {
    c.iter().find(|p| p.exists()).cloned()
}

impl Resources {
    /// Looks next to the executable (…/apps/airDroid → …/data/airDroid,
    /// …/lib/airDroid), in the system and user non-packaged trees, and in
    /// $AIRDROID_RESOURCES (development: everything in one directory).
    pub fn find(exe: Option<&Path>) -> Result<Resources, String> {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Ok(dir) = std::env::var("AIRDROID_RESOURCES") {
            let d = PathBuf::from(dir);
            let pick = |n: &str| d.join(n);
            if pick("vmlinuz").exists() {
                return Ok(Resources {
                    kernel: pick("vmlinuz"),
                    initrd: pick("initramfs.img"),
                    qemu: pick("qemu-system-x86_64"),
                    firmware: Some(pick("firmware")).filter(|f| f.exists()),
                    qemu_shares_ram: true,
                });
            }
        }
        if let Some(exe) = exe {
            // <root>/apps/airDroid
            if let Some(root) = exe.parent().and_then(|p| p.parent()) {
                roots.push(root.to_path_buf());
            }
        }
        for r in ["/boot/system", "/boot/system/non-packaged", "/boot/home/config/non-packaged"] {
            roots.push(PathBuf::from(r));
        }
        let data: Vec<PathBuf> = roots.iter().map(|r| r.join("data/airDroid")).collect();
        let lib: Vec<PathBuf> = roots.iter().map(|r| r.join("lib/airDroid")).collect();
        let kernel = first(&data.iter().map(|d| d.join("vmlinuz")).collect::<Vec<_>>())
            .ok_or("airDroid's Linux kernel (data/airDroid/vmlinuz) is missing")?;
        let base = kernel.parent().unwrap().to_path_buf();
        let initrd = base.join("initramfs.img");
        if !initrd.exists() {
            return Err(format!("{} is missing", initrd.display()));
        }
        let (qemu, shares) = match first(&lib.iter().map(|d| d.join("qemu-system-x86_64")).collect::<Vec<_>>()) {
            Some(q) => (q, true),
            None => match first(&[PathBuf::from("/boot/system/lib/linuxvm/qemu-system-x86_64")]) {
                Some(q) => (q, false),
                None => return Err("no QEMU found (lib/airDroid/qemu-system-x86_64)".into()),
            },
        };
        let firmware = first(&[
            base.join("firmware"),
            PathBuf::from("/boot/system/data/linuxvm/firmware"),
        ]);
        Ok(Resources { kernel, initrd, qemu, firmware, qemu_shares_ram: shares })
    }
}

/// Which Mesa QEMU's EGL (virglrenderer) should use, as glvnd environment:
/// the AMD userland (radeonsi) on machines with a DRM render node, else
/// Summit's private Mesa (Zink on a Vulkan driver such as NVK, or
/// llvmpipe). None when no EGL-capable Mesa is installed.
pub fn gl_environment() -> Option<(Vec<(String, String)>, bool)> {
    let amd = Path::new("/boot/system/non-packaged/amdgpu");
    let has_render_node = std::fs::read_dir("/dev/dri").map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("renderD"))).unwrap_or(false);
    let system_libs = "%A/lib:/boot/home/config/non-packaged/lib:/boot/home/config/lib:/boot/system/non-packaged/lib:/boot/system/lib";
    let (vendor, lib, hardware) = if has_render_node && amd.join("add-ons/opengl/egl_vendor.d/50_mesa.json").exists() {
        (amd.join("add-ons/opengl/egl_vendor.d/50_mesa.json"), amd.join("lib"), true)
    } else {
        let m = Path::new("/boot/system/lib/summit-webkit/mesa");
        if !m.join("egl_vendor.d/50_mesa.json").exists() {
            return None;
        }
        // a Vulkan driver means Zink runs on a GPU; else it is llvmpipe
        let vulkan = std::fs::read_dir("/boot/system/add-ons/vulkan/icd.d").map(|d| d.count() > 0).unwrap_or(false)
            || std::fs::read_dir("/boot/system/non-packaged/add-ons/vulkan/icd.d").map(|d| d.count() > 0).unwrap_or(false);
        (m.join("egl_vendor.d/50_mesa.json"), m.join("lib"), vulkan)
    };
    let env = vec![
        ("__EGL_VENDOR_LIBRARY_FILENAMES".to_string(), vendor.to_string_lossy().into_owned()),
        ("LIBRARY_PATH".to_string(), format!("{}:{system_libs}", lib.to_string_lossy())),
    ];
    Some((env, hardware))
}
