//! settings.json in ~/config/settings/airDroid.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// "auto" (virgl when there is a GPU with EGL), "virgl" or "none".
    pub gpu: String,
    /// The folder shared with Android (as /sdcard/airOS); "~/" is the home
    /// folder, empty shares nothing.
    pub shared_folder: String,
}

impl Default for Settings {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4);
        Settings {
            multi_windows: true,
            width: 1280,
            height: 800,
            dpi: 0,
            ram_mb: 4096,
            cpus: cpus.clamp(2, 8),
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

    pub fn save(&self, dir: &Path) {
        let _ = std::fs::create_dir_all(dir);
        if let Ok(t) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(dir.join("settings.json"), t);
        }
    }
}
