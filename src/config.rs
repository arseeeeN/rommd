use anyhow::{Context, Result};
use indexmap::IndexMap;
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
    /// In file order: when several emulators run a platform, the first one is
    /// used unless `platforms` names one.
    #[serde(default)]
    pub emulators: IndexMap<String, EmulatorConfig>,
    /// RomM platform slug → emulator ID, for platforms several configured
    /// emulators can run.
    #[serde(default)]
    pub platforms: IndexMap<String, String>,
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
        // Only the install folders: looking further (Spotlight, Downloads)
        // could read an app on the Desktop, and the daemon's first start is
        // unattended.
        let found: Vec<_> = crate::detect::detect(&crate::emulator::all(), false)
            .found
            .into_iter()
            .filter(|f| f.installed)
            .collect();
        let text = default_config(Os::current(), &found);
        std::fs::write(&path, &text)
            .with_context(|| format!("writing default config to {}", path.display()))?;
        let names: Vec<&str> = found.iter().map(|f| f.emulator).collect();
        info!(
            "created default config at {} with the emulators found here: {}",
            path.display(),
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        );
        Self::parse(&text, &path)
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

/// The start of the config written on first run. `{ROMS_DIR}` is this OS's
/// default.
const TEMPLATE_HEAD: &str = r#"# rommd configuration — edit to match your setup.
# Both ~ and %ENVVAR% are expanded in path fields.

# ROMs are stored under roms_dir/<platform_slug>/<rom_file>, by default in
# {ROMS_DIR}.
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
# When several emulators run the same platform (Eden and Ryujinx), the one
# listed first is used unless [platforms] below picks another.
# `rommd detect` lists the emulators installed here; `rommd detect --save`
# adds them to this file.
"#;

