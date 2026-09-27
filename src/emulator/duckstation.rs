use anyhow::{Context, Result};
use async_trait::async_trait;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use tokio::process::Child;
use tracing::debug;

use super::{ps1, Emulator, LaunchContext, SaveFilter};
use crate::config::EmulatorConfig;
use crate::ini::IniFile;
use crate::profile::{ProfileBlob, SnapshotCleaner};

pub struct DuckStation;

#[async_trait]
impl Emulator for DuckStation {
    fn id(&self) -> &'static str {
        "duckstation"
    }

    fn display_name(&self) -> &'static str {
        "DuckStation"
    }

    async fn launch(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        game_id: Option<&str>,
        profile: Option<&ProfileBlob>,
    ) -> Result<(Child, LaunchContext)> {
        match (game_id, profile) {
            (Some(id), Some(p)) => self.apply_profile(config, id, p).await?,
            (None, Some(_)) => anyhow::bail!("game_id is required to apply a DuckStation profile"),
            _ => {}
        }

        let child = super::emulator_command(&config.binary)
            .arg(rom_path)
            .spawn()
            .with_context(|| format!("spawning DuckStation from {}", config.binary.display()))?;

        Ok((child, LaunchContext::default()))
    }

    /// `game_id` is the disc serial (e.g. "SLUS-00255").
    async fn snapshot(&self, config: &EmulatorConfig, game_id: &str) -> Result<ProfileBlob> {
        let path = self.gamesettings_path(config, game_id);

        let overlay = if path.exists() {
            let content = tokio::fs::read_to_string(&path)
                .await
                .context("reading gamesettings")?;
            IniFile::parse(&content).to_flat_map()
        } else {
            IndexMap::new()
        };

        let cleaned = SnapshotCleaner::for_emulator("duckstation").clean(overlay);
        Ok(ProfileBlob::new(cleaned))
    }

    fn game_id_for_rom(&self, config: &EmulatorConfig, rom_path: &Path) -> Option<String> {
        let serial = ps1::read_serial(rom_path).or_else(|| {
            let cache = config.config_dir.join("cache").join("gamelist.cache");
            lookup_in_gamelist_cache(&cache, rom_path).map(|e| e.serial)
        });
        if serial.is_none() {
            tracing::warn!(
                "could not determine PS1 serial for {} — \
                 SYSTEM.CNF absent and not in gamelist.cache; \
                 provide game_id manually in the launch request",
                rom_path.display()
            );
        }
        serial
    }

    fn save_filter(&self, config: &EmulatorConfig, rom_path: &Path, game_id: Option<&str>) -> SaveFilter {
        let states = game_id.unwrap_or("").to_owned();

        // The memory card naming mode is set per-card in settings.ini.
        // We look at Card1Type since that is what almost all games use.
        let card_type = read_setting(config, "MemoryCards", "Card1Type");

        let saves = match card_type.as_deref().unwrap_or("PerGameTitle") {
            "PerGame" => states.clone(),
            "PerGameFileTitle" => rom_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            _ => {
                // PerGameTitle (default): named after the game title from the cache
                let cache = config.config_dir.join("cache").join("gamelist.cache");
                lookup_in_gamelist_cache(&cache, rom_path)
                    .map(|e| e.title)
                    .unwrap_or_else(|| states.clone())
            }
        };

        SaveFilter { saves, states }
    }

    async fn saves_dir(&self, config: &EmulatorConfig) -> PathBuf {
        read_folder(config, "MemoryCards", "Directory", "memcards")
    }

    async fn states_dir(&self, config: &EmulatorConfig) -> Option<PathBuf> {
        Some(read_folder(config, "Folders", "SaveStates", "savestates"))
    }
}

struct CacheEntry {
    serial: String,
    title: String,
}

