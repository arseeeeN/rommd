// SPDX-License-Identifier: MPL-2.0
use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

use super::{
    Emulator, FileSet, Hook, LaunchCommand, LaunchContext, Ps2Cards, SaveLayout, SaveSet, ps_disc,
};
use crate::config::EmulatorConfig;
use crate::ini::IniFile;
use crate::profile::ProfileBlob;

/// PCSX2 (PlayStation 2). Data folder rules: `EmuFolders::SetDataDirectory`
/// in `pcsx2/Pcsx2Config.cpp`; paths and file names below are from v2.8.2.
///
/// Saves are the game's folders on the memory cards every game shares
/// (`Mcd001.ps2`, `Mcd002.ps2`), synced as one card of them (`ps2_card`).
/// Folder cards (a slot whose card is a directory) are not synced yet.
pub struct Pcsx2;

#[async_trait]
impl Emulator for Pcsx2 {
    fn id(&self) -> &'static str {
        "pcsx2"
    }

    fn display_name(&self) -> &'static str {
        "PCSX2"
    }

    fn platforms(&self) -> &'static [&'static str] {
        &["ps2"]
    }

    fn config_dir_markers(&self) -> &'static [&'static str] {
        &["inis/PCSX2.ini"]
    }

    /// PCSX2's forks as argosy-launcher names them; all keep its cards
    /// (Sigil's `pcsx2_standalone` layout).
    fn save_family(&self) -> &'static [&'static str] {
        &["aethersx2", "nethersx2", "armsx2", "armsx2_refresh"]
    }

    fn config_dir_hint(&self) -> &'static str {
        if cfg!(windows) {
            r"Documents\PCSX2 (the program's folder when portable.ini or portable.txt is there)"
        } else if cfg!(target_os = "macos") {
            "~/Library/Application Support/PCSX2"
        } else {
            "~/.config/PCSX2 (Flatpak: ~/.var/app/net.pcsx2.PCSX2/config/PCSX2)"
        }
    }

    fn mac_app(&self) -> Option<crate::detect::MacApp> {
        Some(crate::detect::MacApp {
            bundle_id: "net.pcsx2.pcsx2",
            executable: None,
            data_dir: "Library/Application Support/PCSX2",
        })
    }

    /// The Flatpak and the AppImage (whose root desktop file is
    /// `net.pcsx2.PCSX2.desktop`, `appimage-qt.sh`) share the app ID.
    fn linux_app(&self) -> Option<crate::detect::LinuxApp> {
        Some(crate::detect::LinuxApp {
            app_ids: &["net.pcsx2.PCSX2"],
            executables: &["pcsx2-qt"],
            update_info: &[],
            data_dir: crate::detect::XdgDir::Config("PCSX2"),
        })
    }

    /// `ProductName` is "PCSX2" (`pcsx2/windows/PCSX2.rc`). PCSX2 never uses
    /// AppData: its data is in Documents unless it runs portable.
    fn windows_app(&self) -> Option<crate::detect::WindowsApp> {
        use crate::detect::{AppData, Mark, Portable, WindowsApp, WindowsDataDir};
        Some(WindowsApp {
            executables: &["pcsx2*.exe"],
            mark: Mark::Product("PCSX2"),
            data_dir: WindowsDataDir {
                portable: Portable::Here(&["portable.ini", "portable.txt"]),
                registry: None,
                documents: None,
                app_data: AppData::Documents("PCSX2"),
            },
        })
    }

    /// `-batch` makes PCSX2 quit when the game is shut down, so rommd sees
    /// the session end; `--` ends the options, so no file name is read as one.
    async fn command(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        _game_id: Option<&str>,
        _profile: Option<&ProfileBlob>,
    ) -> Result<(LaunchCommand, LaunchContext)> {
        let mut command = LaunchCommand::new(&config.binary);
        command.arg("-batch").arg("--").arg(rom_path);
        Ok((command, LaunchContext::default()))
    }

    fn fullscreen_args(&self, fullscreen: bool) -> &'static [&'static str] {
        if fullscreen {
            &["-fullscreen"]
        } else {
            &["-nofullscreen"]
        }
    }

    async fn snapshot(&self, _config: &EmulatorConfig, _game_id: &str) -> Result<ProfileBlob> {
        anyhow::bail!("PCSX2 per-game profiles are not supported yet")
    }

    /// The disc serial (`SLUS-20811`), from the disc or, for images rommd
    /// can't read (CHD, CSO), PCSX2's game list cache.
    fn game_id_for_rom(&self, config: &EmulatorConfig, rom_path: &Path) -> Option<String> {
        let id = disc_identity(config, rom_path).map(|(serial, _)| serial);
        if id.is_none() {
            tracing::warn!(
                "could not determine the PS2 serial of {}: not a readable disc image and not \
                 in PCSX2's game list; add its folder to PCSX2's game list, or provide \
                 game_id in the launch request",
                rom_path.display()
            );
        }
        id
    }

    /// Save states are `<serial> (<CRC>).<slot>.p2s` (and `.resume.p2s`) in
    /// the save state folder (`VMManager::GetSaveStateFileName`); the CRC is
    /// the boot program's, so states of another release of the game are not
    /// matched. Saves are the game's folders on the slots' cards.
    async fn save_layout(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        game_id: Option<&str>,
    ) -> SaveLayout {
        let crc = disc_identity(config, rom_path)
            .filter(|(serial, _)| game_id.is_none_or(|id| id == serial))
            .map(|(_, crc)| crc);
        let states = match (game_id, crc) {
            (Some(serial), Some(crc)) => FileSet::stem_with_extension(
                folder(config, "Savestates", "sstates"),
                format!("{serial} ({crc:08X})"),
                "p2s",
            ),
            _ => None,
        };
        SaveLayout {
            saves: game_id.and_then(|serial| memory_cards(config, serial)),
            states,
        }
    }
}

