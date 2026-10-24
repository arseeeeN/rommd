use anyhow::{Context, Result};
use async_trait::async_trait;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use tokio::process::Child;
use tracing::debug;

use super::{Emulator, FileSet, LaunchContext, SaveLayout, SaveSet};
use crate::config::EmulatorConfig;
use crate::ini::IniFile;
use crate::profile::ProfileBlob;

pub struct MGba;

#[async_trait]
impl Emulator for MGba {
    fn id(&self) -> &'static str {
        "mgba"
    }

    fn display_name(&self) -> &'static str {
        "mGBA"
    }

    fn platforms(&self) -> &'static [&'static str] {
        &["gb", "gbc", "gba"]
    }

    fn config_dir_markers(&self) -> &'static [&'static str] {
        &["config.ini"]
    }

    fn config_dir_hint(&self) -> &'static str {
        if cfg!(windows) {
            r"%APPDATA%\mGBA (mGBA's install folder when it holds portable.ini)"
        } else if cfg!(target_os = "macos") {
            "~/.config/mgba"
        } else {
            "~/.config/mgba ($XDG_CONFIG_HOME/mgba when XDG_CONFIG_HOME is set)"
        }
    }

    fn mac_app(&self) -> Option<crate::detect::MacApp> {
        Some(crate::detect::MacApp {
            bundle_id: "com.endrift.mgba-qt",
            executable: None,
            data_dir: ".config/mgba",
        })
    }

    fn linux_app(&self) -> Option<crate::detect::LinuxApp> {
        Some(crate::detect::LinuxApp {
            app_ids: &["io.mgba.mGBA"],
            executables: &["mgba-qt"],
            update_info: &[],
            data_dir: crate::detect::XdgDir::Config("mgba"),
        })
    }

    fn game_id_for_rom(&self, _config: &EmulatorConfig, rom_path: &Path) -> Option<String> {
        rom_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
    }

    /// mGBA names saves and states after the ROM file stem (`Game.sav`,
    /// `Game.ss1`). RomM's web player (EmulatorJS, mGBA core) stores the same
    /// battery save as `<rom stem>.srm`, so those restore as `.sav` and the
    /// save moves between the browser and desktop mGBA.
    async fn save_layout(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        _game_id: Option<&str>,
    ) -> SaveLayout {
        let stem = rom_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let saves_dir = read_path_key(config, "savegamePath")
            .unwrap_or_else(|| config.config_dir.join("saves"));
        let states_dir = read_path_key(config, "savestatePath")
            .unwrap_or_else(|| config.config_dir.join("states"));
        SaveLayout {
            saves: FileSet::stem(saves_dir, stem.clone())
                .map(|set| SaveSet::Files(set.with_remote_extensions(&[("srm", "sav")]))),
            states: FileSet::stem(states_dir, stem),
        }
    }

    async fn launch(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        _game_id: Option<&str>,
        profile: Option<&ProfileBlob>,
    ) -> Result<(Child, LaunchContext)> {
        ensure_paths_configured(config).await?;

        #[cfg_attr(windows, allow(unused_mut))]
        let mut launch_ctx = LaunchContext::default();
        let mut cmd = super::emulator_command(&config.binary);

        // Write a merged config to a temp dir and point XDG_CONFIG_HOME at it.
        #[cfg(not(windows))]
        if let Some(profile) = profile {
            let temp_dir = write_temp_config_dir(config, profile).await?;
            cmd.env("XDG_CONFIG_HOME", &temp_dir);
            debug!(
                "launching mGBA with temp config dir {} ({} override(s))",
                temp_dir.display(),
                profile.overlay.len()
            );
            launch_ctx.temp_dirs.push(temp_dir);
        }
        // On Windows mGBA finds its config through the shell's known folders,
        // which no environment variable redirects, so pass the settings as
        // `-C key=value` overrides instead.
        #[cfg(windows)]
        if let Some(profile) = profile {
            let (overrides, skipped) = command_line_overrides(&profile.overlay);
            if !skipped.is_empty() {
                tracing::warn!(
                    "mGBA on Windows cannot take these profile settings on its command line: {}",
                    skipped.join(", ")
                );
            }
            debug!("launching mGBA with {} -C override(s)", overrides.len());
            for o in overrides {
                cmd.arg("-C").arg(o);
            }
        }

        cmd.arg(rom_path);
        let child = cmd
            .spawn()
            .with_context(|| format!("spawning mGBA from {}", config.binary.display()))?;

        Ok((child, launch_ctx))
    }

    /// mGBA has no per-game settings — snapshot returns the global config as
    /// the overlay so callers have something to diff against or store as a base.
    async fn snapshot(&self, config: &EmulatorConfig, _game_id: &str) -> Result<ProfileBlob> {
        let base_path = config.config_dir.join("config.ini");
        let overlay = if base_path.exists() {
            let text = tokio::fs::read_to_string(&base_path)
                .await
                .context("reading mGBA config.ini")?;
            IniFile::parse(&text).to_flat_map()
        } else {
            IndexMap::new()
        };
        Ok(ProfileBlob::new(overlay))
    }
}

