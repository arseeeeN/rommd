use anyhow::{Context, Result};
use async_trait::async_trait;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use tokio::process::Child;
use tracing::debug;

use super::{Emulator, FileSet, LaunchContext, SaveLayout, SaveSet, ps1};
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

    fn platforms(&self) -> &'static [&'static str] {
        &["psx"]
    }

    fn config_dir_markers(&self) -> &'static [&'static str] {
        &["settings.ini"]
    }

    fn config_dir_hint(&self) -> &'static str {
        if cfg!(windows) {
            r"%LOCALAPPDATA%\DuckStation (Documents\DuckStation if that folder existed first; the install folder when it holds portable.txt)"
        } else if cfg!(target_os = "macos") {
            "~/Library/Application Support/DuckStation"
        } else {
            "~/.local/share/duckstation ($XDG_CONFIG_HOME/duckstation when XDG_CONFIG_HOME is set)"
        }
    }

    fn mac_app(&self) -> Option<crate::detect::MacApp> {
        Some(crate::detect::MacApp {
            bundle_id: "com.github.stenzek.duckstation",
            executable: None,
            data_dir: "Library/Application Support/DuckStation",
        })
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

    /// Save states are named after the disc serial (`SLUS-00255_1.sav`). Memory
    /// cards follow the card naming mode in settings.ini (`Card1Type`, which
    /// almost all games use): by serial, by ROM file name, or by default by the
    /// game's save title (`<Title>_1.mcd`). A game's cards (`_1`, `_2`) sync
    /// as one bundle so they stay consistent and keep a version history.
    async fn save_layout(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        game_id: Option<&str>,
    ) -> SaveLayout {
        let serial = game_id.unwrap_or("").to_owned();
        let card_name = match read_setting(config, "MemoryCards", "Card1Type").as_deref() {
            Some("PerGame") => serial.clone(),
            Some("PerGameFileTitle") => rom_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            _ => per_game_title_card(config, rom_path, &serial).unwrap_or_default(),
        };
        let cards = read_folder(config, "MemoryCards", "Directory", "memcards");
        let states = read_folder(config, "Folders", "SaveStates", "savestates");
        SaveLayout {
            saves: FileSet::stem(cards, card_name).map(SaveSet::Bundle),
            states: FileSet::stem(states, serial),
        }
    }
}

/// Memory card name in DuckStation's default "per game title" mode: the
/// game's save title from DuckStation's game database (which keeps card names
/// stable, e.g. "King's Field (Japan)"), falling back to the title in the
/// gamelist cache, sanitised like DuckStation does for file names.
fn per_game_title_card(config: &EmulatorConfig, rom_path: &Path, serial: &str) -> Option<String> {
    let cache = config.config_dir.join("cache");
    let title = (!serial.is_empty())
        .then(|| lookup_save_title(&cache.join("gamedb.cache"), serial))
        .flatten()
        .or_else(|| {
            lookup_in_gamelist_cache(&cache.join("gamelist.cache"), rom_path).map(|e| e.title)
        })?;
    Some(sanitize_file_name(&title))
}

/// Find a serial's entry in DuckStation's compiled game database
/// (`cache/gamedb.cache`, magic `HLCE`). An entry starts with length-prefixed
/// (u32 LE) strings: serial, title, sort title, localized title, save title.
/// Returns the save title, or the title when no save title is set.
fn lookup_save_title(cache_path: &Path, serial: &str) -> Option<String> {
    let data = std::fs::read(cache_path).ok()?;
    if data.len() < 4 || &data[..4] != b"HLCE" {
        return None;
    }
    let mut needle = (serial.len() as u32).to_le_bytes().to_vec();
    needle.extend_from_slice(serial.as_bytes());

    let read_str = |at: usize| -> Option<(String, usize)> {
        let len = u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?) as usize;
        if len > 512 {
            return None;
        }
        let s = std::str::from_utf8(data.get(at + 4..at + 4 + len)?).ok()?;
        Some((s.to_owned(), at + 4 + len))
    };
    // The serial also appears inside other entries (e.g. disc set lists), so
    // take the first occurrence that parses as an entry with a title.
    let mut from = 0;
    while let Some(pos) = data[from..]
        .windows(needle.len())
        .position(|w| w == needle.as_slice())
    {
        let start = from + pos + needle.len();
        from = start;
        let Some((title, next)) = read_str(start) else {
            continue;
        };
        let Some((_sort, next)) = read_str(next) else {
            continue;
        };
        let Some((_localized, next)) = read_str(next) else {
            continue;
        };
        let Some((save_title, _)) = read_str(next) else {
            continue;
        };
        if title.is_empty() {
            continue;
        }
        return Some(if save_title.is_empty() {
            title
        } else {
            save_title
        });
    }
    None
}