/// Serial and boot program CRC: read from the disc, else from PCSX2's game
/// list cache.
fn disc_identity(config: &EmulatorConfig, rom_path: &Path) -> Option<(String, u32)> {
    ps_disc::read_ps2_boot(rom_path).or_else(|| {
        lookup_in_gamelist_cache(
            &folder(config, "Cache", "cache").join("gamelist.cache"),
            rom_path,
        )
    })
}

/// The cards in PCSX2's enabled slots (`[MemoryCards] Slot<N>_Enable`,
/// `Slot<N>_Filename`, in the `MemoryCards` folder). `None` while a slot holds
/// a folder card, whose saves rommd doesn't sync yet.
fn memory_cards(config: &EmulatorConfig, serial: &str) -> Option<SaveSet> {
    let ini = read_ini(config);
    let dir = folder(config, "MemoryCards", "memcards");
    let mut cards = Vec::new();
    for slot in 1..=2 {
        let key = |name: &str| format!("Slot{slot}_{name}");
        let get = |name: &str| ini.as_ref().and_then(|i| i.get("MemoryCards", &key(name)));
        if get("Enable").is_some_and(|v| v.eq_ignore_ascii_case("false")) {
            continue;
        }
        let name = get("Filename")
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Mcd00{slot}.ps2"));
        let path = dir.join(name);
        if path.is_dir() {
            tracing::warn!(
                "PCSX2's slot {slot} card {} is a folder card; its saves are not synced yet",
                path.display()
            );
            return None;
        }
        cards.push(path);
    }
    if cards.is_empty() {
        return None;
    }
    let binary = config.binary.clone();
    let writable = Hook(std::sync::Arc::new(move || {
        anyhow::ensure!(
            !crate::process::program_running(&binary, &["pcsx2-qt", "pcsx2"]),
            "PCSX2 is running; it keeps the memory cards open and would lose the restored save"
        );
        Ok(())
    }));
    Some(SaveSet::Ps2Cards(Ps2Cards {
        cards,
        game_ids: vec![serial.to_owned()],
        writable: Some(writable),
    }))
}

fn read_ini(config: &EmulatorConfig) -> Option<IniFile> {
    std::fs::read_to_string(config.config_dir.join("inis/PCSX2.ini"))
        .map(|t| IniFile::parse(&t))
        .ok()
}

