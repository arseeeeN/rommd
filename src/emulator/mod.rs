pub mod azahar;
pub mod dolphin;
pub mod duckstation;
pub mod eden;
mod layout;
pub mod mgba;
pub mod ps1;
pub mod ryujinx;

use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::process::Child;

use crate::config::EmulatorConfig;
use crate::profile::ProfileBlob;
pub use layout::{FileSet, GameSaveDir, SaveLayout, SaveSet};

/// Command for spawning an emulator. On Unix the emulator becomes the leader of
/// its own process group, so stopping it and waiting for it cover every process
/// it runs. This matters for launchers that keep the real emulator as a child:
/// uruntime AppImages (Eden) stay alive as the parent, and killing only that
/// PID would orphan the emulator.
pub fn emulator_command(binary: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(binary);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// Resources created during launch that must be cleaned up after the game exits.
#[derive(Debug, Default)]
pub struct LaunchContext {
    pub temp_files: Vec<PathBuf>,
    pub temp_dirs: Vec<PathBuf>,
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

    /// Return the game identifier that can be derived from the ROM and the
    /// emulator's local state (config, cache files). Returns `None` when the
    /// identifier cannot be determined without user input.
    fn game_id_for_rom(&self, _config: &EmulatorConfig, _rom_path: &Path) -> Option<String> {
        None
    }

    /// Where this game's saves and save states live. Called before launch and
    /// again after exit, since some emulators only create the save location or
    /// the information to name it during play (Ryujinx's save slot,
    /// DuckStation's title cache).
    async fn save_layout(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        game_id: Option<&str>,
    ) -> SaveLayout;

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
}
