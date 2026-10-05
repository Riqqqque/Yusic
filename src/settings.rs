use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub volume: f32,
    pub sidebar_wide: bool,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
    /// "best" or "standard" (128 kbps AAC only).
    pub quality: String,
    /// Keep playing similar songs when the queue ends.
    pub autoplay: bool,
    /// Download the next song while the current one plays.
    pub prefetch: bool,
    /// Look up synced lyrics on LRCLIB.
    pub lrclib: bool,
    pub lyrics_size: u32,
    /// The window's close button hides to the tray (false: quits).
    pub close_to_tray: bool,
    pub start_with_windows: bool,
    pub start_minimized: bool,
    /// Pause UI updates while a fullscreen game runs.
    pub game_mode: bool,
    /// Accent color as #rrggbb.
    pub accent: String,
    /// Content region (ISO country code).
    pub region: String,
    pub auto_update: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 0.5,
            sidebar_wide: true,
            width: 0,
            height: 0,
            maximized: false,
            quality: "best".into(),
            autoplay: true,
            prefetch: true,
            lrclib: true,
            lyrics_size: 20,
            close_to_tray: true,
            start_with_windows: false,
            start_minimized: false,
            game_mode: true,
            accent: "#ff0000".into(),
            region: "US".into(),
            auto_update: true,
        }
    }
}

impl Settings {
    pub fn load(dir: &Path) -> Self {
        std::fs::read(dir.join("settings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) {
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let tmp = dir.join("settings.json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(tmp, dir.join("settings.json"));
            }
        }
    }
}
