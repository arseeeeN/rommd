// SPDX-License-Identifier: MPL-2.0
use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;

use super::{Emulator, FileSet, LaunchCommand, LaunchContext, N64Files, SaveLayout, SaveSet};
use crate::config::EmulatorConfig;
use crate::n64;
use crate::profile::ProfileBlob;

/// gopher64 (Nintendo 64). Paths and file names are from v1.1.42:
/// `ui::get_dirs` in `src/ui.rs` (data folder), `ui::storage::init` in
/// `src/ui/storage.rs` (file names), `src/device/cart/sram.rs` (SRAM and
/// flash kept in bus order).
///
/// `config_dir` is the data folder, holding `saves/` and `states/`: the
/// platform data folder (`dirs::data_dir()/gopher64`), or with `portable.txt`
/// next to the program, `portable_data/data` there.
pub struct Gopher64;

/// The save types' extensions. gopher64 also writes `.img` (an SC64 SD card)
/// and `.romsave` (writes to ROM), both homebrew only and not synced.
const SAVE_EXTENSIONS: &[&str] = &["eep", "sra", "fla", "mpk"];
const STATE_EXTENSIONS: &[&str] = &[
    "state0", "state1", "state2", "state3", "state4", "state5", "state6", "state7", "state8",
    "state9",
];

#[async_trait]
impl Emulator for Gopher64 {
    fn id(&self) -> &'static str {
        "gopher64"
    }

    fn display_name(&self) -> &'static str {
        "gopher64"
    }

    fn platforms(&self) -> &'static [&'static str] {
        &["n64"]
    }

    fn save_family(&self) -> &'static [&'static str] {
        n64::SAVE_COMPATIBLE_EMULATORS
    }

    /// Created on every start.
    fn config_dir_markers(&self) -> &'static [&'static str] {
        &["saves", "states"]
    }

    fn config_dir_hint(&self) -> &'static str {
        if cfg!(windows) {
            r"%APPDATA%\gopher64 (portable_data\data next to the program when portable.txt is there)"
        } else if cfg!(target_os = "macos") {
            "~/Library/Application Support/gopher64"
        } else {
            "~/.local/share/gopher64 (Flatpak: ~/.var/app/io.github.gopher64.gopher64/data/gopher64)"
        }
    }

    /// `Cargo.toml`'s `[package.metadata.bundle]`, built by `cargo bundle`.
    fn mac_app(&self) -> Option<crate::detect::MacApp> {
        Some(crate::detect::MacApp {
            bundle_id: "io.github.gopher64.gopher64",
            executable: None,
            data_dir: "Library/Application Support/gopher64",
        })
    }

    /// `ui::APP_ID` is the Flatpak's ID; releases are a bare program.
    fn linux_app(&self) -> Option<crate::detect::LinuxApp> {
        Some(crate::detect::LinuxApp {
            app_ids: &["io.github.gopher64.gopher64"],
            executables: &["gopher64"],
            update_info: &[],
            data_dir: crate::detect::XdgDir::Data("gopher64"),
        })
    }

    /// `build.rs` compiles the version resource with `winresource`'s
    /// defaults, whose `ProductName` is the package name. Release builds are
    /// named `gopher64-windows-<arch>.exe`.
    fn windows_app(&self) -> Option<crate::detect::WindowsApp> {
        use crate::detect::{AppData, Mark, Portable, WindowsApp, WindowsDataDir};
        Some(WindowsApp {
            executables: &["gopher64*.exe"],
            mark: Mark::Product("gopher64"),
            data_dir: WindowsDataDir {
                portable: Portable::FolderIf("portable_data/data", &["portable.txt"]),
                registry: None,
                documents: None,
                app_data: AppData::Roaming("gopher64"),
            },
        })
    }

    /// gopher64 quits when the game's window is closed.
    async fn command(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        _game_id: Option<&str>,
        _profile: Option<&ProfileBlob>,
    ) -> Result<(LaunchCommand, LaunchContext)> {
        let mut command = LaunchCommand::new(&config.binary);
        command.arg("--").arg(rom_path);
        Ok((command, LaunchContext::default()))
    }

    /// There is no windowed option; without `--fullscreen` its setting applies.
    fn fullscreen_args(&self, fullscreen: bool) -> &'static [&'static str] {
        if fullscreen { &["--fullscreen"] } else { &[] }
    }

    async fn snapshot(&self, _config: &EmulatorConfig, _game_id: &str) -> Result<ProfileBlob> {
        anyhow::bail!("gopher64 per-game profiles are not supported yet")
    }

    /// The SHA-256 of the ROM in big-endian order, which gopher64 names the
    /// game's files after.
    fn game_id_for_rom(&self, _config: &EmulatorConfig, rom_path: &Path) -> Option<String> {
        let id = n64::rom_digest::<sha2::Sha256>(rom_path);
        if id.is_none() {
            tracing::warn!("{} is not an N64 ROM gopher64 can read", rom_path.display());
        }
        id
    }

    /// `saves/<name>-<SHA-256>.<ext>` and `states/<name>-<SHA-256>.state<N>`.
    async fn save_layout(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        game_id: Option<&str>,
    ) -> SaveLayout {
        let Some(hash) = game_id else {
            return SaveLayout::default();
        };
        let tag = format!("-{hash}");
        let saves =
            FileSet::tagged(config.config_dir.join("saves"), &tag, SAVE_EXTENSIONS).map(|set| {
                SaveSet::N64(N64Files {
                    set,
                    new_stem: n64::read_header(rom_path)
                        .and_then(|h| file_prefix(&h))
                        .map(|prefix| format!("{prefix}{tag}")),
                    word_swapped: false,
                })
            });
        SaveLayout {
            saves,
            states: FileSet::tagged(config.config_dir.join("states"), tag, STATE_EXTENSIONS),
        }
    }
}

