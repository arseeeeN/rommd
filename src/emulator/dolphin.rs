use anyhow::{Context, Result};
use async_trait::async_trait;
use indexmap::IndexMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use tokio::process::Child;
use tracing::{debug, warn};

use super::{Emulator, FileSet, GameSaveDir, LaunchContext, SaveLayout, SaveSet};
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
        read_disc(rom_path).map(|(id, _)| id)
    }

    /// GameCube saves are the game's `.gci` files in `GC/<region>/Card A/`,
    /// synced as one bundle; Wii saves are the title's NAND directory. Save
    /// states are `StateSaves/<game id>.sNN` for both.
    async fn save_layout(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        game_id: Option<&str>,
    ) -> SaveLayout {
        let Some(id) = game_id else {
            return SaveLayout::default();
        };
        let ini = read_dolphin_ini(config);
        let saves = match read_disc(rom_path).map(|(_, kind)| kind) {
            Some(DiscKind::GameCube) => gc_saves(config, ini.as_ref(), id),
            Some(DiscKind::Wii) => Some(SaveSet::Dir(wii_save_dir(config, ini.as_ref(), id))),
            None => None,
        };
        SaveLayout {
            saves,
            states: FileSet::stem(config.config_dir.join("StateSaves"), id),
        }
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

        let child = super::emulator_command(&config.binary)
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

/// Dolphin.ini lives in `<user dir>/Config/` on macOS, Windows and portable
/// installs; on Linux, Dolphin splits the user dir along XDG lines and keeps the
/// config files in `~/.config/dolphin-emu/` while `config_dir` (the data dir,
/// `~/.local/share/dolphin-emu`) holds saves and game settings.
fn read_dolphin_ini(config: &EmulatorConfig) -> Option<IniFile> {
    let bundled = config.config_dir.join("Config").join("Dolphin.ini");
    let path = if bundled.is_file() {
        bundled
    } else {
        dirs::config_dir()?.join("dolphin-emu").join("Dolphin.ini")
    };
    std::fs::read_to_string(path)
        .ok()
        .map(|t| IniFile::parse(&t))
}

fn ini_value<'a>(ini: Option<&'a IniFile>, section: &str, key: &str) -> Option<&'a str> {
    ini?.get(section, key).filter(|v| !v.is_empty())
}

/// Slot A device values in Dolphin.ini (`EXIDeviceType`).
const EXI_MEMORY_CARD: &str = "1";
const EXI_MEMORY_CARD_FOLDER: &str = "8";
const GC_REGIONS: &[&str] = &["USA", "EUR", "JAP"];

/// The game's `.gci` files, when slot A is a GCI folder (Dolphin's default).
/// A raw memory card holds every game's saves in one image and cannot be
/// synced per game.
fn gc_saves(config: &EmulatorConfig, ini: Option<&IniFile>, game_id: &str) -> Option<SaveSet> {
    match ini_value(ini, "Core", "SlotA") {
        None | Some(EXI_MEMORY_CARD_FOLDER) => {}
        Some(EXI_MEMORY_CARD) => {
            warn!(
                "Dolphin slot A is a raw memory card shared by all games; \
                 GameCube saves for {} cannot be synced — switch slot A to a GCI folder",
                game_id
            );
            return None;
        }
        Some(other) => {
            debug!(
                "Dolphin slot A holds device {} rather than a memory card",
                other
            );
            return None;
        }
    }

    // Newer Dolphin versions call the override GCIFolderAPath.
    let dir = match ini_value(ini, "Core", "GCIFolderAPath")
        .or_else(|| ini_value(ini, "Core", "GCIFolderAPathOverride"))
    {
        Some(path) => PathBuf::from(path),
        None => {
            // A card that already holds this game's saves wins over the region
            // guess, e.g. a PAL game the user runs with a USA-region card.
            let gc = config.config_dir.join("GC");
            GC_REGIONS
                .iter()
                .map(|region| gc.join(region).join("Card A"))
                .find(|dir| !FileSet::gci(dir.clone(), game_id).files().is_empty())
                .unwrap_or_else(|| gc.join(gc_region_dir(game_id)).join("Card A"))
        }
    };
    Some(SaveSet::Bundle(FileSet::gci(dir, game_id)))
}

/// Region folder Dolphin uses for a game, from the region code in the 4th
/// character of its ID. GameCube has no Korean region.
fn gc_region_dir(game_id: &str) -> &'static str {
    match game_id.as_bytes().get(3) {
        Some(b'E') => "USA",
        Some(b'J') => "JAP",
        _ => "EUR",
    }
}

