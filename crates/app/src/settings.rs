//! `~/.config/tutor/settings.toml`: what the window last looked like, so a
//! restart picks up where you left off. Written by the app; `tutor.toml`
//! stays the hand-edited file.

use std::{collections::BTreeMap, fs, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Name of the PC on screen.
    pub selected: String,
    pub talk: bool,
    pub hear: bool,
    /// Sidebar width in pixels.
    pub sidebar: i32,
    /// Per PC, by its name in `tutor.toml`.
    pub stations: BTreeMap<String, StationSettings>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StationSettings {
    /// Index into the screen quality choices.
    pub quality: u32,
    pub dad_cam: bool,
    pub dad_cam_at: (u16, u16),
}

impl Default for Settings {
    fn default() -> Self {
        Self { selected: String::new(), talk: true, hear: true, sidebar: 360, stations: BTreeMap::new() }
    }
}

fn path() -> PathBuf {
    crate::config::path().with_file_name("settings.toml")
}

/// Defaults when missing or unreadable: losing settings never stops the app.
pub fn load() -> Settings {
    let Ok(text) = fs::read_to_string(path()) else { return Settings::default() };
    toml::from_str(&text).unwrap_or_else(|e| {
        tracing::warn!(path = %path().display(), error = %e, "settings.ignored");
        Settings::default()
    })
}

pub fn save(settings: &Settings) {
    let body = match toml::to_string(settings) {
        Ok(b) => b,
        Err(e) => return tracing::warn!(error = %e, "settings.encode_failed"),
    };
    let path = path();
    let tmp = path.with_extension("toml.tmp");
    let text = format!("# Written by tutor whenever you change something; edit tutor.toml instead.\n{body}");
    if let Err(e) = fs::write(&tmp, text).and_then(|()| fs::rename(&tmp, &path)) {
        tracing::warn!(path = %path.display(), error = %e, "settings.save_failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut s = Settings { selected: "Logan's PC".into(), talk: false, ..Default::default() };
        s.stations.insert("Logan's PC".into(), StationSettings { quality: 1, dad_cam: true, dad_cam_at: (100, 200) });
        let back: Settings = toml::from_str(&toml::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        // Fields added later fall back to defaults.
        assert!(toml::from_str::<Settings>("talk = false").unwrap().hear);
    }
}
