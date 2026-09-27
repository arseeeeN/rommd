pub mod dolphin;
pub mod duckstation;
pub mod eden;
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

/// A game's save stored as a whole directory (Switch emulators).
#[derive(Debug, Clone)]
pub struct GameSaveDir {
    /// Directory whose contents are the game's save.
    pub dir: PathBuf,
    /// Root folder name inside the uploaded zip (the title ID, uppercase).
    pub archive_root: String,
    /// Directories that must receive the same contents when a save is restored
    /// (Ryujinx's working copy `1/` alongside the committed `0/`).
    pub mirrors: Vec<PathBuf>,
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

    async fn saves_dir(&self, config: &EmulatorConfig) -> PathBuf;

    /// Directory where this emulator stores save states, or `None` if the
    /// emulator does not support save states (e.g. Ryujinx, Eden).
    async fn states_dir(&self, config: &EmulatorConfig) -> Option<PathBuf>;

    /// Return the game identifier that can be derived from the ROM and the
    /// emulator's local state (config, cache files). Returns `None` when the
    /// identifier cannot be determined without user input.
    fn game_id_for_rom(&self, _config: &EmulatorConfig, _rom_path: &Path) -> Option<String> {
        None
    }

    /// Locate this game's directory save. Returning `Some` switches sync to the
    /// whole-directory zip path; `None` falls back to flat-file sync of
    /// `saves_dir()`. The directory need not exist yet if the emulator can pick
    /// it up when created by rommd (Eden); Ryujinx returns `None` until it has
    /// created the slot itself.
    async fn game_saves_dir(
        &self,
        _config: &EmulatorConfig,
        _game_id: &str,
    ) -> Option<GameSaveDir> {
        None
    }

    /// Emulator IDs whose RomM saves can be restored by this emulator. Empty
    /// means only saves tagged with this emulator's own `id()`.
    fn save_family(&self) -> &'static [&'static str] {
        &[]
    }

    fn accepts_saves_from(&self, emulator: Option<&str>) -> bool {
        match emulator {
            Some(e) => e == self.id() || self.save_family().contains(&e),
            None => false,
        }
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
