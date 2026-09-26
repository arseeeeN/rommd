use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::process::Child;
use tracing::{debug, warn};

use super::{Emulator, LaunchContext, SaveFilter};
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

    /// Read the Switch title ID from the NSP. Filenames are deliberately not used —
    /// they can be wrong and would silently scope saves to the wrong game.
    ///
    /// Strategy (in order):
    /// 1. Ticket (`.tik` in the NSP PFS0) — title ID is in plaintext, no keys needed.
    /// 2. NCA header decryption (requires a fully derived `header_key` in prod.keys).
    fn game_id_for_rom(&self, config: &EmulatorConfig, rom_path: &Path) -> Option<String> {
        let ext = rom_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if ext != "nsp" {
            warn!(
                "cannot read title ID from .{} files — only NSP is supported; \
                 save scoping unavailable for {}",
                ext,
                rom_path.display()
            );
            return None;
        }

        // Tier 1: ticket — no keys needed, title ID is in plaintext.
        match crate::switch::read_title_id_from_ticket(rom_path) {
            Ok(id) => return Some(format!("{:016x}", id)),
            Err(e) => debug!("ticket read failed for {} ({}), trying NCA header", rom_path.display(), e),
        }

        // Tier 2: NCA header decryption.
        let keys = prod_keys_path(config);
        if !keys.exists() {
            warn!(
                "prod.keys not found at {} — title ID unavailable for {}",
                keys.display(),
                rom_path.display()
            );
            return None;
        }
        match crate::switch::read_title_id_from_nsp(rom_path, &keys) {
            Ok(id) => Some(format!("{:016x}", id)),
            Err(e) => {
                warn!("could not read title ID from {}: {:#}", rom_path.display(), e);
                None
            }
        }
    }

    fn save_filter(
        &self,
        _config: &EmulatorConfig,
        _rom_path: &Path,
        _game_id: Option<&str>,
    ) -> SaveFilter {
        SaveFilter::default()
    }

    async fn saves_dir(&self, config: &EmulatorConfig) -> PathBuf {
        config.config_dir.join("bis").join("user").join("save")
    }

    async fn states_dir(&self, _config: &EmulatorConfig) -> Option<PathBuf> {
        None
    }

    /// Scan `bis/user/save/*/ExtraData0` for the slot whose title ID matches
    /// `game_id` and whose save type is 1 (Account). Returns None if the slot
    /// hasn't been created yet (game not launched before, or first-ever save).
    async fn game_saves_dir(&self, config: &EmulatorConfig, game_id: &str) -> Option<PathBuf> {
        let title_id = u64::from_str_radix(game_id, 16).ok()?;
        find_account_save_slot(&self.saves_dir(config).await, title_id).await
    }

    async fn launch(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        _game_id: Option<&str>,
        _profile: Option<&ProfileBlob>,
    ) -> Result<(Child, LaunchContext)> {
        let child = tokio::process::Command::new(&config.binary)
            .arg(rom_path)
            .spawn()
            .with_context(|| format!("spawning Ryujinx from {}", config.binary.display()))?;
        Ok((child, LaunchContext::default()))
    }

    async fn snapshot(&self, _config: &EmulatorConfig, _game_id: &str) -> Result<ProfileBlob> {
        anyhow::bail!("Ryujinx does not support per-game profiles")
    }
}

fn prod_keys_path(config: &EmulatorConfig) -> PathBuf {
    config.config_dir.join("system").join("prod.keys")
}

/// Walk `saves_root/*/ExtraData0`, read title ID at offset 0x00 (u64 LE) and
/// save type at offset 0x20 (u32 LE). Return the first slot dir where the title
/// ID matches and save type == 1 (Account). Type 2 is BCAT (server push data),
/// which is not player save data and should not be synced.
async fn find_account_save_slot(saves_root: &Path, title_id: u64) -> Option<PathBuf> {
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
        if data.len() < 0x24 {
            continue;
        }
        let found_id = u64::from_le_bytes(data[0x00..0x08].try_into().unwrap());
        let save_type = u32::from_le_bytes(data[0x20..0x24].try_into().unwrap());
        if found_id == title_id && save_type == 1 {
            debug!("found account save slot at {}", slot_dir.display());
            return Some(slot_dir);
        }
    }
    None
}
