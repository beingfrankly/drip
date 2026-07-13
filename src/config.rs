//! Persistent configuration for `drip`, stored as TOML under the user's
//! config directory (e.g. `~/.config/drip/config.toml` on Linux).

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Top-level `drip` configuration.
///
/// This intentionally holds only the "bootstrap" fields that must be
/// resolvable before the SQLite database can even be opened. Everything
/// else that used to live here (`posts_folder`, `daily_notes_folder`,
/// `daily_note_format`, `default_sort`, `default_limit`, `default_tags`)
/// moved to the `settings` table -- see [`crate::settings`] and bd issue
/// drip-15n.9.8's design note for the full reasoning. `profiles` moved to
/// the `profiles`/`profile_sources`/`profile_tags` tables -- see
/// [`crate::profiles`] and bd issue drip-15n.9.3's design note.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Absolute path to the Obsidian vault. Empty until `drip init` sets it.
    #[serde(default)]
    pub vault_path: PathBuf,
    /// Optional override for the SQLite database file's location. `None`
    /// (the default) means "use the default location next to config.toml"
    /// -- see [`crate::db::default_db_path`]. Kept in `config.toml` (rather
    /// than, say, the database itself) because the DB's own location must
    /// be resolvable before the DB can be opened.
    #[serde(default)]
    pub db_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            vault_path: PathBuf::new(),
            db_path: None,
        }
    }
}

impl Config {
    /// Resolve the on-disk location of `config.toml`, without checking
    /// whether it actually exists yet.
    pub fn config_path() -> Result<PathBuf> {
        let proj_dirs = ProjectDirs::from("", "", "drip")
            .context("could not determine a config directory for this platform")?;
        Ok(proj_dirs.config_dir().join("config.toml"))
    }

    /// Load the config from disk, falling back to defaults if no config
    /// file exists yet. This does NOT create the file.
    pub fn load() -> Result<Config> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(Config::default());
        }

        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config file at {}", path.display()))?;
        let config: Config = toml::from_str(&contents)
            .with_context(|| format!("failed to parse config file at {}", path.display()))?;
        Ok(config)
    }

    /// Write this config to disk as pretty-printed TOML, creating the
    /// parent directory if needed.
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory at {}", parent.display())
            })?;
        }

        let contents = toml::to_string_pretty(self).context("failed to serialize config")?;
        std::fs::write(&path, contents)
            .with_context(|| format!("failed to write config file at {}", path.display()))?;
        Ok(())
    }
}