/// What gopher64 puts before the hash (`get_game_name`): the header name
/// without anything but ASCII letters, digits, `_`, ` ` and `-`, trimmed;
/// when that leaves nothing, the 3-character game ID (`UNK` when it holds a
/// NUL). `None` where gopher64 itself would fail.
fn file_prefix(header: &[u8; 0x40]) -> Option<String> {
    let name = std::str::from_utf8(n64::header_name(header))
        .map(|s| {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ' ' | '-'))
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .unwrap_or_default();
    if !name.is_empty() {
        return Some(name);
    }
    let id = &header[0x3B..0x3E];
    if id.contains(&0) {
        Some("UNK".to_owned())
    } else {
        String::from_utf8(id.to_vec()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TempDir, write};

    fn z64(name: &[u8], game_id: &[u8; 3]) -> Vec<u8> {
        let mut rom = vec![0u8; 0x1000];
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        rom[0x20..0x34].fill(b' ');
        rom[0x20..0x20 + name.len()].copy_from_slice(name);
        rom[0x3B..0x3E].copy_from_slice(game_id);
        rom
    }

    #[test]
    fn prefix_is_the_cleaned_header_name_else_the_game_id() {
        let header = |rom: Vec<u8>| -> [u8; 0x40] { rom[..0x40].try_into().unwrap() };
        assert_eq!(
            file_prefix(&header(z64(b"THE LEGEND OF ZELDA", b"CZL"))).as_deref(),
            Some("THE LEGEND OF ZELDA")
        );
        assert_eq!(
            file_prefix(&header(z64(b"F-ZERO X!", b"CFZ"))).as_deref(),
            Some("F-ZERO X")
        );
        assert_eq!(
            file_prefix(&header(z64(b"\xb6\xb0\xcb\xde", b"NKT"))).as_deref(),
            Some("NKT")
        );
        assert_eq!(
            file_prefix(&header(z64(b"", b"\0\0\0"))).as_deref(),
            Some("UNK")
        );
    }

    #[tokio::test]
    async fn layout_names_files_after_the_rom_hash() {
        let root = TempDir::new("gopher64-layout");
        let rom = root.path().join("Zelda.z64");
        write(&rom, &z64(b"THE LEGEND OF ZELDA", b"CZL"));
        let data = root.path().join("data");
        let config = EmulatorConfig {
            binary: "gopher64".into(),
            config_dir: data.clone(),
        };
        let hash = Gopher64.game_id_for_rom(&config, &rom).unwrap();
        assert_eq!(hash.len(), 64);
        let saves = data.join("saves");
        let states = data.join("states");
        write(&saves.join(format!("THE LEGEND OF ZELDA-{hash}.eep")), b"x");
        write(
            &saves.join(format!("THE LEGEND OF ZELDA-{hash}.romsave")),
            b"x",
        );
        write(&saves.join("OTHER-00.eep"), b"x");
        write(
            &states.join(format!("THE LEGEND OF ZELDA-{hash}.state3")),
            b"x",
        );

        let layout = Gopher64.save_layout(&config, &rom, Some(&hash)).await;
        let Some(SaveSet::N64(files)) = layout.saves else {
            panic!("expected N64 saves")
        };
        assert_eq!(
            files.set.files(),
            vec![saves.join(format!("THE LEGEND OF ZELDA-{hash}.eep"))]
        );
        assert_eq!(
            files.new_stem.as_deref(),
            Some(format!("THE LEGEND OF ZELDA-{hash}").as_str())
        );
        assert!(!files.word_swapped);
        assert_eq!(
            layout.states.unwrap().files(),
            vec![states.join(format!("THE LEGEND OF ZELDA-{hash}.state3"))]
        );
    }

    #[tokio::test]
    async fn command_ends_options_before_the_rom() {
        let config = EmulatorConfig {
            binary: "gopher64".into(),
            config_dir: "/data".into(),
        };
        let (mut command, _) = Gopher64
            .command(&config, Path::new("/roms/-game.z64"), None, None)
            .await
            .unwrap();
        command.set_fullscreen(&Gopher64, true);
        assert_eq!(command.args, ["--fullscreen", "--", "/roms/-game.z64"]);
    }
}