/// DuckStation's `Path::SanitizeFileName`: characters not allowed in file
/// names become `_`.
fn sanitize_file_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect()
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
            if sl_off + 4 > data.len() {
                break;
            }
            let sl = u32::from_le_bytes(data[sl_off..sl_off + 4].try_into().ok()?) as usize;
            let ss = sl_off + 4;
            if sl == 0 || sl > 16 || ss + sl > data.len() {
                i += 1;
                continue;
            }
            let serial = std::str::from_utf8(&data[ss..ss + sl]).ok()?.to_owned();
            if !is_ps_serial(&serial) {
                i += 1;
                continue;
            }

            let tl_off = ss + sl;
            if tl_off + 4 > data.len() {
                break;
            }
            let tl = u32::from_le_bytes(data[tl_off..tl_off + 4].try_into().ok()?) as usize;
            let ts = tl_off + 4;
            if tl == 0 || tl > 256 || ts + tl > data.len() {
                i += 1;
                continue;
            }
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
    IniFile::parse(&text)
        .get(section, key)
        .map(|v| v.to_owned())
}

/// Read a directory path from DuckStation's settings.ini, falling back to `default`
/// if the file or key is absent. Relative paths are resolved against `config_dir`.
fn read_folder(config: &EmulatorConfig, section: &str, key: &str, default: &str) -> PathBuf {
    let raw = read_setting(config, section, key);

    let dir = raw.as_deref().unwrap_or(default);
    let p = std::path::Path::new(dir);
    if p.is_absolute() {
        p.to_owned()
    } else {
        config.config_dir.join(p)
    }
}

impl DuckStation {
    fn gamesettings_path(&self, config: &EmulatorConfig, serial: &str) -> PathBuf {
        config
            .config_dir
            .join("gamesettings")
            .join(format!("{}.ini", serial))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_title_from_gamedb_cache() {
        let mut data = b"HLCE!\x00\x00\x00".to_vec();
        for s in [
            "SLUS-00158",
            "King's Field",
            "King's Field 2",
            "",
            "King's Field (USA)",
            "rest",
        ] {
            data.extend_from_slice(&(s.len() as u32).to_le_bytes());
            data.extend_from_slice(s.as_bytes());
        }
        for s in ["SCUS-94900", "Crash Bandicoot", "Crash Bandicoot 1", "", ""] {
            data.extend_from_slice(&(s.len() as u32).to_le_bytes());
            data.extend_from_slice(s.as_bytes());
        }
        let path = std::env::temp_dir().join(format!("rommd-gamedb-{}.cache", std::process::id()));
        std::fs::write(&path, &data).unwrap();
        assert_eq!(
            lookup_save_title(&path, "SLUS-00158").as_deref(),
            Some("King's Field (USA)")
        );
        assert_eq!(
            lookup_save_title(&path, "SCUS-94900").as_deref(),
            Some("Crash Bandicoot")
        );
        assert_eq!(lookup_save_title(&path, "SLUS-99999"), None);
        let _ = std::fs::remove_file(&path);
        assert_eq!(sanitize_file_name("Game: Part 1/2?"), "Game_ Part 1_2_");
    }
}
