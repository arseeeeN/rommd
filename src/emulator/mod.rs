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
use std::sync::Arc;
use tokio::process::Child;

use crate::config::EmulatorConfig;
use crate::profile::ProfileBlob;
pub use layout::{CreateDir, FileSet, GameSaveDir, SaveLayout, SaveSet};

/// Every supported emulator. To add one, implement `Emulator` in its own
/// module here and list it.
pub fn all() -> Vec<Arc<dyn Emulator>> {
    vec![
        Arc::new(duckstation::DuckStation),
        Arc::new(mgba::MGba),
        Arc::new(dolphin::Dolphin),
        Arc::new(ryujinx::Ryujinx),
        Arc::new(eden::Eden),
        Arc::new(azahar::Azahar),
    ]
}

/// Command for spawning an emulator. On Unix the emulator becomes the leader of
/// its own process group, so stopping it and waiting for it cover every process
/// it runs. This matters for launchers that keep the real emulator as a child:
/// uruntime AppImages (Eden) stay alive as the parent, and killing only that
/// PID would orphan the emulator.
pub fn emulator_command(binary: &Path) -> tokio::process::Command {
    #[cfg_attr(not(unix), allow(unused_mut))]
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

    /// RomM platform slugs (the canonical `slug`, not the renamable `fs_slug`)
    /// whose games this emulator runs. Picks the emulator when a launch or
    /// search doesn't name one.
    fn platforms(&self) -> &'static [&'static str];

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

    /// Paths relative to `config_dir`, one of which exists once the emulator
    /// has run with that folder. Empty skips the check.
    fn config_dir_markers(&self) -> &'static [&'static str] {
        &[]
    }

    /// Where `config_dir` usually is on this OS, for the warning when it
    /// looks wrong.
    fn config_dir_hint(&self) -> &'static str {
        ""
    }

    /// How `rommd detect` recognises this emulator's macOS app; `None` means
    /// it isn't detected there.
    fn mac_app(&self) -> Option<crate::detect::MacApp> {
        None
    }

    /// How `rommd detect` recognises this emulator's packages, Flatpaks and
    /// AppImages on Linux; `None` means it isn't detected there.
    fn linux_app(&self) -> Option<crate::detect::LinuxApp> {
        None
    }
}

/// Why `config_dir` does not look like this emulator's folder, if it doesn't.
/// A wrong folder means saves silently never sync, so this is logged at
/// startup and launch.
pub fn config_dir_problem(emulator: &dyn Emulator, config: &EmulatorConfig) -> Option<String> {
    let dir = &config.config_dir;
    let markers = emulator.config_dir_markers();
    let name = emulator.display_name();
    let problem = if !dir.is_dir() {
        format!("config_dir {} does not exist", dir.display())
    } else if !markers.is_empty() && !markers.iter().any(|m| dir.join(m).exists()) {
        format!(
            "config_dir {} has no {}; unless {} has never run, it is not {}'s folder",
            dir.display(),
            markers.join(" or "),
            name,
            name
        )
    } else {
        return None;
    };
    Some(match emulator.config_dir_hint() {
        "" => problem,
        hint => format!("{problem}. {name} usually keeps it in {hint}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TempDir, write};

    #[test]
    fn config_dir_problems_name_the_missing_marker() {
        let root = TempDir::new("config-dir-check");
        let config = |dir: &Path| EmulatorConfig {
            binary: "duckstation".into(),
            config_dir: dir.to_owned(),
        };
        let emu = duckstation::DuckStation;

        let missing = config_dir_problem(&emu, &config(&root.path().join("nope"))).unwrap();
        assert!(missing.contains("does not exist"), "{missing}");

        let wrong = config_dir_problem(&emu, &config(root.path())).unwrap();
        assert!(wrong.contains("has no settings.ini"), "{wrong}");
        assert!(wrong.contains("usually keeps it in"), "{wrong}");

        write(&root.path().join("settings.ini"), b"");
        assert_eq!(config_dir_problem(&emu, &config(root.path())), None);
    }
}
