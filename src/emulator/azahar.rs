use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::process::Child;

use super::{Emulator, FileSet, GameSaveDir, LaunchContext, SaveLayout, SaveSet};
use crate::config::EmulatorConfig;
use crate::ini::IniFile;
use crate::profile::ProfileBlob;

/// Azahar (Citra fork, 3DS). `config_dir` is Azahar's user dir
/// (`~/.local/share/azahar-emu` on Linux); on Linux its qt-config.ini lives in
/// `~/.config/azahar-emu/` unless the install is portable.
///
/// A game's save is two trees under the emulated SD card's
/// `Nintendo 3DS/<ID0>/<ID1>/`: `title/<high>/<low>/data/` and
/// `extdata/00000000/<low >> 8>/`. Some games keep their progress only in
/// extdata. Both sync as one zip with roots `data/` and `extdata/`, the layout
/// argosy-launcher uses. Save states are `states/<TITLE ID>.<slot>.cst`.
pub struct Azahar;

/// Citra-derived emulators share the save layout.
const SAVE_FAMILY: &[&str] = &["azahar", "citra", "citra_mmj", "lime3ds", "borked3ds"];
const ZERO_ID: &str = "00000000000000000000000000000000";

#[async_trait]
impl Emulator for Azahar {
    fn id(&self) -> &'static str {
        "azahar"
    }

    fn display_name(&self) -> &'static str {
        "Azahar"
    }

    fn game_id_for_rom(&self, _config: &EmulatorConfig, rom_path: &Path) -> Option<String> {
        crate::n3ds::title_id_for_rom(rom_path)
    }

    fn save_family(&self) -> &'static [&'static str] {
        SAVE_FAMILY
    }

    async fn save_layout(
        &self,
        config: &EmulatorConfig,
        _rom_path: &Path,
        game_id: Option<&str>,
    ) -> SaveLayout {
        let Some(id) = game_id.and_then(|g| u64::from_str_radix(g, 16).ok()) else {
            return SaveLayout::default();
        };
        let (high, low) = ((id >> 32) as u32, id as u32);
        let root = sd_root(config);
        SaveLayout {
            saves: Some(SaveSet::Dir(GameSaveDir {
                dir: root
                    .join("title")
                    .join(format!("{:08x}", high))
                    .join(format!("{:08x}", low))
                    .join("data"),
                archive_root: "data".into(),
                extra_roots: vec![(
                    "extdata".into(),
                    root.join("extdata")
                        .join("00000000")
                        .join(format!("{:08x}", low >> 8)),
                )],
                mirrors: Vec::new(),
                required_subdir: None,
            })),
            states: FileSet::stem(config.config_dir.join("states"), format!("{:016X}", id)),
        }
    }

    async fn launch(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        _game_id: Option<&str>,
        _profile: Option<&ProfileBlob>,
    ) -> Result<(Child, LaunchContext)> {
        let child = super::emulator_command(&config.binary)
            .arg(rom_path)
            .spawn()
            .with_context(|| format!("spawning Azahar from {}", config.binary.display()))?;
        Ok((child, LaunchContext::default()))
    }

    async fn snapshot(&self, _config: &EmulatorConfig, _game_id: &str) -> Result<ProfileBlob> {
        anyhow::bail!("Azahar per-game profiles are not supported yet")
    }
}

/// `<sdmc>/Nintendo 3DS/<ID0>/<ID1>`. The SD card is `config_dir/sdmc` unless
/// qt-config.ini enables custom storage. Azahar always uses all-zero IDs; an
/// SD card copied from elsewhere with a single other pair is used as found.
fn sd_root(config: &EmulatorConfig) -> PathBuf {
    let ini = read_qt_config(config);
    let custom = ini
        .as_ref()
        .and_then(|i| i.get("Data%20Storage", "use_custom_storage"))
        == Some("true");
    let sdmc = ini
        .as_ref()
        .and_then(|i| i.get("Data%20Storage", "sdmc_directory"))
        .filter(|v| custom && !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| config.config_dir.join("sdmc"));
    let base = sdmc.join("Nintendo 3DS");
    let zeros = base.join(ZERO_ID).join(ZERO_ID);
    if zeros.is_dir() {
        return zeros;
    }
    let only_dir = |dir: &Path| -> Option<PathBuf> {
        let mut dirs = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir());
        let first = dirs.next()?;
        dirs.next().is_none().then_some(first)
    };
    only_dir(&base)
        .and_then(|id0| only_dir(&id0))
        .unwrap_or(zeros)
}

/// Portable and macOS installs keep `config/qt-config.ini` inside the user
/// dir; on Linux it is in the XDG config dir.
fn read_qt_config(config: &EmulatorConfig) -> Option<IniFile> {
    let bundled = config.config_dir.join("config").join("qt-config.ini");
    let path = if bundled.is_file() {
        bundled
    } else {
        dirs::config_dir()?.join("azahar-emu").join("qt-config.ini")
    };
    std::fs::read_to_string(path)
        .ok()
        .map(|t| IniFile::parse(&t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TempDir, write};

    #[tokio::test]
    async fn layout_covers_title_data_extdata_and_states() {
        let root = TempDir::new("azahar");
        write(
            &root.path().join("config/qt-config.ini"),
            b"[Data%20Storage]\nuse_custom_storage=false\nsdmc_directory=/elsewhere/\n",
        );
        let config = EmulatorConfig {
            binary: "azahar".into(),
            config_dir: root.path().to_owned(),
        };
        let layout = Azahar
            .save_layout(&config, Path::new("psmd.cci"), Some("0004000000174400"))
            .await;

        let sd = root
            .path()
            .join("sdmc/Nintendo 3DS")
            .join(ZERO_ID)
            .join(ZERO_ID);
        let Some(SaveSet::Dir(save)) = layout.saves else {
            panic!("expected a directory save")
        };
        assert_eq!(save.dir, sd.join("title/00040000/00174400/data"));
        assert_eq!(save.archive_root, "data");
        assert_eq!(
            save.extra_roots,
            vec![("extdata".to_owned(), sd.join("extdata/00000000/00001744"))]
        );
        let states = layout.states.unwrap();
        assert_eq!(states.dir, root.path().join("states"));
        assert!(states.matcher.matches_name("0004000000174400.01.cst"));
        assert!(!states.matcher.matches_name("0004000000174401.01.cst"));
    }

    #[tokio::test]
    async fn custom_storage_moves_the_sd_card() {
        let root = TempDir::new("azahar-custom");
        let sd = root.path().join("card");
        write(
            &root.path().join("config/qt-config.ini"),
            format!(
                "[Data%20Storage]\nuse_custom_storage=true\nsdmc_directory={}/\n",
                sd.display()
            )
            .as_bytes(),
        );
        let config = EmulatorConfig {
            binary: "azahar".into(),
            config_dir: root.path().to_owned(),
        };
        let layout = Azahar
            .save_layout(&config, Path::new("x.cci"), Some("00040000000ba900"))
            .await;
        let Some(SaveSet::Dir(save)) = layout.saves else {
            panic!()
        };
        assert!(save.dir.starts_with(&sd), "{}", save.dir.display());
        assert!(save.dir.ends_with("title/00040000/000ba900/data"));
    }
}