/// A `[Folders]` entry of `inis/PCSX2.ini`; relative paths are inside the
/// data folder.
fn folder(config: &EmulatorConfig, key: &str, default: &str) -> PathBuf {
    let ini = read_ini(config);
    let dir = ini
        .as_ref()
        .and_then(|i| i.get("Folders", key))
        .filter(|v| !v.is_empty())
        .unwrap_or(default);
    config.config_dir.join(dir)
}

/// PCSX2's game list cache, version 34 (`GameList.cpp`): `GLCE`, the
/// version, then per entry the strings path, serial, title, sort title and
/// English title (u32 LE length + bytes), type and region (u8), size and
/// modification time (u64), CRC (u32) and compatibility (u8). Another version
/// has another layout and gives nothing. PCSX2 stores paths as it joined them
/// (`.../ps2//game.chd`), so they are compared as paths, not strings.
fn lookup_in_gamelist_cache(cache: &Path, rom_path: &Path) -> Option<(String, u32)> {
    const VERSION: u32 = 34;
    let data = std::fs::read(cache).ok()?;
    let mut pos = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let bytes = data.get(pos..pos.checked_add(n)?)?;
        pos += n;
        Some(bytes)
    };
    if take(4)? != b"GLCE" || u32::from_le_bytes(take(4)?.try_into().ok()?) != VERSION {
        return None;
    }
    loop {
        let mut strings = Vec::with_capacity(5);
        for _ in 0..5 {
            let len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            strings.push(String::from_utf8_lossy(take(len)?).into_owned());
        }
        take(1 + 1 + 8 + 8)?;
        let crc = u32::from_le_bytes(take(4)?.try_into().ok()?);
        take(1)?;
        let serial = std::mem::take(&mut strings[1]);
        if Path::new(&strings[0]) == rom_path && !serial.is_empty() && crc != 0 {
            return Some((serial, crc));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TempDir, write};

    fn cache_entry(path: &str, serial: &str, title: &str, crc: u32) -> Vec<u8> {
        let mut e = Vec::new();
        for s in [path, serial, title, "", ""] {
            e.extend_from_slice(&(s.len() as u32).to_le_bytes());
            e.extend_from_slice(s.as_bytes());
        }
        e.extend_from_slice(&[0, 13]);
        e.extend_from_slice(&3_820_060_672u64.to_le_bytes());
        e.extend_from_slice(&1_690_000_000u64.to_le_bytes());
        e.extend_from_slice(&crc.to_le_bytes());
        e.push(0);
        e
    }

    fn cache(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut data = b"GLCE".to_vec();
        data.extend_from_slice(&34u32.to_le_bytes());
        entries.iter().for_each(|e| data.extend_from_slice(e));
        data
    }

    #[tokio::test]
    async fn states_are_named_by_serial_and_crc() {
        let root = TempDir::new("pcsx2");
        let roms = root.path().join("roms/ps2");
        let rom = roms.join("Need for Speed - Underground (USA, Canada).chd");
        write(
            &root.path().join("data/cache/gamelist.cache"),
            &cache(&[
                cache_entry(
                    "/elsewhere/Burnout 3.chd",
                    "SLES-52585",
                    "Burnout 3",
                    0xCE49B0DE,
                ),
                cache_entry(
                    &format!(
                        "{}//{}",
                        roms.display(),
                        rom.file_name().unwrap().to_string_lossy()
                    ),
                    "SLUS-20811",
                    "Need for Speed - Underground",
                    0xCB99CD12,
                ),
            ]),
        );
        let elsewhere = root.path().join("states/elsewhere");
        write(
            &root.path().join("data/inis/PCSX2.ini"),
            format!(
                "[Folders]\nSavestates = {}\nCache = cache\n",
                elsewhere.display()
            )
            .as_bytes(),
        );
        let config = EmulatorConfig {
            binary: "pcsx2-qt".into(),
            config_dir: root.path().join("data"),
        };

        assert_eq!(
            Pcsx2.game_id_for_rom(&config, &rom).as_deref(),
            Some("SLUS-20811")
        );
        let layout = Pcsx2.save_layout(&config, &rom, Some("SLUS-20811")).await;
        assert!(matches!(layout.saves, Some(SaveSet::Ps2Cards(_))));
        let states = layout.states.unwrap();
        assert_eq!(states.dir, elsewhere);
        assert!(states.matcher.matches_name("SLUS-20811 (CB99CD12).01.p2s"));
        assert!(
            states
                .matcher
                .matches_name("SLUS-20811 (CB99CD12).resume.p2s")
        );
        assert!(
            !states
                .matcher
                .matches_name("SLUS-20811 (CB99CD12).01.p2s.backup")
        );
        assert!(!states.matcher.matches_name("SLES-52585 (CE49B0DE).01.p2s"));

        // A game ID that is not this disc's gets no CRC, so no states.
        let other = Pcsx2.save_layout(&config, &rom, Some("SLUS-99999")).await;
        assert!(other.states.is_none());
        // Unknown ROM: nothing.
        let unknown = roms.join("unknown.chd");
        assert_eq!(Pcsx2.game_id_for_rom(&config, &unknown), None);
    }

    #[test]
    fn other_cache_versions_are_ignored() {
        let root = TempDir::new("pcsx2-cache");
        let path = root.path().join("gamelist.cache");
        let mut data = cache(&[cache_entry("/r/g.iso", "SLUS-20811", "NFS", 1)]);
        data[4] = 35;
        write(&path, &data);
        assert_eq!(lookup_in_gamelist_cache(&path, Path::new("/r/g.iso")), None);
        data[4] = 34;
        write(&path, &data);
        assert_eq!(
            lookup_in_gamelist_cache(&path, Path::new("/r/g.iso")),
            Some(("SLUS-20811".into(), 1))
        );
    }

    #[tokio::test]
    async fn launches_in_batch_mode() {
        let config = EmulatorConfig {
            binary: "/Applications/PCSX2.app/Contents/MacOS/PCSX2".into(),
            config_dir: "/data".into(),
        };
        let (command, _) = Pcsx2
            .command(&config, Path::new("/roms/-game.iso"), None, None)
            .await
            .unwrap();
        assert_eq!(command.args, ["-batch", "--", "/roms/-game.iso"]);
    }

    #[tokio::test]
    async fn fullscreen_options_go_before_the_end_of_options() {
        let config = EmulatorConfig {
            binary: "pcsx2-qt".into(),
            config_dir: "/data".into(),
        };
        let (mut command, _) = Pcsx2
            .command(&config, Path::new("/roms/game.iso"), None, None)
            .await
            .unwrap();
        command.set_fullscreen(&Pcsx2, true);
        assert_eq!(
            command.args,
            ["-fullscreen", "-batch", "--", "/roms/game.iso"]
        );
    }

    #[test]
    fn saves_are_on_the_enabled_slots_cards() {
        let root = TempDir::new("pcsx2-cards");
        let config = EmulatorConfig {
            binary: "/usr/bin/pcsx2-qt".into(),
            config_dir: root.path().to_owned(),
        };
        let cards = |config: &EmulatorConfig| match memory_cards(config, "SLUS-20152") {
            Some(SaveSet::Ps2Cards(c)) => Some((c.cards, c.game_ids)),
            _ => None,
        };
        let memcards = root.path().join("memcards");
        assert_eq!(
            cards(&config),
            Some((
                vec![memcards.join("Mcd001.ps2"), memcards.join("Mcd002.ps2")],
                vec!["SLUS-20152".to_owned()]
            )),
            "PCSX2's defaults"
        );

        write(
            &root.path().join("inis/PCSX2.ini"),
            b"[Folders]\nMemoryCards = cards\n\n[MemoryCards]\nSlot1_Enable = true\n\
              Slot1_Filename = Mine.ps2\nSlot2_Enable = false\n",
        );
        let mine = root.path().join("cards/Mine.ps2");
        assert_eq!(cards(&config).unwrap().0, vec![mine.clone()]);

        std::fs::create_dir_all(&mine).unwrap();
        assert_eq!(cards(&config), None, "a folder card");
    }
}
