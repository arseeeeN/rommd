//! Changes rommd makes to the user's config file. The file is for people
//! first, so an edit keeps every comment, blank line and value it doesn't
//! touch exactly as written (`toml_edit`).
//!
//! rommd only edits when asked (`rommd detect --save`, `rommd setup`), never
//! in the background: the user may have the file open, and a silent write is
//! how their work gets lost.

use anyhow::{Context, Result};
use std::path::Path;
use toml_edit::{DocumentMut, Item, Key, Table, value};

use crate::config::{Config, RommConfig};
use crate::update::UpdateMode;

/// Add `[emulators.<id>]` to the config file at `path`.
pub fn add_emulator(path: &Path, id: &str, binary: &Path, config_dir: &Path) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let updated = with_emulator(&text, path, id, binary, config_dir)?;
    write_replacing(path, &updated)
}

/// `text` with `[emulators.<id>]` added after the last emulator table, or at
/// the end when there is none. Refuses a file that doesn't parse (that is the
/// user's to fix, not rommd's to repair) and an emulator that is already
/// there, which is never overwritten.
pub fn with_emulator(
    text: &str,
    path: &Path,
    id: &str,
    binary: &Path,
    config_dir: &Path,
) -> Result<String> {
    let before = Config::parse(text, path)?;
    anyhow::ensure!(
        !before.emulators.contains_key(id),
        "the config already has [emulators.{id}]"
    );
    let mut doc: DocumentMut = text.parse().context("parsing the config")?;
    let first_emulator = before.emulators.is_empty();
    // Comments after the last table belong to the document's end, which is
    // written after every table. The first emulator goes to the very end, so
    // it takes those comments as the text before it.
    let trailing = if first_emulator {
        let t = doc.trailing().as_str().unwrap_or_default().to_owned();
        doc.set_trailing("");
        t
    } else {
        String::new()
    };
    let emulators = doc
        .entry("emulators")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_mut()
        .context("[emulators] is not a table rommd can add to")?;

    let mut entry = entry_table(binary, config_dir);
    if first_emulator && !trailing.trim().is_empty() {
        let mut prefix = trailing;
        if !prefix.ends_with('\n') {
            prefix.push('\n');
        }
        prefix.push('\n');
        entry.decor_mut().set_prefix(prefix);
    }
    // A new table goes after its siblings, so after the last emulator.
    emulators.insert(id, Item::Table(entry));
    let updated = doc.to_string();

    let after = Config::parse(&updated, path).context("the edited config")?;
    let mut expected = before.clone();
    expected
        .emulators
        .insert(id.to_owned(), after.emulators[id].clone());
    anyhow::ensure!(
        after == expected,
        "the edited config doesn't read back as intended"
    );
    Ok(updated)
}

/// What `rommd setup` changes; `None` leaves a setting as it is.
#[derive(Debug, Default, Clone)]
pub struct Settings {
    /// Base URL and API token.
    pub romm: Option<(String, String)>,
    pub roms_dir: Option<std::path::PathBuf>,
    pub update_mode: Option<UpdateMode>,
}

/// The commented examples in the generated config, filled in where they are
/// so the setting stays next to its explanation.
const ROMM_EXAMPLE: &str = "# [romm]\n# base_url = \"https://your-romm.example.com\"\n";
const TOKEN_EXAMPLE: &str = "# api_token = \"your_api_token_here\"\n";
const ROMS_DIR_EXAMPLE: &str = "# roms_dir = \"~/Roms\"\n";

/// `text` with `settings` applied. Other RomM settings (a username and
/// password) are kept; the token takes precedence over them.
pub fn with_settings(text: &str, path: &Path, settings: &Settings) -> Result<String> {
    let before = Config::parse(text, path)?;
    let mut text = text.to_owned();
    if let Some((url, token)) = &settings.romm
        && before.romm.is_none()
        && text.contains(ROMM_EXAMPLE)
        && text.contains(TOKEN_EXAMPLE)
    {
        text = text
            .replacen(
                ROMM_EXAMPLE,
                &format!("[romm]\nbase_url = {}\n", quoted(url)),
                1,
            )
            .replacen(
                TOKEN_EXAMPLE,
                &format!("api_token = {}\n", quoted(token)),
                1,
            );
    }
    if let Some(dir) = &settings.roms_dir
        && before.roms_dir.is_none()
        && text.contains(ROMS_DIR_EXAMPLE)
    {
        text = text.replacen(
            ROMS_DIR_EXAMPLE,
            &format!("roms_dir = {}\n", quoted(&config_path(dir))),
            1,
        );
    }

    let mut doc: DocumentMut = text.parse().context("parsing the config")?;
    let mut expected = before.clone();
    if let Some((url, token)) = &settings.romm {
        let romm = doc
            .entry("romm")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .context("[romm] is not a table rommd can change")?;
        set(romm, "base_url", url);
        set(romm, "api_token", token);
        let (username, password) = before
            .romm
            .map(|r| (r.username, r.password))
            .unwrap_or_default();
        expected.romm = Some(RommConfig {
            base_url: url.clone(),
            api_token: Some(token.clone()),
            username,
            password,
        });
    }
    if let Some(dir) = &settings.roms_dir {
        set(doc.as_table_mut(), "roms_dir", &config_path(dir));
        expected.roms_dir = Some(dir.clone());
    }
    if let Some(mode) = settings.update_mode {
        let updates = doc
            .entry("updates")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .context("[updates] is not a table rommd can change")?;
        set(updates, "mode", mode_name(mode));
        expected.updates.mode = mode;
    }
    let updated = doc.to_string();

    let after = Config::parse(&updated, path).context("the edited config")?;
    anyhow::ensure!(
        after == expected,
        "the edited config doesn't read back as intended"
    );
    Ok(updated)
}

