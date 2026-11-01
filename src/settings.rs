use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub volume: f32,
    pub sidebar_wide: bool,
    /// Window size in logical pixels, independent of display scaling.
    /// (Older versions stored physical pixels as `width`/`height`; restoring
    /// those at scale 1 made the window grow on every launch. They're ignored.)
    pub window_width: f32,
    pub window_height: f32,
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
    /// After a song started from an artist's page: "shuffle" (their songs,
    /// shuffled), "order" (their songs in order) or "radio".
    pub artist_play: String,
    /// Shuffle albums, playlists and artists whenever they're played.
    pub always_shuffle: bool,
    /// Let autoplay learn from what's finished and what's skipped.
    pub smart_autoplay: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 0.5,
            sidebar_wide: true,
            window_width: 0.0,
            window_height: 0.0,
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
            artist_play: "shuffle".into(),
            always_shuffle: false,
            smart_autoplay: true,
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
