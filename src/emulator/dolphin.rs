use anyhow::{Context, Result};
use async_trait::async_trait;
use indexmap::IndexMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use tokio::process::Child;
use tracing::{debug, warn};

use super::{Emulator, LaunchContext, SaveFilter};
use crate::config::EmulatorConfig;
use crate::ini::IniFile;
use crate::profile::ProfileBlob;

pub struct Dolphin;

#[async_trait]
impl Emulator for Dolphin {
    fn id(&self) -> &'static str {
        "dolphin"
    }

    fn display_name(&self) -> &'static str {
        "Dolphin"
    }

    fn game_id_for_rom(&self, _config: &EmulatorConfig, rom_path: &Path) -> Option<String> {
        read_game_id(rom_path)
    }

    fn save_filter(
        &self,
        config: &EmulatorConfig,
        _rom_path: &Path,
        game_id: Option<&str>,
    ) -> SaveFilter {
        let states = game_id.unwrap_or("").to_owned();
        // GCI folder mode (Dolphin's default) scopes saves to the game ID prefix.
        // A shared raw memory card can't be scoped to one game — warn and sync everything.
        let saves = if is_shared_memcard(config) {
            if !states.is_empty() {
                warn!(
                    "Dolphin slot A is a shared memory card — \
                     GC saves cannot be scoped to {}; all card files will be synced",
                    states
                );
            }
            String::new()
        } else {
            states.clone()
        };
        SaveFilter { saves, states }
    }

    async fn launch(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        game_id: Option<&str>,
        profile: Option<&ProfileBlob>,
    ) -> Result<(Child, LaunchContext)> {
        match (game_id, profile) {
            (Some(id), Some(p)) => apply_game_settings(config, id, p).await?,
            (None, Some(_)) => warn!(
                "skipping Dolphin profile — game ID could not be determined for {}",
                rom_path.display()
            ),
            _ => {}
        }

        let child = tokio::process::Command::new(&config.binary)
            .arg("-e")
            .arg(rom_path)
            .spawn()
            .with_context(|| format!("spawning Dolphin from {}", config.binary.display()))?;

        Ok((child, LaunchContext::default()))
    }

    async fn snapshot(&self, config: &EmulatorConfig, game_id: &str) -> Result<ProfileBlob> {
        let path = game_settings_path(config, game_id);
        let overlay = if path.exists() {
            let text = tokio::fs::read_to_string(&path)
                .await
                .context("reading Dolphin game settings")?;
            IniFile::parse(&text).to_flat_map()
        } else {
            IndexMap::new()
        };
        Ok(ProfileBlob::new(overlay))
    }

    // GC memory card saves live under GC/<REGION>/Card A/*.gci (GCI folder mode).
    // Wii saves are in the Wii/ NAND tree which is too complex to scope by game ID —
    // left for a future pass.
    fn saves_dir(&self, config: &EmulatorConfig) -> PathBuf {
        config.config_dir.join("GC")
    }

    fn states_dir(&self, config: &EmulatorConfig) -> PathBuf {
        config.config_dir.join("StateSaves")
    }
}

fn game_settings_path(config: &EmulatorConfig, game_id: &str) -> PathBuf {
    config
        .config_dir
        .join("GameSettings")
        .join(format!("{}.ini", game_id))
}

async fn apply_game_settings(
    config: &EmulatorConfig,
    game_id: &str,
    profile: &ProfileBlob,
) -> Result<()> {
    let path = game_settings_path(config, game_id);

    let mut ini = if path.exists() {
        let text = tokio::fs::read_to_string(&path)
            .await
            .context("reading existing game settings")?;
        IniFile::parse(&text)
    } else {
        IniFile::default()
    };

    ini.apply_overlay(&profile.overlay);

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("creating GameSettings directory")?;
    }
    tokio::fs::write(&path, ini.to_string())
        .await
        .context("writing game settings")?;

    debug!("applied profile to {}", path.display());
    Ok(())
}

