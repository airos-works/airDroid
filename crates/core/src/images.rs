//! Android system and vendor images: what `waydroid init` downloads.
//!
//! Waydroid's OTA channels (https://ota.waydro.id/{system,vendor}/...) list
//! builds as JSON; each has a zip (`url`, `filename`, `size`) whose SHA-256
//! is its `id`. airDroid uses the same channels and images, unchanged.
//! Downloads go through `curl` (air/OS ships it) and zips through `unzip`.

use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const SYSTEM_CHANNEL: &str = "https://ota.waydro.id/system";
pub const VENDOR_CHANNEL: &str = "https://ota.waydro.id/vendor";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Arm64,
}

impl Arch {
    pub fn host() -> Arch {
        if cfg!(target_arch = "aarch64") {
            Arch::Arm64
        } else {
            Arch::X86_64
        }
    }
    fn waydroid(self) -> &'static str {
        match self {
            Arch::X86_64 => "waydroid_x86_64",
            Arch::Arm64 => "waydroid_arm64",
        }
    }
}

/// "VANILLA" (no Google apps) or "GAPPS", like `waydroid init -s`.
pub fn system_ota(arch: Arch, flavour: &str) -> String {
    format!("{SYSTEM_CHANNEL}/lineage/{}/{flavour}.json", arch.waydroid())
}