/// Search DuckStation's `gamelist.cache` for a ROM's serial and title by filename.
///
/// Entry format: `[path_len(u32)][path][serial_len(u32)][serial][title_len(u32)][title][metadata]`.
/// DuckStation sometimes writes double slashes in stored paths (`.../psx//game.m3u`) because it
/// joins a trailing-slash scan directory with the filename — so we match by filename rather than
/// the full path.
fn lookup_in_gamelist_cache(cache_path: &Path, rom_path: &Path) -> Option<CacheEntry> {
    let data = std::fs::read(cache_path).ok()?;

    // 4-byte magic "HLCE" + version (u32) + unknown (u16) = 10-byte header
    if data.len() < 10 || &data[..4] != b"HLCE" {
        return None;
    }

    let filename = rom_path.file_name()?.to_string_lossy();
    let fb = filename.as_bytes();

    let mut i = 10usize;
    while i + fb.len() <= data.len() {
        if &data[i..i + fb.len()] == fb {
            // Byte before must be a path separator — guards against a longer filename
            // that happens to end with our target string.
            if i > 0 && data[i - 1] != b'/' && data[i - 1] != b'\\' {
                i += 1;
                continue;
            }

            // [serial_len(u32)][serial_bytes][title_len(u32)][title_bytes]
            let sl_off = i + fb.len();
            if sl_off + 4 > data.len() { break; }
            let sl = u32::from_le_bytes(data[sl_off..sl_off + 4].try_into().ok()?) as usize;
            let ss = sl_off + 4;
            if sl == 0 || sl > 16 || ss + sl > data.len() { i += 1; continue; }
            let serial = std::str::from_utf8(&data[ss..ss + sl]).ok()?.to_owned();
            if !is_ps_serial(&serial) { i += 1; continue; }

            let tl_off = ss + sl;
            if tl_off + 4 > data.len() { break; }
            let tl = u32::from_le_bytes(data[tl_off..tl_off + 4].try_into().ok()?) as usize;
            let ts = tl_off + 4;
            if tl == 0 || tl > 256 || ts + tl > data.len() { i += 1; continue; }
            let title = std::str::from_utf8(&data[ts..ts + tl]).ok()?.to_owned();

            return Some(CacheEntry { serial, title });
        }
        i += 1;
    }
    None
}

/// Validate a PS1/PS2 disc serial: 4 uppercase letters + '-' + 5 digits (e.g. "SLPS-00017").
fn is_ps_serial(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[..4].iter().all(|c| c.is_ascii_uppercase())
        && b[4] == b'-'
        && b[5..].iter().all(|c| c.is_ascii_digit())
}

/// Read a single value from DuckStation's settings.ini.
fn read_setting(config: &EmulatorConfig, section: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(config.config_dir.join("settings.ini")).ok()?;
    IniFile::parse(&text).get(section, key).map(|v| v.to_owned())
}

/// Read a directory path from DuckStation's settings.ini, falling back to `default`
/// if the file or key is absent. Relative paths are resolved against `config_dir`.
fn read_folder(config: &EmulatorConfig, section: &str, key: &str, default: &str) -> PathBuf {
    let raw = read_setting(config, section, key);

    let dir = raw.as_deref().unwrap_or(default);
    let p = std::path::Path::new(dir);
    if p.is_absolute() { p.to_owned() } else { config.config_dir.join(p) }
}

impl DuckStation {
    fn gamesettings_path(&self, config: &EmulatorConfig, serial: &str) -> PathBuf {
        config.config_dir.join("gamesettings").join(format!("{}.ini", serial))
    }

    async fn apply_profile(
        &self,
        config: &EmulatorConfig,
        serial: &str,
        profile: &ProfileBlob,
    ) -> Result<()> {
        let path = self.gamesettings_path(config, serial);

        let mut ini = if path.exists() {
            let content = tokio::fs::read_to_string(&path)
                .await
                .context("reading existing gamesettings")?;
            IniFile::parse(&content)
        } else {
            IniFile::default()
        };

        ini.apply_overlay(&profile.overlay);

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("creating gamesettings directory")?;
        }
        tokio::fs::write(&path, ini.to_string())
            .await
            .context("writing gamesettings")?;

        debug!("applied profile to {}", path.display());
        Ok(())
    }
}
