//! `~/.config/tutor/tutor.toml`: this PC's devices and the kids' PCs.

use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

const STARTER: &str = r#"[me]
camera = "/dev/video0"      # C920
mic = "Jabra"               # substring of a PulseAudio source name; "" = default
speakers = ""               # substring of a sink name; "" = default

[[station]]
name = "Logan's PC"
host = "192.168.8.106"      # ssh target, may be user@host
headset = "Jabra"           # their mic + where your voice plays; "" = their defaults
camera = "/dev/video0"

[[station]]
name = "Danielle's PC"
host = "192.168.8.214"
headset = ""
camera = "/dev/video0"
"#;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub me: Me,
    #[serde(rename = "station")]
    pub stations: Vec<StationConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    #[serde(default = "default_camera")]
    pub camera: String,
    #[serde(default)]
    pub mic: String,
    #[serde(default)]
    pub speakers: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StationConfig {
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub headset: String,
    #[serde(default = "default_camera")]
    pub camera: String,
}

fn default_camera() -> String {
    "/dev/video0".into()
}

pub fn path() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("tutor/tutor.toml")
}

/// Loads the config, writing the starter file on first run.
pub fn load() -> Result<Config> {
    let path = path();
    if !path.exists() {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, STARTER)?;
        eprintln!("tutor: wrote starter config {}", path.display());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let config: Config = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    anyhow::ensure!(!config.stations.is_empty(), "{} lists no [[station]]", path.display());
    Ok(config)
}

#[cfg(test)]
mod tests {
    #[test]
    fn starter_parses() {
        let c: super::Config = toml::from_str(super::STARTER).unwrap();
        assert_eq!(c.stations.len(), 2);
        assert_eq!(c.stations[0].headset, "Jabra");
        assert_eq!(c.me.mic, "Jabra");
    }
}