/// airDroid needs the MAINLINE vendor (no host Android HALs).
pub fn vendor_ota(arch: Arch) -> String {
    format!("{VENDOR_CHANNEL}/{}/MAINLINE.json", arch.waydroid())
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Build {
    pub datetime: i64,
    pub filename: String,
    pub id: String,
    pub size: u64,
    pub url: String,
}

#[derive(serde::Deserialize)]
struct Channel {
    response: Vec<Build>,
}

fn curl() -> Command {
    let mut c = Command::new("curl");
    c.args(["-fsSL", "--retry", "3", "--connect-timeout", "20"]);
    c
}

/// The newest build listed on a channel.
pub fn latest(ota_url: &str) -> io::Result<Build> {
    let out = curl().arg(ota_url).stderr(Stdio::piped()).output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "{ota_url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let ch: Channel = serde_json::from_slice(&out.stdout).map_err(|e| io::Error::other(format!("{ota_url}: {e}")))?;
    ch.response
        .into_iter()
        .max_by_key(|b| b.datetime)
        .ok_or_else(|| io::Error::other(format!("{ota_url} lists no builds")))
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Progress reports: (what, done bytes, total bytes).
pub type Progress<'a> = &'a mut dyn FnMut(&str, u64, u64);

/// Downloads `build` into `dir/<filename>`, reporting progress; resumes a
/// partial download. Returns the zip's path once its hash matches.
pub fn download(build: &Build, dir: &Path, label: &str, progress: Progress, cancel: &Arc<AtomicBool>) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let zip = dir.join(&build.filename);
    if zip.exists() && sha256_file(&zip)? == build.id {
        return Ok(zip);
    }
    let part = dir.join(format!("{}.part", build.filename));
    let mut child = curl()
        .args(["-C", "-", "-o"])
        .arg(&part)
        .arg(&build.url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    loop {
        if let Some(st) = child.try_wait()? {
            if !st.success() {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
                return Err(io::Error::other(format!("downloading {}: {}", build.filename, err.trim())));
            }
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let done = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        progress(label, done, build.size);
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    progress(label, build.size, build.size);
    let hash = sha256_file(&part)?;
    if hash != build.id {
        let _ = std::fs::remove_file(&part);
        return Err(io::Error::other(format!("{}: checksum mismatch", build.filename)));
    }
    std::fs::rename(&part, &zip)?;
    Ok(zip)
}

/// Extracts `member` (system.img / vendor.img) from a zip into `dir`.
pub fn extract(zip: &Path, member: &str, dir: &Path) -> io::Result<PathBuf> {
    let st = Command::new("unzip").args(["-o", "-qq"]).arg(zip).arg(member).arg("-d").arg(dir).status()?;
    if !st.success() {
        return Err(io::Error::other(format!("unzip {} failed", zip.display())));
    }
    Ok(dir.join(member))
}

/// What is installed (written next to the images after a setup).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Installed {
    pub system: String,
    pub vendor: String,
    pub system_datetime: i64,
    pub vendor_datetime: i64,
}

pub fn installed(dir: &Path) -> Option<Installed> {
    if !dir.join("system.img").exists() || !dir.join("vendor.img").exists() {
        return None;
    }
    let text = std::fs::read_to_string(dir.join("installed.json")).unwrap_or_default();
    Some(serde_json::from_str(&text).unwrap_or_default())
}

/// The whole `waydroid init`: newest system and vendor, downloaded,
/// verified and extracted into `dir` (zips removed afterwards).
pub fn setup(dir: &Path, arch: Arch, flavour: &str, progress: Progress, cancel: &Arc<AtomicBool>) -> io::Result<Installed> {
    let system = latest(&system_ota(arch, flavour))?;
    let vendor = latest(&vendor_ota(arch))?;
    let dl = dir.join("download");
    let szip = download(&system, &dl, "Android system", progress, cancel)?;
    let vzip = download(&vendor, &dl, "Android vendor", progress, cancel)?;
    progress("Unpacking", 0, 1);
    extract(&szip, "system.img", &dl)?;
    extract(&vzip, "vendor.img", &dl)?;
    std::fs::rename(dl.join("system.img"), dir.join("system.img"))?;
    std::fs::rename(dl.join("vendor.img"), dir.join("vendor.img"))?;
    let _ = std::fs::remove_dir_all(&dl);
    let inst = Installed {
        system: system.filename,
        vendor: vendor.filename,
        system_datetime: system.datetime,
        vendor_datetime: vendor.datetime,
    };
    std::fs::write(dir.join("installed.json"), serde_json::to_string_pretty(&inst).unwrap_or_default())?;
    progress("Unpacking", 1, 1);
    Ok(inst)
}

/// `waydroid upgrade`: downloads the newest system/vendor when they are
/// newer than the installed ones and swaps them in (the running VM keeps
/// the files it has open). Returns the new state, or None when current.
pub fn upgrade(dir: &Path, arch: Arch, flavour: &str, progress: Progress, cancel: &Arc<AtomicBool>) -> io::Result<Option<Installed>> {
    let cur = installed(dir).unwrap_or_default();
    let system = latest(&system_ota(arch, flavour))?;
    let vendor = latest(&vendor_ota(arch))?;
    if system.datetime <= cur.system_datetime && vendor.datetime <= cur.vendor_datetime {
        return Ok(None);
    }
    let dl = dir.join("download");
    let mut next = cur.clone();
    if system.datetime > cur.system_datetime {
        let zip = download(&system, &dl, "Android system", progress, cancel)?;
        extract(&zip, "system.img", &dl)?;
        std::fs::rename(dl.join("system.img"), dir.join("system.img"))?;
        next.system = system.filename;
        next.system_datetime = system.datetime;
    }
    if vendor.datetime > cur.vendor_datetime {
        let zip = download(&vendor, &dl, "Android vendor", progress, cancel)?;
        extract(&zip, "vendor.img", &dl)?;
        std::fs::rename(dl.join("vendor.img"), dir.join("vendor.img"))?;
        next.vendor = vendor.filename;
        next.vendor_datetime = vendor.datetime;
    }
    let _ = std::fs::remove_dir_all(&dl);
    std::fs::write(dir.join("installed.json"), serde_json::to_string_pretty(&next).unwrap_or_default())?;
    Ok(Some(next))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Network: downloads ~1 GB. `cargo test -p airdroid-core -- --ignored`
    #[test]
    #[ignore]
    fn setup_then_upgrade_is_current() {
        let dir = std::env::temp_dir().join(format!("airdroid-images-{}", std::process::id()));
        let cancel = Arc::new(AtomicBool::new(false));
        let mut last = 0u64;
        let mut progress = |what: &str, done: u64, total: u64| {
            if done >= last + (100 << 20) || done == total {
                eprintln!("{what}: {} / {} MB", done >> 20, total >> 20);
                last = done;
            }
        };
        let inst = setup(&dir, Arch::host(), "VANILLA", &mut progress, &cancel).expect("setup");
        assert!(dir.join("system.img").metadata().unwrap().len() > 1 << 30);
        assert!(dir.join("vendor.img").exists());
        assert!(inst.system.contains("VANILLA"));
        assert!(upgrade(&dir, Arch::host(), "VANILLA", &mut progress, &cancel).expect("upgrade").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
