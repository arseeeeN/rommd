use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::process::Child;
use tracing::warn;

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

    /// Read the Switch title ID from the NSP header via PFS0 → CNMT NCA decryption.
    /// Filenames are deliberately not used — they can be wrong and would silently
    /// scope saves to the wrong game.
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
                warn!(
                    "could not read title ID from {}: {:#}",
                    rom_path.display(),
                    e
                );
                None
            }
        }
    }

    // Ryujinx stores saves under bis/user/save/<save_data_id>/ — a runtime-assigned
    // index unrelated to the title ID. Each slot contains profile subdirectories (0/, 1/)
    // whose contents are the actual save files. Syncing requires recursive directory
    // traversal which the current SaveFilter prefix mechanism does not support.
    fn save_filter(
        &self,
        _config: &EmulatorConfig,
        _rom_path: &Path,
        _game_id: Option<&str>,
    ) -> SaveFilter {
        SaveFilter::default()
    }

    fn saves_dir(&self, config: &EmulatorConfig) -> PathBuf {
        config.config_dir.join("bis").join("user").join("save")
    }

    fn states_dir(&self, config: &EmulatorConfig) -> PathBuf {
        config.config_dir.join("bis").join("user").join("save")
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