/// Set `key`, keeping the comment after an existing value.
fn set(table: &mut Table, key: &str, new: &str) {
    match table.get_mut(key).and_then(Item::as_value_mut) {
        Some(old) => {
            let decor = old.decor().clone();
            *old = new.into();
            *old.decor_mut() = decor;
        }
        None => {
            table.insert(key, value(new));
        }
    }
}

/// A TOML string as `toml_edit` writes it: a literal string when that saves
/// escaping backslashes.
fn quoted(s: &str) -> String {
    toml_edit::Value::from(s).to_string().trim().to_owned()
}

pub fn mode_name(mode: UpdateMode) -> &'static str {
    match mode {
        UpdateMode::Auto => "auto",
        UpdateMode::Notify => "notify",
        UpdateMode::Off => "off",
    }
}

/// An emulator's table, aligned like the examples in the generated config.
pub fn entry_table(binary: &Path, config_dir: &Path) -> Table {
    let mut entry = Table::new();
    let mut binary_key = Key::new("binary");
    binary_key.leaf_decor_mut().set_suffix("     ");
    entry.insert_formatted(&binary_key, value(config_path(binary)));
    entry.insert("config_dir", value(config_path(config_dir)));
    entry
}

/// A path as people write it in the config: `~/…` under the home folder
/// (except on Windows, where `%APPDATA%`-style absolute paths are the norm).
pub fn config_path(path: &Path) -> String {
    if !cfg!(windows)
        && let Some(rest) =
            dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(|r| r.to_owned()))
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Write through a temp file and rename, so the config is never half written
/// (and hot reload never reads a half-written file). On Unix only the user
/// may read it, since it holds the RomM token; on Windows the folder it is in
/// (the user's AppData) already is private.
pub fn write_replacing(path: &Path, text: &str) -> Result<()> {
    let staged = path.with_extension("toml.rommd-tmp");
    let _ = std::fs::remove_file(&staged);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&staged)
        .with_context(|| format!("writing {}", staged.display()))?;
    std::io::Write::write_all(&mut file, text.as_bytes())
        .with_context(|| format!("writing {}", staged.display()))?;
    drop(file);
    std::fs::rename(&staged, path).with_context(|| format!("replacing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn add(text: &str, id: &str, binary: &str, config_dir: &str) -> Result<String> {
        with_emulator(
            text,
            Path::new("config.toml"),
            id,
            &PathBuf::from(binary),
            &PathBuf::from(config_dir),
        )
    }

    #[test]
    fn adds_after_the_last_emulator_and_keeps_everything_else() {
        let text = r#"# my config

[romm]
base_url = "https://romm.example"   # my server

[emulators.dolphin]
binary     = "/Applications/Dolphin.app/Contents/MacOS/Dolphin"
config_dir = "~/Library/Application Support/Dolphin"

# Switch: I prefer Eden
[platforms]
switch = "eden"
"#;
        let out = add(
            text,
            "azahar",
            "/Applications/Azahar.app/Contents/MacOS/azahar",
            "/data/Azahar",
        )
        .unwrap();
        assert_eq!(
            out,
            r#"# my config

[romm]
base_url = "https://romm.example"   # my server

[emulators.dolphin]
binary     = "/Applications/Dolphin.app/Contents/MacOS/Dolphin"
config_dir = "~/Library/Application Support/Dolphin"

[emulators.azahar]
binary     = "/Applications/Azahar.app/Contents/MacOS/azahar"
config_dir = "/data/Azahar"

# Switch: I prefer Eden
[platforms]
switch = "eden"
"#
        );
    }

    #[test]
    fn adds_at_the_end_when_there_is_no_emulator_yet() {
        let text = "[server]\nport = 7497\n\n# [emulators.dolphin]\n# binary = \"…\"\n";
        let out = add(text, "mgba", "/bin/mgba", "/data/mgba").unwrap();
        assert_eq!(
            out,
            "[server]\nport = 7497\n\n# [emulators.dolphin]\n# binary = \"…\"\n\n\
             [emulators.mgba]\nbinary     = \"/bin/mgba\"\nconfig_dir = \"/data/mgba\"\n"
        );
    }

    #[test]
    fn comments_after_the_last_emulator_stay_after_the_new_one() {
        let text = "[emulators.dolphin]\nbinary = \"/d\"\nconfig_dir = \"/d\"\n\n# old notes\n";
        let out = add(text, "mgba", "/m", "/m").unwrap();
        assert!(
            out.ends_with(
                "[emulators.mgba]\nbinary     = \"/m\"\nconfig_dir = \"/m\"\n\n# old notes\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn adds_to_the_bottom_of_the_generated_config() {
        let generated = crate::config::default_config(crate::config::Os::current(), &[]);
        let template = generated.as_str();
        let out = add(template, "dolphin", "/d", "/d").unwrap();
        let (kept, added) = out.split_at(template.trim_end().len());
        assert_eq!(kept, template.trim_end(), "the template is kept as written");
        assert_eq!(
            added.trim_start(),
            "[emulators.dolphin]\nbinary     = \"/d\"\nconfig_dir = \"/d\"\n"
        );
    }

    #[test]
    fn writes_home_paths_with_a_tilde_and_backslashes_readably() {
        let home = dirs::home_dir().unwrap();
        let out = add(
            "",
            "dolphin",
            r"C:\Program Files\Dolphin\Dolphin.exe",
            &home
                .join("Library/Application Support/Dolphin")
                .display()
                .to_string(),
        )
        .unwrap();
        assert!(
            out.contains(r"binary     = 'C:\Program Files\Dolphin\Dolphin.exe'"),
            "a literal string, not escaped backslashes: {out}"
        );
        if !cfg!(windows) {
            assert!(
                out.contains(r#"config_dir = "~/Library/Application Support/Dolphin""#),
                "{out}"
            );
        }
    }

    #[test]
    fn never_overwrites_an_entry_or_repairs_a_broken_file() {
        let text = "[emulators.dolphin]\nbinary = \"/mine\"\nconfig_dir = \"/mine\"\n";
        let err = add(text, "dolphin", "/other", "/other").unwrap_err();
        assert!(format!("{err:#}").contains("already has"), "{err:#}");
        assert!(add("[server\nport = 1", "mgba", "/bin/mgba", "/d").is_err());
    }

    #[test]
    fn writes_the_file_in_place() {
        let dir = crate::testing::TempDir::new("config-edit");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# keep me\n").unwrap();
        add_emulator(&path, "mgba", Path::new("/bin/mgba"), Path::new("/d")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# keep me\n") && text.contains("[emulators.mgba]"));
        assert!(!dir.path().join("config.toml.rommd-tmp").exists());
    }

    fn settings(url: &str, token: &str) -> Settings {
        Settings {
            romm: Some((url.into(), token.into())),
            ..Default::default()
        }
    }

    #[test]
    fn fills_in_the_generated_configs_examples_where_they_are() {
        let generated = crate::config::default_config(crate::config::Os::current(), &[]);
        for example in [ROMM_EXAMPLE, TOKEN_EXAMPLE, ROMS_DIR_EXAMPLE] {
            assert!(generated.contains(example), "the template has {example:?}");
        }
        let dir = if cfg!(windows) { r"D:\Games" } else { "/games" };
        let out = with_settings(
            &generated,
            Path::new("config.toml"),
            &Settings {
                romm: Some(("https://romm.example".into(), "tok\"en".into())),
                roms_dir: Some(PathBuf::from(dir)),
                update_mode: Some(UpdateMode::Notify),
            },
        )
        .unwrap();
        assert!(
            out.contains("[romm]\nbase_url = \"https://romm.example\"\n#\n# API token"),
            "in place of the example, before its explanation: {out}"
        );
        assert!(out.contains("# Or authenticate with username and password:"));
        assert!(out.contains("\nmode = \"notify\"\n"), "{out}");
        assert!(!out.contains(ROMS_DIR_EXAMPLE));
        let config = Config::parse(&out, Path::new("config.toml")).unwrap();
        let romm = config.romm.unwrap();
        assert_eq!(romm.api_token.as_deref(), Some("tok\"en"));
        assert_eq!(config.roms_dir, Some(PathBuf::from(dir)));
        assert_eq!(
            out.lines().count(),
            generated.lines().count(),
            "filled in, nothing added"
        );
    }

    #[test]
    fn changes_existing_settings_and_keeps_the_rest() {
        let text = "[romm]\nbase_url = \"http://old\"   # my NAS\nusername = \"me\"\npassword = \"pw\"\n\n[updates]\nmode = \"off\"\n";
        let out = with_settings(text, Path::new("c.toml"), &settings("https://new", "t")).unwrap();
        assert_eq!(
            out,
            "[romm]\nbase_url = \"https://new\"   # my NAS\nusername = \"me\"\npassword = \"pw\"\napi_token = \"t\"\n\n[updates]\nmode = \"off\"\n"
        );
    }

    #[test]
    fn adds_romm_to_a_config_without_it() {
        let text = "# mine\n[server]\nport = 7497\n";
        let out = with_settings(text, Path::new("c.toml"), &settings("https://r", "t")).unwrap();
        assert!(out.starts_with(text), "{out}");
        let config = Config::parse(&out, Path::new("c.toml")).unwrap();
        assert_eq!(config.romm.unwrap().base_url, "https://r");
        assert!(with_settings("[romm\n", Path::new("c.toml"), &settings("a", "b")).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn only_the_user_may_read_the_written_config() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testing::TempDir::new("config-private");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_replacing(&path, "[romm]\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