/// The end of the config written on first run, after the emulators.
const TEMPLATE_TAIL: &str = r#"# ── Platforms ─────────────────────────────────────────────────────────────────
# Which emulator runs a platform's games when several can, by RomM platform
# slug (switch, ngc, wii, 3ds, psx, gba, ...; `rommd search` shows them):
# [platforms]
# switch = "eden"

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Os {
    Mac,
    Windows,
    Linux,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }

    fn default_roms_dir(self) -> &'static str {
        match self {
            Self::Mac => "~/Library/Application Support/rommd/roms",
            Self::Windows => r"%LOCALAPPDATA%\rommd\roms",
            Self::Linux => "~/.local/share/rommd/roms",
        }
    }

    /// Notes before this OS's emulator examples.
    fn examples_note(self) -> &'static str {
        match self {
            Self::Windows => {
                "# Write Windows paths in single quotes: in double quotes a backslash starts\n\
                 # an escape sequence. Portable installs keep config_dir next to the program\n\
                 # (DuckStation and mGBA: its folder; Dolphin, Eden, Azahar: \"User\"/\"user\";\n\
                 # Ryujinx: \"portable\").\n#\n"
            }
            _ => "",
        }
    }

    /// Example entries: (emulator ID, `binary` line value, `config_dir` line
    /// value), written as they appear in the file.
    fn examples(self) -> &'static [(&'static str, &'static str, &'static str)] {
        match self {
            Self::Mac => &[
                (
                    "duckstation",
                    r#""/Applications/DuckStation.app/Contents/MacOS/DuckStation""#,
                    r#""~/Library/Application Support/DuckStation""#,
                ),
                (
                    "mgba",
                    r#""/Applications/mGBA.app/Contents/MacOS/mGBA""#,
                    r#""~/.config/mgba""#,
                ),
                (
                    "dolphin",
                    r#""/Applications/Dolphin.app/Contents/MacOS/Dolphin""#,
                    r#""~/Library/Application Support/Dolphin""#,
                ),
                (
                    "ryujinx",
                    r#""/Applications/Ryujinx.app/Contents/MacOS/Ryujinx""#,
                    r#""~/Library/Application Support/Ryujinx""#,
                ),
                (
                    "azahar",
                    r#""/Applications/Azahar.app/Contents/MacOS/azahar""#,
                    r#""~/Library/Application Support/Azahar""#,
                ),
            ],
            Self::Windows => &[
                (
                    "duckstation",
                    r"'C:\Program Files\DuckStation\duckstation-qt-x64-ReleaseLTCG.exe'",
                    r"'%LOCALAPPDATA%\DuckStation'   # older installs: '~\Documents\DuckStation'",
                ),
                (
                    "mgba",
                    r"'C:\Program Files\mGBA\mGBA.exe'",
                    r"'%APPDATA%\mGBA'",
                ),
                (
                    "dolphin",
                    r"'C:\Program Files\Dolphin\Dolphin.exe'",
                    r"'%APPDATA%\Dolphin Emulator'  # older installs: '~\Documents\Dolphin Emulator'",
                ),
                (
                    "ryujinx",
                    r"'C:\Program Files\Ryujinx\Ryujinx.exe'",
                    r"'%APPDATA%\Ryujinx'",
                ),
                (
                    "eden",
                    r"'C:\Program Files\Eden\eden.exe'",
                    r"'%APPDATA%\eden'",
                ),
                (
                    "azahar",
                    r"'C:\Program Files\Azahar\azahar.exe'",
                    r"'%APPDATA%\Azahar'",
                ),
            ],
            Self::Linux => &[
                (
                    "duckstation",
                    r#""/usr/bin/duckstation-qt""#,
                    r#""~/.local/share/duckstation"   # or $XDG_CONFIG_HOME/duckstation if set"#,
                ),
                ("mgba", r#""/usr/bin/mgba-qt""#, r#""~/.config/mgba""#),
                (
                    "dolphin",
                    r#""/usr/bin/dolphin-emu""#,
                    r#""~/.config/dolphin-emu""#,
                ),
                ("ryujinx", r#""/usr/bin/Ryujinx""#, r#""~/.config/Ryujinx""#),
            ],
        }
    }
}

/// The config written on first run: this OS's notes and examples, with the
/// emulators found on this computer filled in and their examples left out.
pub fn default_config(os: Os, found: &[crate::detect::Found]) -> String {
    let mut text = TEMPLATE_HEAD.replace("{ROMS_DIR}", os.default_roms_dir());
    if !found.is_empty() {
        text.push_str("#\n# Found on this computer when this file was created:\n\n");
        let mut doc = toml_edit::DocumentMut::new();
        let mut emulators = toml_edit::Table::new();
        emulators.set_implicit(true);
        for (i, f) in found.iter().enumerate() {
            let mut entry = crate::config_edit::entry_table(&f.binary, &f.config_dir);
            if i > 0 {
                entry.decor_mut().set_prefix("\n");
            }
            if !f.config_dir_in_use
                && let Some(dir) = entry.get_mut("config_dir").and_then(|v| v.as_value_mut())
            {
                dir.decor_mut()
                    .set_suffix(format!("  # created when {} first runs", f.name));
            }
            emulators.insert(f.emulator, toml_edit::Item::Table(entry));
        }
        doc.insert("emulators", toml_edit::Item::Table(emulators));
        text.push_str(&doc.to_string());
        for (platform, ids) in crate::detect::shared_platforms(found) {
            text.push_str(&format!(
                "\n# {} all run {platform}: {} is used, being listed first. To use another,\n\
                 # set it under [platforms] below, e.g. {platform} = \"{}\".\n",
                ids.join(", "),
                ids[0],
                ids[1]
            ));
        }
    }
    let examples: Vec<_> = os
        .examples()
        .iter()
        .filter(|(id, _, _)| !found.iter().any(|f| f.emulator == *id))
        .collect();
    if !examples.is_empty() {
        text.push_str(if found.is_empty() {
            "#\n# Examples (remove the # to use one):\n#\n"
        } else {
            "\n# More examples (remove the # to use one):\n#\n"
        });
        text.push_str(os.examples_note());
        for (i, (id, binary, config_dir)) in examples.iter().enumerate() {
            if i > 0 {
                text.push_str("#\n");
            }
            text.push_str(&format!(
                "# [emulators.{id}]\n# binary     = {binary}\n# config_dir = {config_dir}\n"
            ));
        }
    }
    text.push('\n');
    text.push_str(TEMPLATE_TAIL);
    text
}

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

    const ALL_OS: [Os; 3] = [Os::Mac, Os::Windows, Os::Linux];

    /// The generated config with its emulator examples uncommented.
    fn uncommented(text: &str) -> String {
        text.lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(rest) if rest.starts_with("[emulators.") => rest,
                Some(rest) if rest.starts_with("binary") || rest.starts_with("config_dir") => rest,
                _ => l,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn found(emulator: &'static str, in_use: bool) -> crate::detect::Found {
        crate::detect::Found {
            emulator,
            name: "Dolphin",
            kind: crate::detect::Kind::App,
            install: PathBuf::from("/Applications/Dolphin.app"),
            binary: PathBuf::from("/Applications/Dolphin.app/Contents/MacOS/Dolphin"),
            config_dir: PathBuf::from("/Users/me/Library/Application Support/Dolphin"),
            config_dir_in_use: in_use,
            installed: true,
            sandbox: None,
        }
    }

    #[test]
    fn the_generated_config_has_this_os_examples_only() {
        for os in ALL_OS {
            let text = default_config(os, &[]);
            let config = Config::parse(&text, Path::new("t")).unwrap();
            assert!(
                config.emulators.is_empty(),
                "{os:?}: examples stay commented"
            );
            assert!(text.contains(os.default_roms_dir()), "{os:?}");
            let examples = Config::parse(&uncommented(&text), Path::new("t"))
                .unwrap_or_else(|e| panic!("{os:?}: {e:#}\n{text}"));
            assert_eq!(examples.emulators.len(), os.examples().len(), "{os:?}");
            for other in ALL_OS.into_iter().filter(|o| *o != os) {
                assert!(
                    !text.contains(other.default_roms_dir()),
                    "{os:?} mentions {other:?}"
                );
            }
        }
        let windows = Config::parse(
            &uncommented(&default_config(Os::Windows, &[])),
            Path::new("t"),
        )
        .unwrap();
        assert_eq!(
            windows.emulators["mgba"].binary,
            PathBuf::from(r"C:\Program Files\mGBA\mGBA.exe")
        );
    }

    #[test]
    fn the_generated_config_fills_in_the_emulators_found() {
        let text = default_config(Os::Mac, &[found("dolphin", false), found("mgba", true)]);
        let config = Config::parse(&text, Path::new("t")).unwrap();
        assert_eq!(
            config.emulators.keys().collect::<Vec<_>>(),
            ["dolphin", "mgba"],
            "in the order found"
        );
        assert_eq!(
            config.emulators["dolphin"].binary,
            PathBuf::from("/Applications/Dolphin.app/Contents/MacOS/Dolphin")
        );
        assert!(
            text.contains("  # created when Dolphin first runs"),
            "{text}"
        );
        assert!(
            !text.contains("# [emulators.dolphin]"),
            "its example is left out"
        );
        assert!(
            text.contains("# [emulators.ryujinx]"),
            "other examples stay"
        );
        let examples = Config::parse(&uncommented(&text), Path::new("t")).unwrap();
        assert_eq!(examples.emulators.len(), Os::Mac.examples().len());

        let more = crate::config_edit::with_emulator(
            &text,
            Path::new("t"),
            "azahar",
            Path::new("/a"),
            Path::new("/a"),
        )
        .unwrap();
        let mgba_end = more.find("[emulators.mgba]").unwrap();
        assert!(
            more[mgba_end..].find("[emulators.azahar]").unwrap()
                < more[mgba_end..].find("# More examples").unwrap(),
            "detect --save adds to the same section"
        );
    }

    #[test]
    fn the_generated_config_says_which_of_two_switch_emulators_runs() {
        let text = default_config(Os::Linux, &[found("eden", true), found("ryujinx", true)]);
        assert!(
            text.contains(
                "# eden, ryujinx all run switch: eden is used, being listed first. To use \
                 another,\n# set it under [platforms] below, e.g. switch = \"ryujinx\"."
            ),
            "{text}"
        );
        let config = Config::parse(&text, Path::new("t")).unwrap();
        assert!(config.platforms.is_empty(), "only a hint");
        let one = default_config(Os::Linux, &[found("eden", true), found("dolphin", true)]);
        assert!(!one.contains("all run"), "{one}");
    }

    #[test]
    fn percent_var_expanded() {
        // SAFETY: single-threaded test, no concurrent env access.
        unsafe { std::env::set_var("ROMMD_TEST_VAR", "/test/path") };
        let p = expand_path(PathBuf::from("%ROMMD_TEST_VAR%/sub"));
        assert_eq!(p, PathBuf::from("/test/path/sub"));
    }
}
