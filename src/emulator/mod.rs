pub mod dolphin;
pub mod duckstation;
pub mod mgba;
pub mod ps1;
pub mod ryujinx;

use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::process::Child;

use crate::config::EmulatorConfig;
use crate::profile::ProfileBlob;

/// Resources created during launch that must be cleaned up after the game exits.
#[derive(Debug, Default)]
pub struct LaunchContext {
    pub temp_files: Vec<PathBuf>,
    pub temp_dirs: Vec<PathBuf>,
}

/// Filename prefixes used to scope save and savestate file operations to the
/// current game. Saves and savestates frequently use different naming
/// conventions within the same emulator (e.g. DuckStation defaults to game
/// title for memory cards but uses the disc serial for savestates). An empty
/// string means "no filtering — match all files in the directory".
#[derive(Debug, Clone, Default)]
pub struct SaveFilter {
    pub saves: String,
    pub states: String,
}

impl SaveFilter {
    pub fn both(prefix: impl Into<String>) -> Self {
        let s = prefix.into();
        Self { saves: s.clone(), states: s }
    }

    pub fn is_unscoped(&self) -> bool {
        self.saves.is_empty() && self.states.is_empty()
    }
}

#[async_trait]
pub trait Emulator: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;

    /// Apply the profile overlay (if any) and spawn the emulator process.
    async fn launch(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        game_id: Option<&str>,
        profile: Option<&ProfileBlob>,
    ) -> Result<(Child, LaunchContext)>;

    /// Read the emulator's current per-game config for `game_id`, clean it,
    /// and return a portable profile blob. For DuckStation, `game_id` is the disc serial.
    async fn snapshot(&self, config: &EmulatorConfig, game_id: &str) -> Result<ProfileBlob>;

    fn saves_dir(&self, config: &EmulatorConfig) -> PathBuf;
    fn states_dir(&self, config: &EmulatorConfig) -> PathBuf;

    /// Return the game identifier that can be derived from the ROM and the
    /// emulator's local state (config, cache files). Returns `None` when the
    /// identifier cannot be determined without user input.
    fn game_id_for_rom(&self, _config: &EmulatorConfig, _rom_path: &Path) -> Option<String> {
        None
    }

    /// Compute the filename prefixes for save and savestate file filtering.
    /// The default implementation uses `game_id` for both; override when
    /// saves and savestates follow different naming conventions.
    fn save_filter(
        &self,
        _config: &EmulatorConfig,
        _rom_path: &Path,
        game_id: Option<&str>,
    ) -> SaveFilter {
        SaveFilter::both(game_id.unwrap_or(""))
    }
}
