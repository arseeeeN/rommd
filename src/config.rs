use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::info;

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub romm: Option<RommConfig>,
    /// Root directory for locally cached ROMs. Platform subdirectories (psx/, switch/, …)
    /// are created inside it automatically. Supports ~ expansion.
    pub roms_dir: Option<PathBuf>,
    #[serde(default)]
    pub emulators: HashMap<String, EmulatorConfig>,
    #[serde(default)]
    pub controllers: HashMap<String, ControllerConfig>,
    #[serde(default)]
    pub updates: crate::update::UpdateConfig,
}

/// Physical controller registered on this machine.
///
/// The `type` field selects the variant. Machine-specific details (MAC address,
/// VID/PID, device expressions) stay local and are never sent to RomM.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControllerConfig {
    /// Generic Bluetooth gamepad identified by MAC address.
    Bluetooth {
        name: String,
        mac: String,
        /// SDL device name used to match the live SDL index at launch time.
        sdl_name: Option<String>,
    },
    /// USB gamepad identified by VID + PID (+ optional serial for duplicates).
    Usb {
        name: String,
        vid: String,
        pid: String,
        serial: Option<String>,
    },
    /// Wiimote managed directly by Dolphin's own Bluetooth stack.
    DolphinWiimote {
        name: String,
        /// Dolphin device expression, e.g. `"Bluetooth/0/Nintendo RVL-CNT-01-TR"`.
        device_expr: String,
    },
    /// DSU protocol device (phones, Steam Deck gyro, Wii U Gamepad via Cemuhook).
    Dsu {
        name: String,
        /// DSU device expression, e.g. `"DSUClient/0/0"`.
        device_expr: String,
    },
}

