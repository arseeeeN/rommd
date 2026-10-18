use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::process::Child;
use tracing::warn;

use super::{Emulator, GameSaveDir, LaunchContext, SaveLayout, SaveSet};
use crate::config::EmulatorConfig;
use crate::ini::IniFile;
use crate::profile::ProfileBlob;

/// Eden (yuzu fork). `config_dir` is Eden's data dir (`~/.local/share/eden`);
/// its qt-config.ini lives separately under the XDG config dir unless portable.
///
/// Save layout: `<nand>/user/save/0000000000000000/<USER_ID>/<TITLE_ID>/`, with
/// no metadata files — the directory is the save, so rommd can create it.
pub struct Eden;

const PROFILE_ENTRY_SIZE: usize = 0xC8;
const DEVICE_USER_DIR: &str = "00000000000000000000000000000000";
const PROFILE_TABLE_OFFSET: usize = 0x10;

#[async_trait]
impl Emulator for Eden {
    fn id(&self) -> &'static str {
        "eden"
    }

    fn display_name(&self) -> &'static str {
        "Eden"
    }

    fn config_dir_markers(&self) -> &'static [&'static str] {
        &["nand", "keys"]
    }

    fn config_dir_hint(&self) -> &'static str {
        if cfg!(windows) {
            r"%APPDATA%\eden (the user folder next to eden.exe for portable installs)"
        } else if cfg!(target_os = "macos") {
            ""
        } else {
            "~/.local/share/eden"
        }
    }

    fn game_id_for_rom(&self, config: &EmulatorConfig, rom_path: &Path) -> Option<String> {
        crate::switch::title_id_for_rom(rom_path, &config.config_dir.join("keys").join("prod.keys"))
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
        SaveLayout {
            saves: game_id
                .and_then(|id| game_save_dir(config, id))
                .map(SaveSet::Dir),
            states: None,
        }
    }

    async fn launch(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        _game_id: Option<&str>,
        _profile: Option<&ProfileBlob>,
    ) -> Result<(Child, LaunchContext)> {
        let child = super::emulator_command(&config.binary)
            .arg("-g")
            .arg(rom_path)
            .spawn()
            .with_context(|| format!("spawning Eden from {}", config.binary.display()))?;
        Ok((child, LaunchContext::default()))
    }

    async fn snapshot(&self, _config: &EmulatorConfig, _game_id: &str) -> Result<ProfileBlob> {
        anyhow::bail!("Eden does not support per-game profiles")
    }
}

/// Resolve the save dir for the active user (`[System] current_user`, an
/// index into profiles.dat). Games with a per-console "device" save keep it
/// under the all-zero user ID instead; the save found on disk decides, and
/// `DEVICE_SAVE_TITLES` breaks the tie when neither exists yet.
fn game_save_dir(config: &EmulatorConfig, game_id: &str) -> Option<GameSaveDir> {
    let settings = read_qt_config(config);
    if let Some(custom) = setting(settings.as_ref(), "Data%20Storage", "save_directory") {
        warn!(
            "Eden save_directory override ({}) is not supported; skipping save sync",
            custom
        );
        return None;
    }
    let nand = nand_dir(config, settings.as_ref());
    let title = game_id.to_uppercase();
    let users = nand.join("user/save/0000000000000000");
    let device = users.join(DEVICE_USER_DIR).join(&title);
    let account = active_user_dir(&nand, settings.as_ref()).map(|u| users.join(u).join(&title));

    let dir = match account {
        Some(a) if a.is_dir() => a,
        _ if device.is_dir() => device,
        _ if crate::switch::DEVICE_SAVE_TITLES.contains(&title.as_str()) => device,
        Some(a) => a,
        None => return None,
    };
    Some(GameSaveDir {
        dir,
        archive_root: title,
        extra_roots: Vec::new(),
        mirrors: Vec::new(),
        required_subdir: None,
        create: None,
    })
}

fn active_user_dir(nand: &Path, settings: Option<&IniFile>) -> Option<String> {
    let index: usize = setting(settings, "System", "current_user")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let profiles = nand.join("system/save/8000000000000010/su/avators/profiles.dat");
    let data = match std::fs::read(&profiles) {
        Ok(d) => d,
        Err(e) => {
            warn!("cannot read Eden profiles at {}: {}", profiles.display(), e);
            return None;
        }
    };
    let user = user_dir_name(&data, index);
    if user.is_none() {
        warn!(
            "Eden profile index {} not found in {}",
            index,
            profiles.display()
        );
    }
    user
}

