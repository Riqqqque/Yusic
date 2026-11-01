//! What the user finishes and what they skip, kept on this PC
//! (`<data>\taste.json`). Autoplay uses it to leave out songs and artists
//! that keep getting skipped, and to pick a better seed when it drifts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Plays and early skips.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Count {
    pub plays: u32,
    pub skips: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Taste {
    tracks: HashMap<String, Count>,
    artists: HashMap<String, Count>,
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    dirty: bool,
}

/// Keeps the file small; the least active entries go first.
const MAX_TRACKS: usize = 6000;
const MAX_ARTISTS: usize = 2000;

impl Taste {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("taste.json");
        let mut t: Taste = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        t.path = path;
        t
    }

    pub fn save(&mut self) {
        if !self.dirty || self.path.as_os_str().is_empty() {
            return;
        }
        prune(&mut self.tracks, MAX_TRACKS);
        prune(&mut self.artists, MAX_ARTISTS);
        if let Ok(json) = serde_json::to_vec(self) {
            let tmp = self.path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &self.path).is_ok() {
                self.dirty = false;
            }
        }
    }

    pub fn played(&mut self, track: &str, artist: Option<&str>) {
        self.tracks.entry(track.to_owned()).or_default().plays += 1;
        if let Some(a) = artist {
            self.artists.entry(a.to_owned()).or_default().plays += 1;
        }
        self.dirty = true;
    }

    /// `weight` 1 for a skip, more for a dislike.
    pub fn skipped(&mut self, track: &str, artist: Option<&str>, weight: u32) {
        self.tracks.entry(track.to_owned()).or_default().skips += weight;
        if let Some(a) = artist {
            self.artists.entry(a.to_owned()).or_default().skips += weight;
        }
        self.dirty = true;
    }

    /// A song the user keeps skipping and rarely finishes.
    pub fn unwanted_track(&self, id: &str) -> bool {
        self.tracks.get(id).is_some_and(|c| c.skips >= 2 && c.skips > c.plays * 2)
    }

    /// An artist the user skips far more often than they listen to.
    pub fn unwanted_artist(&self, id: &str) -> bool {
        self.artists.get(id).is_some_and(|c| c.skips >= 4 && c.skips > c.plays * 3)
    }
}

fn prune(map: &mut HashMap<String, Count>, max: usize) {
    if map.len() <= max {
        return;
    }
    let mut v: Vec<(String, Count)> = map.drain().collect();
    v.sort_by_key(|(_, c)| std::cmp::Reverse(c.plays + c.skips));
    v.truncate(max);
    map.extend(v);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_from_skips() {
        let mut t = Taste::default();
        t.skipped("a", Some("x"), 1);
        assert!(!t.unwanted_track("a"));
        t.skipped("a", Some("x"), 1);
        assert!(t.unwanted_track("a"));
        // Finishing it once outweighs those skips again.
        t.played("a", Some("x"));
        assert!(!t.unwanted_track("a"));
        for _ in 0..4 {
            t.skipped("b", Some("y"), 1);
        }
        assert!(t.unwanted_artist("y"));
        assert!(!t.unwanted_artist("x"));
    }
}
