//! `~/.config/raven/desktop.toml` is owned by Raven Settings and read here
//! only for the look, so the player matches the rest of the desktop.
//! `~/.config/raven/player.toml` is the player's own.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const DEFAULT_ACCENT: &str = "#7AA2F7";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    #[default]
    Dark,
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme_mode: ThemeMode,
    pub accent: String,
    pub transparency: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self { theme_mode: ThemeMode::Dark, accent: DEFAULT_ACCENT.into(), transparency: true }
    }
}

/// The slice of desktop.toml the player cares about. Unknown keys are
/// ignored so Settings can grow without breaking us.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Desktop {
    pub appearance: Appearance,
}

impl Desktop {
    pub fn load() -> Desktop {
        std::fs::read_to_string(config_dir().join("desktop.toml"))
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerConfig {
    pub show_sidebar: bool,
    pub show_queue: bool,
    pub volume: f64,
    /// Resume each file where it was left, keyed by path.
    pub remember_position: bool,
    pub positions: std::collections::BTreeMap<String, f64>,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            show_sidebar: true,
            show_queue: true,
            volume: 1.0,
            remember_position: true,
            positions: Default::default(),
        }
    }
}

impl PlayerConfig {
    pub fn path() -> PathBuf {
        config_dir().join("player.toml")
    }

    pub fn load() -> PlayerConfig {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text =
            format!("# OwlPlayer preferences. Written by owl-player.\n{}", toml::to_string_pretty(self)?);
        std::fs::write(&path, text)?;
        Ok(())
    }

    /// Only remember a position that is worth resuming: not the first few
    /// seconds, and not the credits.
    pub fn remember(&mut self, path: &std::path::Path, position: f64, duration: f64) {
        if !self.remember_position || duration <= 0.0 {
            return;
        }
        let key = path.to_string_lossy().to_string();
        if position < 15.0 || position > duration - 30.0 {
            self.positions.remove(&key);
        } else {
            self.positions.insert(key, position);
        }
    }

    pub fn resume_for(&self, path: &std::path::Path) -> Option<f64> {
        if !self.remember_position {
            return None;
        }
        self.positions.get(&path.to_string_lossy().to_string()).copied()
    }
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("raven")
}