/// `key=value` arguments for mGBA's `-C` option. mGBA consults these before
/// its `[ports.qt]` and root settings, but not for other sections such as
/// input mappings, whose keys are returned separately.
#[cfg_attr(not(windows), allow(dead_code))]
fn command_line_overrides(overlay: &IndexMap<String, String>) -> (Vec<String>, Vec<&str>) {
    let mut overrides = Vec::new();
    let mut skipped = Vec::new();
    for (flat_key, value) in overlay {
        match flat_key.rsplit_once('.') {
            Some(("" | "ports.qt", key)) => overrides.push(format!("{}={}", key, value)),
            _ => skipped.push(flat_key.as_str()),
        }
    }
    (overrides, skipped)
}

/// Write a merged config to `<fresh temp dir>/mgba/config.ini` for use as
/// `XDG_CONFIG_HOME`. The caller is responsible for cleanup.
#[cfg(not(windows))]
async fn write_temp_config_dir(config: &EmulatorConfig, profile: &ProfileBlob) -> Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_dir = std::env::temp_dir().join(format!("rommd-mgba-{}", ts));
    let mgba_dir = temp_dir.join("mgba");
    tokio::fs::create_dir_all(&mgba_dir)
        .await
        .context("creating temp config dir")?;

    let base_path = config.config_dir.join("config.ini");
    let mut ini = if base_path.exists() {
        let text = tokio::fs::read_to_string(&base_path)
            .await
            .context("reading mGBA config.ini")?;
        IniFile::parse(&text)
    } else {
        IniFile::default()
    };
    ini.apply_overlay(&profile.overlay);

    tokio::fs::write(mgba_dir.join("config.ini"), ini.to_string())
        .await
        .context("writing temp mGBA config")?;

    Ok(temp_dir)
}

/// Read a path key from mGBA's config.ini.
/// Checks the root (no-section) entries first, then [ports.qt].
fn read_path_key(config: &EmulatorConfig, key: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(config.config_dir.join("config.ini")).ok()?;
    let ini = IniFile::parse(&text);
    let val = ini.get("", key).or_else(|| ini.get("ports.qt", key))?;
    let p = Path::new(val);
    Some(if p.is_absolute() {
        p.to_owned()
    } else {
        config.config_dir.join(p)
    })
}

/// Ensure `savegamePath` and `savestatePath` are present in mGBA's config.ini.
/// Writes sensible defaults under [ports.qt] when the keys are absent.
async fn ensure_paths_configured(config: &EmulatorConfig) -> Result<()> {
    let config_path = config.config_dir.join("config.ini");

    let mut ini = if config_path.exists() {
        let text = tokio::fs::read_to_string(&config_path)
            .await
            .context("reading mGBA config")?;
        IniFile::parse(&text)
    } else {
        IniFile::default()
    };

    let has_saves =
        ini.get("", "savegamePath").is_some() || ini.get("ports.qt", "savegamePath").is_some();
    let has_states =
        ini.get("", "savestatePath").is_some() || ini.get("ports.qt", "savestatePath").is_some();

    if has_saves && has_states {
        return Ok(());
    }

    let saves_dir = config.config_dir.join("saves");
    let states_dir = config.config_dir.join("states");
    tokio::fs::create_dir_all(&saves_dir).await?;
    tokio::fs::create_dir_all(&states_dir).await?;

    if !has_saves {
        ini.set(
            "ports.qt",
            "savegamePath",
            saves_dir.to_string_lossy().as_ref(),
        );
    }
    if !has_states {
        ini.set(
            "ports.qt",
            "savestatePath",
            states_dir.to_string_lossy().as_ref(),
        );
    }

    if let Some(parent) = config_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&config_path, ini.to_string())
        .await
        .context("writing mGBA config")?;

    debug!("configured mGBA save paths in {}", config_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TempDir, write};

    #[test]
    fn command_line_overrides_cover_core_settings_only() {
        let overlay: IndexMap<String, String> = [
            ("ports.qt.frameskip", "2"),
            (".volume", "0x80"),
            ("gba.input.QT_K.keyA", "88"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let (overrides, skipped) = command_line_overrides(&overlay);
        assert_eq!(overrides, ["frameskip=2", "volume=0x80"]);
        assert_eq!(skipped, ["gba.input.QT_K.keyA"]);
    }

    #[tokio::test]
    async fn layout_follows_configured_paths_and_maps_srm() {
        let root = TempDir::new("mgba");
        let config = EmulatorConfig {
            binary: "mgba".into(),
            config_dir: root.path().to_owned(),
        };
        let saves_dir = root.path().join("saves");
        write(
            &root.path().join("config.ini"),
            format!("[ports.qt]\nsavegamePath={}\n", saves_dir.display()).as_bytes(),
        );
        let layout = MGba
            .save_layout(&config, Path::new("/roms/Golden Sun (USA).gba"), None)
            .await;
        let Some(SaveSet::Files(saves)) = layout.saves else {
            panic!("expected single-file saves")
        };
        assert_eq!(saves.dir, saves_dir);
        assert!(saves.matcher.matches_name("Golden Sun (USA).sav"));
        assert!(
            !saves
                .matcher
                .matches_name("Golden Sun - The Lost Age (USA).sav")
        );
        assert_eq!(
            saves.local_name("Golden Sun (USA).srm"),
            "Golden Sun (USA).sav"
        );
        assert_eq!(layout.states.unwrap().dir, root.path().join("states"));
    }
}
