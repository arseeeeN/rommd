use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::process::Child;
use tracing::{debug, info};

use super::{Emulator, GameSaveDir, LaunchContext, SaveLayout, SaveSet};
use crate::config::EmulatorConfig;
use crate::profile::ProfileBlob;

pub struct Ryujinx;

#[async_trait]
impl Emulator for Ryujinx {
    fn id(&self) -> &'static str {
        "ryujinx"
    }

    fn display_name(&self) -> &'static str {
        "Ryujinx"
    }

    fn game_id_for_rom(&self, config: &EmulatorConfig, rom_path: &Path) -> Option<String> {
        crate::switch::title_id_for_rom(rom_path, &config.config_dir.join("system").join("prod.keys"))
    }

    fn save_family(&self) -> &'static [&'static str] {
        crate::switch::SAVE_COMPATIBLE_EMULATORS
    }

    async fn save_layout(
        &self,
        config: &EmulatorConfig,
        _rom_path: &Path,
        game_id: Option<&str>,
    ) -> SaveLayout {
        let saves = match game_id {
            Some(id) => game_save_dir(config, id).await.map(SaveSet::Dir),
            None => None,
        };
        SaveLayout { saves, states: None }
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
            .with_context(|| format!("spawning Ryujinx from {}", config.binary.display()))?;
        Ok((child, LaunchContext::default()))
    }

    async fn snapshot(&self, _config: &EmulatorConfig, _game_id: &str) -> Result<ProfileBlob> {
        anyhow::bail!("Ryujinx does not support per-game profiles")
    }
}

/// Scan `bis/user/save/*/ExtraData0` for the slot whose title ID matches
/// `game_id`, preferring the Account save and falling back to the Device
/// save for games that keep one save per console. Returns None if the slot
/// hasn't been created yet — only Ryujinx can create one, since it must also
/// be registered in its `imkvdb` save index.
///
/// Inside a slot, `0/` is the committed save and `1/` the working copy of the
/// save filesystem's two-phase commit; they are not user profiles. `0/` is the
/// source of truth, and restores write both so neither holds a stale copy.
async fn game_save_dir(config: &EmulatorConfig, game_id: &str) -> Option<GameSaveDir> {
    let title_id = u64::from_str_radix(game_id, 16).ok()?;
    let saves_root = config.config_dir.join("bis").join("user").join("save");
    let Some(slot) = find_save_slot(&saves_root, title_id).await else {
        info!(
            "no Ryujinx save slot for {} yet; saves sync once Ryujinx has booted the game and created it",
            game_id
        );
        return None;
    };
    Some(GameSaveDir {
        dir: slot.join("0"),
        archive_root: game_id.to_uppercase(),
        mirrors: vec![slot.join("1")],
        required_subdir: None,
    })
}

const SAVE_TYPE_ACCOUNT: u8 = 1;
const SAVE_TYPE_DEVICE: u8 = 3;

/// Walk `saves_root/*/ExtraData0` (a SaveDataAttribute: program ID u64 at 0x00,
/// user ID at 0x08, save type u8 at 0x20) and return the slot for `title_id`,
/// Account before Device. Other types (BCAT server data, cache, temporary) are
/// not player saves and are never synced.
async fn find_save_slot(saves_root: &Path, title_id: u64) -> Option<PathBuf> {
    let mut device = None;
    let mut rd = tokio::fs::read_dir(saves_root).await.ok()?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let slot_dir = entry.path();
        if !slot_dir.is_dir() {
            continue;
        }
        let data = match tokio::fs::read(slot_dir.join("ExtraData0")).await {
            Ok(d) => d,
            Err(_) => continue,
        };
        if data.len() < 0x21 {
            continue;
        }
        let found_id = u64::from_le_bytes(data[0x00..0x08].try_into().unwrap());
        if found_id != title_id {
            continue;
        }
        match data[0x20] {
            SAVE_TYPE_ACCOUNT => {
                debug!("found account save slot at {}", slot_dir.display());
                return Some(slot_dir);
            }
            SAVE_TYPE_DEVICE => device = Some(slot_dir),
            _ => {}
        }
    }
    if let Some(ref d) = device {
        debug!("found device save slot at {}", d.display());
    }
    device
}
