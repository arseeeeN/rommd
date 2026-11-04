// SPDX-License-Identifier: MPL-2.0
use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

use super::{Emulator, FileSet, LaunchCommand, LaunchContext, N64Files, SaveLayout, SaveSet};
use crate::config::EmulatorConfig;
use crate::ini::IniFile;
use crate::n64;
use crate::profile::ProfileBlob;

/// RMG (Rosalie's Mupen GUI, Nintendo 64), a frontend for mupen64plus.
/// Paths and file names are from v0.9.0: `Source/RMG-Core/Directories.cpp`
/// and `Settings.cpp`, and its mupen64plus-core's `get_save_filename`
/// (`src/main/main.c`) and `open_rom` (`src/main/rom.c`).
///
/// `config_dir` is the folder holding `mupen64plus.cfg`: `$XDG_CONFIG_HOME/RMG`,
/// on Windows `%APPDATA%\RMG\Config`, or `Config` next to the program when it
/// runs portable. The save and state folders are `[Core] SaveSRAMPath` and
/// `SaveStatePath` there, which RMG writes on its first start; relative ones
/// (portable) are in the program's folder.
///
/// A game's files are named `<GoodName, 32 bytes>-<first 8 of the ROM's MD5>`,
/// the GoodName from mupen64plus's ROM database. rommd finds them by the MD5
/// part, and spells the name for a save RMG hasn't written yet from the
/// database installed with RMG, where it can read it (not inside an AppImage).
pub struct Rmg;

const SAVE_EXTENSIONS: &[&str] = &["eep", "sra", "fla", "mpk"];
const STATE_EXTENSIONS: &[&str] = &[
    "st0", "st1", "st2", "st3", "st4", "st5", "st6", "st7", "st8", "st9",
];

