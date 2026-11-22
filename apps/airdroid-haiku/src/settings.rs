//! settings.json in ~/config/settings/airDroid.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Each Android app in its own window (Waydroid's multi_windows).
    pub multi_windows: bool,
    /// The Android display in single-window mode.
    pub width: u32,
    pub height: u32,
    /// 0: Android's default density.
    pub dpi: u32,
    pub ram_mb: u64,
    pub cpus: u32,
    /// "VANILLA" or "GAPPS" (the system image channel).
    pub flavour: String,
    /// Send mouse clicks as touches (some games want touch).
    pub mouse_as_touch: bool,
    /// "auto" (virgl when there is a GPU with EGL), "virgl", "none", or
    /// "venus" (virgl, and Vulkan through Venus: experimental).
    pub gpu: String,
    /// The folder shared with Android (as /sdcard/airOS); "~/" is the home
    /// folder, empty shares nothing.
    pub shared_folder: String,
}

/// Android's CPUs on a host with this many hardware threads: all but two,
/// at least two and at most eight. The two are for the host's side of
/// every frame (QEMU's device threads, the renderer, this app's compositor):
/// with all four threads of a two-core machine given to Android, "Show
/// Android" stuttered in 8-11 % of its frames and apps started slower than
/// with two (1 %, and 460 ms instead of 570).
fn default_cpus(threads: u32) -> u32 {
    threads.saturating_sub(2).clamp(2, 8)
}

impl Default for Settings {
    fn default() -> Self {
        let threads = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4);
        Settings {
            multi_windows: true,
            width: 1280,
            height: 800,
            dpi: 0,
            ram_mb: 4096,
            cpus: default_cpus(threads),
            flavour: "VANILLA".into(),
            mouse_as_touch: false,
            gpu: "auto".into(),
            shared_folder: "~/Android".into(),
        }
    }
}

impl Settings {
    pub fn load(dir: &Path) -> Settings {
        std::fs::read_to_string(dir.join("settings.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// The settings window's answer: "key=value" lines (ram, cpus, gpu,
    /// dpi, touch, share); unknown keys and bad values are ignored.
    pub fn apply_text(&mut self, text: &str) {
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k {
                "ram" => {
                    if let Ok(n) = v.parse::<u64>() {
                        self.ram_mb = n.clamp(1024, 65536);
                    }
                }
                "cpus" => {
                    if let Ok(n) = v.parse::<u32>() {
                        self.cpus = n.clamp(1, 64);
                    }
                }
                "gpu" if ["auto", "virgl", "none", "venus"].contains(&v) => self.gpu = v.to_string(),
                "dpi" => {
                    if let Ok(n) = v.parse() {
                        self.dpi = n;
                    }
                }
                "touch" => self.mouse_as_touch = v == "1",
                "share" => self.shared_folder = v.trim().to_string(),
                _ => {}
            }
        }
    }

    pub fn save(&self, dir: &Path) {
        let _ = std::fs::create_dir_all(dir);
        if let Ok(t) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(dir.join("settings.json"), t);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_cpus_leave_the_host_two_threads() {
        assert_eq!([1, 2, 4, 6, 8, 12, 32].map(super::default_cpus), [2, 2, 2, 4, 6, 8, 8]);
    }

    use super::*;

    #[test]
    fn settings_window_text() {
        let mut s = Settings::default();
        s.apply_text("ram=6144\ncpus=6\ngpu=none\ndpi=213\ntouch=1\nshare=\nbogus=1\n");
        assert_eq!((s.ram_mb, s.cpus, s.gpu.as_str(), s.dpi, s.mouse_as_touch), (6144, 6, "none", 213, true));
        assert_eq!(s.shared_folder, "");
        s.apply_text("gpu=fast\nram=x\nshare=/boot/home/Shared\n");
        assert_eq!((s.gpu.as_str(), s.ram_mb, s.shared_folder.as_str()), ("none", 6144, "/boot/home/Shared"));
        s.apply_text("gpu=venus\n");
        assert_eq!(s.gpu, "venus");
    }
}