/// Returns true only if slot A is explicitly configured as a shared raw memory card.
/// Dolphin's default (no SlotA key) is GCI folder mode, so we treat absent = GCI.
fn is_shared_memcard(config: &EmulatorConfig) -> bool {
    let path = config.config_dir.join("Config").join("Dolphin.ini");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let ini = IniFile::parse(&text);
    ini.get("Core", "SlotA")
        .map_or(false, |v| v.contains("Memory Card"))
}

/// Read the 6-character game ID from a disc image.
///
/// Format notes:
/// - ISO/GCM: raw disc image, game ID at offset 0x00.
/// - RVZ: Dolphin's compressed format. The fixed v1 file header is 0x58 bytes,
///   followed immediately by the uncompressed disc header (game ID at disc offset 0x00,
///   so RVZ file offset 0x58). Verified empirically against multiple RVZ files.
/// - WBFS: scans common disc-header offsets within the container.
fn read_game_id(rom_path: &Path) -> Option<String> {
    let ext = rom_path.extension()?.to_str()?.to_lowercase();
    match ext.as_str() {
        "iso" | "gcm" => read_id_at(rom_path, 0),
        "rvz" => read_id_at(rom_path, 0x58),
        "wbfs" => read_id_wbfs(rom_path),
        _ => {
            warn!(
                "cannot read game ID from .{} — only ISO/GCM/RVZ/WBFS supported; \
                 save scoping and profiles will be unavailable for {}",
                ext,
                rom_path.display()
            );
            None
        }
    }
}

/// Read 6 bytes at `offset` and validate them as a game ID.
fn read_id_at(rom_path: &Path, offset: u64) -> Option<String> {
    let mut f = std::fs::File::open(rom_path).ok()?;
    let mut buf = [0u8; 6];
    f.seek(SeekFrom::Start(offset)).ok()?;
    f.read_exact(&mut buf).ok()?;
    parse_game_id(&buf)
}

/// WBFS files embed the disc header at an offset that depends on the WBFS geometry.
/// Rather than fully parsing the WBFS container we try the common offsets.
fn read_id_wbfs(rom_path: &Path) -> Option<String> {
    // Common locations found empirically for single-game .wbfs files.
    const OFFSETS: &[u64] = &[0x60, 0x80, 0x100, 0x200];
    for &offset in OFFSETS {
        if let Some(id) = read_id_at(rom_path, offset) {
            return Some(id);
        }
    }
    warn!(
        "could not find game ID in WBFS file {} — \
         consider converting to ISO for reliable ID detection",
        rom_path.display()
    );
    None
}

/// Validate 6 bytes as a GameCube/Wii game ID.
/// All bytes must be ASCII alphanumeric; byte 3 must be a known region code.
fn parse_game_id(b: &[u8]) -> Option<String> {
    if b.len() < 6 {
        return None;
    }
    if !b[..6].iter().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    // Region codes at position 3. The list covers all documented codes.
    let region_ok = matches!(
        b[3],
        b'A' | b'D' | b'E' | b'F' | b'H' | b'I' | b'J' | b'K'
            | b'L' | b'M' | b'N' | b'P' | b'Q' | b'R' | b'S'
            | b'T' | b'U' | b'W' | b'X' | b'Z'
    );
    if !region_ok {
        return None;
    }
    std::str::from_utf8(&b[..6]).ok().map(|s| s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_ids() {
        assert!(parse_game_id(b"GMSE01").is_some()); // Super Mario Sunshine (USA)
        assert!(parse_game_id(b"GALE01").is_some()); // Melee (USA)
        assert!(parse_game_id(b"RM8P01").is_some()); // Mario Party 8 (PAL)
        assert!(parse_game_id(b"RMCE01").is_some()); // Mario Galaxy (USA)
    }

    #[test]
    fn parse_rejects_invalid() {
        assert!(parse_game_id(b"WBFS\x00\x00").is_none()); // WBFS magic — non-alphanumeric bytes
        assert!(parse_game_id(b"GAL501").is_none());        // '5' at position 3 is not a region code
        assert!(parse_game_id(b"gale01").is_none());        // lowercase
        assert!(parse_game_id(b"GAL 01").is_none());        // space
    }
}