#[async_trait]
impl Emulator for Rmg {
    fn id(&self) -> &'static str {
        "rmg"
    }

    fn display_name(&self) -> &'static str {
        "RMG"
    }

    fn platforms(&self) -> &'static [&'static str] {
        &["n64"]
    }

    fn save_family(&self) -> &'static [&'static str] {
        n64::SAVE_COMPATIBLE_EMULATORS
    }

    fn config_dir_markers(&self) -> &'static [&'static str] {
        &["mupen64plus.cfg"]
    }

    fn config_dir_hint(&self) -> &'static str {
        if cfg!(windows) {
            r"%APPDATA%\RMG\Config (Config next to the program when portable)"
        } else {
            "~/.config/RMG (Flatpak: ~/.var/app/com.github.Rosalie241.RMG/config/RMG)"
        }
    }

    /// `Package/com.github.Rosalie241.RMG.desktop` names the Flatpak and is
    /// the AppImage's root desktop file (`Package/AppImage/Create.sh`).
    fn linux_app(&self) -> Option<crate::detect::LinuxApp> {
        Some(crate::detect::LinuxApp {
            app_ids: &["com.github.Rosalie241.RMG"],
            executables: &["RMG"],
            update_info: &[],
            data_dir: crate::detect::XdgDir::Config("RMG"),
        })
    }

    /// `ProductName` from `Source/RMG/UserInterface/UIResources.rc`. Windows
    /// builds always look for portable data first (`CoreGetPortableDirectoryMode`).
    fn windows_app(&self) -> Option<crate::detect::WindowsApp> {
        use crate::detect::{AppData, Mark, Portable, WindowsApp, WindowsDataDir};
        Some(WindowsApp {
            executables: &["RMG.exe"],
            mark: Mark::Product("Rosalie's Mupen GUI"),
            data_dir: WindowsDataDir {
                portable: Portable::Folder("Config"),
                registry: None,
                documents: None,
                app_data: AppData::Roaming(r"RMG\Config"),
            },
        })
    }

    /// `--quit-after-emulation` closes RMG with the game, so rommd sees the
    /// session end; `--` ends the options.
    async fn command(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        _game_id: Option<&str>,
        _profile: Option<&ProfileBlob>,
    ) -> Result<(LaunchCommand, LaunchContext)> {
        let mut command = LaunchCommand::new(&config.binary);
        command
            .arg("--quit-after-emulation")
            .arg("--")
            .arg(rom_path);
        Ok((command, LaunchContext::default()))
    }

    /// There is no windowed option; without `--fullscreen` its setting applies.
    fn fullscreen_args(&self, fullscreen: bool) -> &'static [&'static str] {
        if fullscreen { &["--fullscreen"] } else { &[] }
    }

    async fn snapshot(&self, _config: &EmulatorConfig, _game_id: &str) -> Result<ProfileBlob> {
        anyhow::bail!("RMG per-game profiles are not supported yet")
    }

    /// The MD5 of the ROM in big-endian order, as mupen64plus computes it.
    fn game_id_for_rom(&self, _config: &EmulatorConfig, rom_path: &Path) -> Option<String> {
        let id = n64::rom_digest::<md5::Md5>(rom_path);
        if id.is_none() {
            tracing::warn!("{} is not an N64 ROM RMG can read", rom_path.display());
        }
        id
    }

    /// `<name>-<MD5 prefix>.<ext>` in the save folder and `.st<N>` in the
    /// state folder. With `SaveFilenameFormat = 0` mupen64plus names files
    /// after the header alone; those aren't synced.
    async fn save_layout(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        game_id: Option<&str>,
    ) -> SaveLayout {
        let Some(md5) = game_id.filter(|id| id.len() == 32) else {
            return SaveLayout::default();
        };
        let Ok(text) = std::fs::read_to_string(config.config_dir.join("mupen64plus.cfg")) else {
            return SaveLayout::default();
        };
        let cfg = IniFile::parse(&text);
        if cfg.get("Core", "SaveFilenameFormat") == Some("0") {
            tracing::warn!(
                "RMG names saves after the ROM header alone (SaveFilenameFormat = 0); \
                 they are not synced"
            );
            return SaveLayout::default();
        }
        let folder = |key: &str| {
            let value = cfg.get("Core", key)?.trim_matches('"');
            let program_dir = config.config_dir.parent().unwrap_or(&config.config_dir);
            (!value.is_empty()).then(|| program_dir.join(value))
        };
        let tag = format!("-{}", &md5[..8]);
        let saves = folder("SaveSRAMPath")
            .and_then(|dir| FileSet::tagged(dir, &tag, SAVE_EXTENSIONS))
            .map(|set| {
                SaveSet::N64(N64Files {
                    set,
                    new_stem: save_name(&config.binary, rom_path, md5),
                    word_swapped: true,
                })
            });
        SaveLayout {
            saves,
            states: folder("SaveStatePath")
                .and_then(|dir| FileSet::tagged(dir, &tag, STATE_EXTENSIONS)),
        }
    }
}

/// mupen64plus's name for the game's files (`get_save_filename`, format 1):
/// the database's GoodName cut to 32 bytes, else the header name, else
/// `unknown`, then `-` and the MD5's first 8 digits, with `:<>"/\|?*`
/// replaced by `_`. `None` without the database.
fn save_name(binary: &Path, rom: &Path, md5: &str) -> Option<String> {
    let header = n64::read_header(rom)?;
    let db = std::fs::read_to_string(database(binary)?).ok()?;
    let crc = |at: usize| u32::from_be_bytes(header[at..at + 4].try_into().unwrap());
    let crcs = format!("{:08X} {:08X}", crc(0x10), crc(0x14));
    let name = match good_name(&IniFile::parse(&db), md5, &crcs) {
        Some(good) => {
            let bytes = good.as_bytes();
            String::from_utf8_lossy(&bytes[..bytes.len().min(32)]).into_owned()
        }
        None => {
            let raw = n64::header_name(&header);
            let raw = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
            let trimmed = String::from_utf8_lossy(raw)
                .trim_matches(|c: char| c.is_ascii_whitespace() || c == '\x0B')
                .to_owned();
            if trimmed.is_empty() {
                "unknown".to_owned()
            } else {
                trimmed
            }
        }
    };
    let name = format!("{name}-{}", &md5[..8]);
    Some(name.replace([':', '<', '>', '"', '/', '\\', '|', '?', '*'], "_"))
}