impl ControllerConfig {
    pub fn name(&self) -> &str {
        match self {
            Self::Bluetooth { name, .. }
            | Self::Usb { name, .. }
            | Self::DolphinWiimote { name, .. }
            | Self::Dsu { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    /// Web origins (e.g. `"https://romm.example.com"`) allowed to call the API
    /// from a browser, in addition to the configured RomM server's own origin.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

fn default_port() -> u16 {
    7497
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            allowed_origins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct RommConfig {
    pub base_url: String,
    /// API token (recommended). Create one in RomM → Settings → API Tokens.
    /// Takes precedence over username/password when set.
    pub api_token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct EmulatorConfig {
    /// Path to the emulator binary. Supports ~ expansion.
    pub binary: PathBuf,
    /// Directory where the emulator stores its config (gamesettings, memcards, etc.).
    /// Supports ~ expansion.
    pub config_dir: PathBuf,
}

impl Config {
    /// Load config from disk, creating a commented default file if none exists.
    pub fn load_or_create() -> Result<Self> {
        let path = Self::path();
        if path.exists() {
            return Self::read(&path);
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config directory {}", parent.display()))?;
        }
        std::fs::write(&path, DEFAULT_CONFIG_TEMPLATE)
            .with_context(|| format!("writing default config to {}", path.display()))?;
        info!("created default config at {}", path.display());

        Ok(Self::default())
    }

    /// Read and parse an existing config file.
    pub fn read(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading config from {}", path.display()))?;
        Self::parse(&content, path)
    }

    pub fn parse(content: &str, path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(content).map_err(|e| {
            // The usual cause: a Windows path in double quotes.
            let hint = if e.message().contains("escape") {
                "\nhint: in double quotes a backslash starts an escape sequence; \
                 write Windows paths in single quotes ('C:\\Games\\rom.gba') \
                 or with forward slashes (\"C:/Games/rom.gba\")"
            } else {
                ""
            };
            anyhow::anyhow!("parsing {}: {}{}", path.display(), e, hint)
        })?;
        Ok(config.expand_paths())
    }

    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rommd")
            .join("config.toml")
    }

    /// Resolved ROMs root directory (explicit config value or platform default).
    pub fn roms_dir(&self) -> PathBuf {
        self.roms_dir.clone().unwrap_or_else(default_roms_dir)
    }

    /// Expand `~` and `%VARNAME%` in all path fields after deserialization.
    fn expand_paths(mut self) -> Self {
        if let Some(p) = self.roms_dir {
            self.roms_dir = Some(expand_path(p));
        }
        for cfg in self.emulators.values_mut() {
            cfg.binary = expand_path(std::mem::take(&mut cfg.binary));
            cfg.config_dir = expand_path(std::mem::take(&mut cfg.config_dir));
        }
        self
    }
}

pub fn default_roms_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rommd")
        .join("roms")
}

fn expand_path(path: PathBuf) -> PathBuf {
    let s = match path.to_str() {
        Some(s) => s,
        None => return path,
    };
    // ~ expansion (Unix convention; also works on Windows via dirs::home_dir).
    if s == "~" {
        return dirs::home_dir().unwrap_or(path);
    }
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\"))
        && let Some(home) = dirs::home_dir()
    {
        return PathBuf::from(expand_percent_vars(&home.join(rest).to_string_lossy()));
    }
    PathBuf::from(expand_percent_vars(s))
}

/// Expand `%VARNAME%` placeholders using the current process environment.
/// Unexpandable variables are left as-is.
fn expand_percent_vars(s: &str) -> String {
    let mut result = s.to_owned();
    while let Some(start) = result.find('%') {
        match result[start + 1..].find('%') {
            Some(rel_end) => {
                let end = start + 1 + rel_end;
                let var_name = result[start + 1..end].to_owned();
                match std::env::var(&var_name) {
                    Ok(val) => result.replace_range(start..=end, &val),
                    Err(_) => break,
                }
            }
            None => break,
        }
    }
    result
}

// Written on first run when no config file exists yet.
const DEFAULT_CONFIG_TEMPLATE: &str = r#"# rommd configuration — edit to match your setup.
# Both ~ and %ENVVAR% are expanded in path fields.

# ROMs are stored under roms_dir/<platform_slug>/<rom_file>.
# Defaults to the platform data directory if not set:
#   Linux:   ~/.local/share/rommd/roms
#   macOS:   ~/Library/Application Support/rommd/roms
#   Windows: %LOCALAPPDATA%\rommd\roms
# roms_dir = "~/Roms"

[server]
port = 7497
# Browsers may only call rommd from the RomM server's own page (its base_url
# below). Add other web origins here if a different frontend needs access:
# allowed_origins = ["https://my-frontend.example.com"]

# Updates come from GitHub Releases and are verified before installing.
#   auto   — check daily, install while no game is running, restart (default)
#   notify — check daily and report new versions (log, GET /update)
#   off    — no background checks; POST /update still installs on request
[updates]
mode = "auto"

# RomM server connection. Required for remote launch and save sync.
# [romm]
# base_url = "https://your-romm.example.com"
#
# API token (recommended) — create one in RomM → Settings → API Tokens:
# api_token = "your_api_token_here"
#
# Or authenticate with username and password:
# username = "your_username"
# password = "your_password"

# ── Emulators ─────────────────────────────────────────────────────────────────
# Add one [emulators.<id>] block per emulator. The id is used in API calls.
#
# macOS
# -----
# [emulators.duckstation]
# binary     = "/Applications/DuckStation.app/Contents/MacOS/DuckStation"
# config_dir = "~/Library/Application Support/DuckStation"
#
# [emulators.mgba]
# binary     = "/Applications/mGBA.app/Contents/MacOS/mGBA"
# config_dir = "~/.config/mgba"
#
# [emulators.dolphin]
# binary     = "/Applications/Dolphin.app/Contents/MacOS/Dolphin"
# config_dir = "~/Library/Application Support/Dolphin"
#
# [emulators.ryujinx]
# binary     = "/Applications/Ryujinx.app/Contents/MacOS/Ryujinx"
# config_dir = "~/Library/Application Support/Ryujinx"
#
# Windows
# -------
# Write Windows paths in single quotes: in double quotes a backslash starts
# an escape sequence. Portable installs keep config_dir next to the program
# (DuckStation and mGBA: its folder; Dolphin, Eden, Azahar: "User"/"user";
# Ryujinx: "portable").
#
# [emulators.duckstation]
# binary     = 'C:\Program Files\DuckStation\duckstation-qt-x64-ReleaseLTCG.exe'
# config_dir = '%LOCALAPPDATA%\DuckStation'   # older installs: '~\Documents\DuckStation'
#
# [emulators.mgba]
# binary     = 'C:\Program Files\mGBA\mGBA.exe'
# config_dir = '%APPDATA%\mGBA'
#
# [emulators.dolphin]
# binary     = 'C:\Program Files\Dolphin\Dolphin.exe'
# config_dir = '%APPDATA%\Dolphin Emulator'  # older installs: '~\Documents\Dolphin Emulator'
#
# [emulators.ryujinx]
# binary     = 'C:\Program Files\Ryujinx\Ryujinx.exe'
# config_dir = '%APPDATA%\Ryujinx'
#
# [emulators.eden]
# binary     = 'C:\Program Files\Eden\eden.exe'
# config_dir = '%APPDATA%\eden'
#
# [emulators.azahar]
# binary     = 'C:\Program Files\Azahar\azahar.exe'
# config_dir = '%APPDATA%\Azahar'
#
# Linux
# -----
# [emulators.duckstation]
# binary     = "/usr/bin/duckstation-qt"
# config_dir = "~/.local/share/duckstation"   # or $XDG_CONFIG_HOME/duckstation if set
#
# [emulators.mgba]
# binary     = "/usr/bin/mgba-qt"
# config_dir = "~/.config/mgba"
#
# [emulators.dolphin]
# binary     = "/usr/bin/dolphin-emu"
# config_dir = "~/.config/dolphin-emu"
#
# [emulators.ryujinx]
# binary     = "/usr/bin/Ryujinx"
# config_dir = "~/.config/Ryujinx"

# ── Controllers ───────────────────────────────────────────────────────────────
# Register physical controllers here. The id (e.g. "dualshock_mine") is what
# you reference in launch requests. These entries are machine-local and are
# never sent to RomM.
#
# Bluetooth gamepad (identified by MAC address):
# [controllers.dualshock_mine]
# type     = "bluetooth"
# name     = "My DualShock 4"
# mac      = "AA:BB:CC:DD:EE:FF"
# sdl_name = "DualSense Wireless Controller"   # used for live SDL index matching
#
# USB gamepad (identified by VID + PID):
# [controllers.xbox_living_room]
# type   = "usb"
# name   = "Living Room Xbox"
# vid    = "045E"
# pid    = "02EA"
# serial = ""   # optional — disambiguates multiple identical devices
#
# Wiimote managed by Dolphin's own Bluetooth stack:
# [controllers.wiimote_1]
# type        = "dolphin_wiimote"
# name        = "Wiimote 1"
# device_expr = "Bluetooth/0/Nintendo RVL-CNT-01-TR"
#
# DSU protocol device (phone, Steam Deck gyro, Wii U Gamepad via Cemuhook):
# [controllers.phone_dsu]
# type        = "dsu"
# name        = "Phone (DSU)"
# device_expr = "DSUClient/0/0"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expanded() {
        let toml = "roms_dir = \"~/Roms\"\n[server]\nport = 7497\n";
        let config: Config = toml::from_str(toml).unwrap();
        let dir = config.expand_paths().roms_dir();
        assert_eq!(dir, dirs::home_dir().unwrap().join("Roms"));
    }

    #[test]
    fn backslashes_in_double_quotes_get_a_hint() {
        let err = Config::parse(
            "[emulators.mgba]\nbinary = \"C:\\Program Files\\mGBA\\mGBA.exe\"\nconfig_dir = \"x\"\n",
            Path::new("config.toml"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("single quotes"), "{err}");
    }

    /// Uncomment one platform's emulator examples in the template.
    fn template_examples(platform: &str) -> String {
        DEFAULT_CONFIG_TEMPLATE
            .split(&format!("# {platform}\n"))
            .nth(1)
            .unwrap()
            .lines()
            .take_while(|l| !l.starts_with("# ──") && *l != "# Linux" && *l != "# Windows")
            .filter_map(|l| l.strip_prefix("# "))
            .filter(|l| {
                l.starts_with('[') || l.starts_with("binary") || l.starts_with("config_dir")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn template_examples_parse() {
        for platform in ["macOS", "Windows", "Linux"] {
            let examples = template_examples(platform);
            let config = Config::parse(&examples, Path::new("template"))
                .unwrap_or_else(|e| panic!("{platform}: {e:#}\n{examples}"));
            assert!(config.emulators.len() >= 4, "{platform}: {examples}");
        }
        let windows = Config::parse(&template_examples("Windows"), Path::new("t")).unwrap();
        let mgba = &windows.emulators["mgba"];
        assert_eq!(
            mgba.binary,
            PathBuf::from(r"C:\Program Files\mGBA\mGBA.exe")
        );
    }

    #[test]
    fn percent_var_expanded() {
        // SAFETY: single-threaded test, no concurrent env access.
        unsafe { std::env::set_var("ROMMD_TEST_VAR", "/test/path") };
        let p = expand_path(PathBuf::from("%ROMMD_TEST_VAR%/sub"));
        assert_eq!(p, PathBuf::from("/test/path/sub"));
    }
}