/// Portable installs keep `config/qt-config.ini` inside the data dir; otherwise
/// it lives in the XDG config dir.
fn read_qt_config(config: &EmulatorConfig) -> Option<IniFile> {
    let portable = config.config_dir.join("config").join("qt-config.ini");
    let path = if portable.is_file() {
        portable
    } else {
        dirs::config_dir()?.join("eden").join("qt-config.ini")
    };
    std::fs::read_to_string(path)
        .ok()
        .map(|t| IniFile::parse(&t))
}

/// A Qt setting's value, only when the user changed it from the default.
fn setting<'a>(ini: Option<&'a IniFile>, section: &str, key: &str) -> Option<&'a str> {
    let ini = ini?;
    if ini.get(section, &format!("{}\\default", key)) == Some("true") {
        return None;
    }
    ini.get(section, key).filter(|v| !v.is_empty())
}

fn nand_dir(config: &EmulatorConfig, ini: Option<&IniFile>) -> PathBuf {
    setting(ini, "Data%20Storage", "nand_directory")
        .map(PathBuf::from)
        .unwrap_or_else(|| config.config_dir.join("nand"))
}

/// Save folder name for profile `index`: the 128-bit UUID stored little-endian
/// in profiles.dat, printed as uppercase hex (i.e. its bytes reversed).
fn user_dir_name(profiles: &[u8], index: usize) -> Option<String> {
    let start = PROFILE_TABLE_OFFSET + index * PROFILE_ENTRY_SIZE;
    let uuid = profiles.get(start..start + 16)?;
    if uuid.iter().all(|&b| b == 0) {
        return None;
    }
    Some(uuid.iter().rev().map(|b| format!("{:02X}", b)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_dir_from_profiles_dat() {
        let mut data = vec![0u8; PROFILE_TABLE_OFFSET + PROFILE_ENTRY_SIZE];
        data[0x10..0x20].copy_from_slice(&[
            0x47, 0x1c, 0xe7, 0x3e, 0x49, 0x9b, 0x45, 0x62, 0xd1, 0xf6, 0x28, 0x64, 0xdc, 0xf6,
            0xf3, 0xde,
        ]);
        assert_eq!(
            user_dir_name(&data, 0).as_deref(),
            Some("DEF3F6DC6428F6D162459B493EE71C47")
        );
        assert_eq!(user_dir_name(&data, 1), None);
    }

    #[test]
    fn non_default_settings_only() {
        let ini = IniFile::parse(
            "[Data%20Storage]\nnand_directory\\default=false\nnand_directory=/x/nand\n\
             save_directory\\default=true\nsave_directory=\n",
        );
        assert_eq!(
            setting(Some(&ini), "Data%20Storage", "nand_directory"),
            Some("/x/nand")
        );
        assert_eq!(
            setting(Some(&ini), "Data%20Storage", "save_directory"),
            None
        );
    }

    #[tokio::test]
    async fn picks_account_or_device_save() {
        let data = std::env::temp_dir().join(format!("rommd-eden-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let avatars = data.join("nand/system/save/8000000000000010/su/avators");
        std::fs::create_dir_all(&avatars).unwrap();
        let mut profiles = vec![0u8; PROFILE_TABLE_OFFSET + PROFILE_ENTRY_SIZE];
        profiles[0x10] = 0xAB;
        std::fs::write(avatars.join("profiles.dat"), &profiles).unwrap();
        std::fs::create_dir_all(data.join("config")).unwrap();
        std::fs::write(
            data.join("config/qt-config.ini"),
            "[System]\ncurrent_user=0\n",
        )
        .unwrap();
        let config = EmulatorConfig {
            binary: "eden".into(),
            config_dir: data.clone(),
        };
        let users = data.join("nand/user/save/0000000000000000");
        let user = format!("{:0>32}", "AB");

        let dir = |id: &str| {
            let config = config.clone();
            let id = id.to_owned();
            async move { game_save_dir(&config, &id).unwrap().dir }
        };
        assert_eq!(
            dir("0100000000010000").await,
            users.join(&user).join("0100000000010000")
        );
        assert_eq!(
            dir("01006f8002326000").await,
            users.join(DEVICE_USER_DIR).join("01006F8002326000")
        );

        std::fs::create_dir_all(users.join(DEVICE_USER_DIR).join("0100000000020000")).unwrap();
        assert_eq!(
            dir("0100000000020000").await,
            users.join(DEVICE_USER_DIR).join("0100000000020000")
        );
        let _ = std::fs::remove_dir_all(&data);
    }
}