/// `<NAND root>/title/00010000/<game code as hex>/` — the whole title folder,
/// the same unit argosy-launcher archives, rooted at its folder name.
fn wii_save_dir(config: &EmulatorConfig, ini: Option<&IniFile>, game_id: &str) -> GameSaveDir {
    let nand = ini_value(ini, "General", "NANDRootPath")
        .map(PathBuf::from)
        .unwrap_or_else(|| config.config_dir.join("Wii"));
    let code: String = game_id
        .bytes()
        .take(4)
        .map(|b| format!("{:02x}", b))
        .collect();
    GameSaveDir {
        dir: nand.join("title").join("00010000").join(&code),
        archive_root: code,
        extra_roots: Vec::new(),
        mirrors: Vec::new(),
        required_subdir: Some("data"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscKind {
    GameCube,
    Wii,
}

const WII_MAGIC: u32 = 0x5D1C_9EA3;
const GC_MAGIC: u32 = 0xC233_9F3D;

/// Game ID and platform of a disc image. The disc header carries a magic word
/// for each platform (Wii at 0x18, GameCube at 0x1C); WBFS images are Wii-only.
fn read_disc(rom_path: &Path) -> Option<(String, DiscKind)> {
    let id = read_game_id(rom_path)?;
    let ext = rom_path.extension()?.to_str()?.to_lowercase();
    let header = match ext.as_str() {
        "iso" | "gcm" => 0,
        "rvz" => 0x58,
        "wbfs" => return Some((id, DiscKind::Wii)),
        _ => return None,
    };
    let magic_at = |offset: u64| -> Option<u32> {
        let mut f = std::fs::File::open(rom_path).ok()?;
        let mut buf = [0u8; 4];
        f.seek(SeekFrom::Start(header + offset)).ok()?;
        f.read_exact(&mut buf).ok()?;
        Some(u32::from_be_bytes(buf))
    };
    if magic_at(0x18) == Some(WII_MAGIC) {
        Some((id, DiscKind::Wii))
    } else if magic_at(0x1C) == Some(GC_MAGIC) {
        Some((id, DiscKind::GameCube))
    } else {
        warn!(
            "{} has neither a GameCube nor a Wii disc header",
            rom_path.display()
        );
        None
    }
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
        b'A' | b'D'
            | b'E'
            | b'F'
            | b'H'
            | b'I'
            | b'J'
            | b'K'
            | b'L'
            | b'M'
            | b'N'
            | b'P'
            | b'Q'
            | b'R'
            | b'S'
            | b'T'
            | b'U'
            | b'W'
            | b'X'
            | b'Z'
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
        assert!(parse_game_id(b"GAL501").is_none()); // '5' at position 3 is not a region code
        assert!(parse_game_id(b"gale01").is_none()); // lowercase
        assert!(parse_game_id(b"GAL 01").is_none()); // space
    }

    #[test]
    fn gc_region_from_game_id() {
        assert_eq!(gc_region_dir("GALE01"), "USA");
        assert_eq!(gc_region_dir("GALJ01"), "JAP");
        assert_eq!(gc_region_dir("GALP01"), "EUR");
        assert_eq!(gc_region_dir("GALD01"), "EUR");
    }

    #[test]
    fn wii_title_dir_is_hex_game_code() {
        let config = EmulatorConfig {
            binary: "dolphin".into(),
            config_dir: "/d".into(),
        };
        let dir = wii_save_dir(&config, None, "RMGE01");
        assert_eq!(dir.dir, PathBuf::from("/d/Wii/title/00010000/524d4745"));
        assert_eq!(dir.archive_root, "524d4745");
    }

    #[test]
    fn slot_a_modes() {
        let config = EmulatorConfig {
            binary: "dolphin".into(),
            config_dir: "/d".into(),
        };
        let ini = |v: &str| IniFile::parse(&format!("[Core]\nSlotA = {}\n", v));
        assert!(matches!(
            gc_saves(&config, None, "GALE01"),
            Some(SaveSet::Bundle(_))
        ));
        assert!(matches!(
            gc_saves(&config, Some(&ini("8")), "GALE01"),
            Some(SaveSet::Bundle(_))
        ));
        assert!(gc_saves(&config, Some(&ini("1")), "GALE01").is_none());
        assert!(gc_saves(&config, Some(&ini("255")), "GALE01").is_none());
        let with_override = IniFile::parse("[Core]\nGCIFolderAPath = /cards/a\n");
        match gc_saves(&config, Some(&with_override), "GALE01") {
            Some(SaveSet::Bundle(set)) => assert_eq!(set.dir, PathBuf::from("/cards/a")),
            other => panic!("unexpected {:?}", other),
        }
    }

    fn disc(dir: &Path, name: &str, id: &[u8; 6], magic_offset: usize, magic: u32) -> PathBuf {
        let mut header = vec![0u8; 0x40];
        header[..6].copy_from_slice(id);
        header[magic_offset..magic_offset + 4].copy_from_slice(&magic.to_be_bytes());
        let path = dir.join(name);
        crate::testing::write(&path, &header);
        path
    }

    #[test]
    fn disc_header_tells_gamecube_from_wii() {
        let dir = crate::testing::TempDir::new("discs");
        let gc = disc(dir.path(), "melee.iso", b"GALE01", 0x1C, GC_MAGIC);
        let wii = disc(dir.path(), "galaxy.iso", b"RMGE01", 0x18, WII_MAGIC);
        let neither = disc(dir.path(), "junk.iso", b"GALE01", 0x1C, 0);
        assert_eq!(read_disc(&gc), Some(("GALE01".into(), DiscKind::GameCube)));
        assert_eq!(read_disc(&wii), Some(("RMGE01".into(), DiscKind::Wii)));
        assert_eq!(read_disc(&neither), None);
    }

    #[test]
    fn a_card_holding_the_games_saves_wins_over_the_region_guess() {
        let root = crate::testing::TempDir::new("gc-card");
        let config = EmulatorConfig {
            binary: "dolphin".into(),
            config_dir: root.path().to_owned(),
        };
        let dir_for = |config: &EmulatorConfig| match gc_saves(config, None, "GALP01") {
            Some(SaveSet::Bundle(set)) => set.dir,
            other => panic!("unexpected {:?}", other),
        };
        assert_eq!(dir_for(&config), root.path().join("GC/EUR/Card A"));
        crate::testing::write(
            &root.path().join("GC/USA/Card A/01-GALE-save.gci"),
            b"GALP01 pal save on a usa card",
        );
        assert_eq!(dir_for(&config), root.path().join("GC/USA/Card A"));
    }
}