/// The ROM's GoodName in mupen64plus's database: by MD5, else by CRCs when
/// only one entry has them. An entry with `RefMD5` takes what it lacks from
/// that entry (`romdatabase_resolve_round`).
fn good_name(db: &IniFile, md5: &str, crcs: &str) -> Option<String> {
    let field = |section: &str, key: &str| -> Option<String> {
        let entry = db
            .sections
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(section))?
            .1;
        entry.get(key).cloned().or_else(|| {
            let reference = entry.get("RefMD5")?;
            db.sections
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(reference))?
                .1
                .get(key)
                .cloned()
        })
    };
    if db
        .sections
        .keys()
        .any(|name| name.eq_ignore_ascii_case(md5))
    {
        return field(md5, "GoodName");
    }
    let mut matches = db
        .sections
        .keys()
        .filter(|name| field(name, "CRC").is_some_and(|c| c.eq_ignore_ascii_case(crcs)));
    match (matches.next(), matches.next()) {
        (Some(only), None) => field(only, "GoodName"),
        _ => None,
    }
}

/// `mupen64plus.ini` as installed with RMG: `Data/` next to the program
/// (Windows), `share/RMG/` beside its `bin/` (packages), or inside the
/// Flatpak, whose launcher is `<installation>/exports/bin/<id>`.
fn database(binary: &Path) -> Option<PathBuf> {
    let dir = binary.parent()?;
    let mut candidates = vec![
        dir.join("Data").join("mupen64plus.ini"),
        dir.join("../share/RMG/mupen64plus.ini"),
    ];
    if dir.ends_with("exports/bin") {
        candidates.push(
            dir.parent()?
                .parent()?
                .join("app")
                .join(binary.file_name()?)
                .join("current/active/files/share/RMG/mupen64plus.ini"),
        );
    }
    candidates.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TempDir, write};

    const DB: &str = "; Mupen64Plus Rom Catalog\n\n\
        [A56B15FAF24389DD375E7AFB03A7D3C1]\n\
        GoodName=007 - The World is Not Enough (E) (M3) [!]\n\
        CRC=3B941695 F90A5EEB\n\n\
        [551324F5FD0A2B0BC2B1BFBB0FE29C08]\n\
        CRC=D8C74049 29CDD8C4\n\
        RefMD5=A56B15FAF24389DD375E7AFB03A7D3C1\n\n\
        [11111111111111111111111111111111]\n\
        GoodName=Twin A\n\
        CRC=00000001 00000002\n\n\
        [22222222222222222222222222222222]\n\
        GoodName=Twin B\n\
        CRC=00000001 00000002\n";

    #[test]
    fn good_name_by_md5_reference_and_unique_crc() {
        let db = IniFile::parse(DB);
        let ok = Some("007 - The World is Not Enough (E) (M3) [!]".to_owned());
        assert_eq!(good_name(&db, "a56b15faf24389dd375e7afb03a7d3c1", ""), ok);
        assert_eq!(good_name(&db, "551324F5FD0A2B0BC2B1BFBB0FE29C08", ""), ok);
        assert_eq!(
            good_name(&db, "0".repeat(32).as_str(), "3B941695 F90A5EEB"),
            ok
        );
        assert_eq!(
            good_name(&db, "0".repeat(32).as_str(), "00000001 00000002"),
            None
        );
        assert_eq!(
            good_name(&db, "0".repeat(32).as_str(), "DEADBEEF DEADBEEF"),
            None
        );
    }

    fn rom(name: &[u8], crcs: [u32; 2]) -> Vec<u8> {
        let mut rom = vec![0u8; 0x1000];
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        rom[0x10..0x14].copy_from_slice(&crcs[0].to_be_bytes());
        rom[0x14..0x18].copy_from_slice(&crcs[1].to_be_bytes());
        rom[0x20..0x34].fill(b' ');
        rom[0x20..0x20 + name.len()].copy_from_slice(name);
        rom
    }

    #[test]
    fn save_names_follow_mupen64plus() {
        let root = TempDir::new("rmg-names");
        let binary = root.path().join("usr/bin/RMG");
        write(&binary, b"");
        let md5 = "A56B15FAF24389DD375E7AFB03A7D3C1";
        let known = root.path().join("known.z64");
        write(&known, &rom(b"007 TWINE", [0x3B941695, 0xF90A5EEB]));
        let unknown = root.path().join("unknown.z64");
        write(&unknown, &rom(b"HOMEBREW: A/B", [1, 1]));
        let nameless = root.path().join("nameless.z64");
        write(&nameless, &rom(b"", [1, 1]));

        assert_eq!(save_name(&binary, &known, md5), None, "no database");
        write(
            &root.path().join("usr/share/RMG/mupen64plus.ini"),
            DB.as_bytes(),
        );
        let other = "0".repeat(32);
        assert_eq!(
            save_name(&binary, &known, &other).as_deref(),
            Some("007 - The World is Not Enough (E-00000000")
        );
        assert_eq!(
            save_name(&binary, &unknown, &other).as_deref(),
            Some("HOMEBREW_ A_B-00000000")
        );
        assert_eq!(
            save_name(&binary, &nameless, &other).as_deref(),
            Some("unknown-00000000")
        );
    }

    #[test]
    fn finds_the_database_inside_a_flatpak() {
        let root = TempDir::new("rmg-flatpak");
        let installation = root.path().join("flatpak");
        let launcher = installation.join("exports/bin/com.github.Rosalie241.RMG");
        let db = installation
            .join("app/com.github.Rosalie241.RMG/current/active/files/share/RMG/mupen64plus.ini");
        write(&db, DB.as_bytes());
        assert_eq!(database(&launcher), Some(db));
    }

    #[tokio::test]
    async fn layout_follows_the_configured_folders() {
        let root = TempDir::new("rmg-layout");
        let program = root.path().join("RMG");
        let config_dir = program.join("Config");
        write(
            &config_dir.join("mupen64plus.cfg"),
            b"[Core]\n# Path to directory where SRAM/EEPROM data are saved\n\
              SaveSRAMPath = \"Save/Game\"\nSaveStatePath = \"Save/State\"\n",
        );
        let rom_path = root.path().join("game.z64");
        write(&rom_path, &rom(b"GAME", [1, 1]));
        let config = EmulatorConfig {
            binary: program.join("RMG.exe"),
            config_dir,
        };
        let md5 = Rmg.game_id_for_rom(&config, &rom_path).unwrap();
        let saves = program.join("Save/Game");
        let states = program.join("Save/State");
        let tag = &md5[..8];
        write(&saves.join(format!("Some Good Name-{tag}.sra")), b"x");
        write(&saves.join(format!("Some Good Name-{tag}.st0")), b"x");
        write(&saves.join("Other-00000000.sra"), b"x");
        write(&states.join(format!("Some Good Name-{tag}.st1")), b"x");

        let layout = Rmg.save_layout(&config, &rom_path, Some(&md5)).await;
        let Some(SaveSet::N64(files)) = layout.saves else {
            panic!("expected N64 saves")
        };
        assert_eq!(
            files.set.files(),
            vec![saves.join(format!("Some Good Name-{tag}.sra"))]
        );
        assert!(files.word_swapped);
        assert_eq!(files.new_stem, None, "no database next to the program");
        assert_eq!(
            layout.states.unwrap().files(),
            vec![states.join(format!("Some Good Name-{tag}.st1"))]
        );
    }

    #[tokio::test]
    async fn command_quits_with_the_game() {
        let config = EmulatorConfig {
            binary: "/usr/bin/RMG".into(),
            config_dir: "/cfg".into(),
        };
        let (mut command, _) = Rmg
            .command(&config, Path::new("/roms/game.z64"), None, None)
            .await
            .unwrap();
        command.set_fullscreen(&Rmg, true);
        assert_eq!(
            command.args,
            [
                "--fullscreen",
                "--quit-after-emulation",
                "--",
                "/roms/game.z64"
            ]
        );
    }
}
