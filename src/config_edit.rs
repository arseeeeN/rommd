//! Changes rommd makes to the user's config file. The file is for people
//! first, so an edit keeps every comment, blank line and value it doesn't
//! touch exactly as written (`toml_edit`).
//!
//! rommd only edits when asked (`rommd detect --save`), never in the
//! background: the user may have the file open, and a silent write is how
//! their work gets lost.

use anyhow::{Context, Result};
use std::path::Path;
use toml_edit::{DocumentMut, Item, Key, Table, value};

use crate::config::Config;

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

    // Aligned like the examples in the generated config.
    let mut entry = Table::new();
    let mut binary_key = Key::new("binary");
    binary_key.leaf_decor_mut().set_suffix("     ");
    entry.insert_formatted(&binary_key, value(config_path(binary)));
    entry.insert("config_dir", value(config_path(config_dir)));
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

/// A path as people write it in the config: `~/…` under the home folder
/// (except on Windows, where `%APPDATA%`-style absolute paths are the norm).
fn config_path(path: &Path) -> String {
    if !cfg!(windows)
        && let Some(rest) =
            dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(|r| r.to_owned()))
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Write through a temp file and rename, so the config is never half written
/// (and hot reload never reads a half-written file).
fn write_replacing(path: &Path, text: &str) -> Result<()> {
    let staged = path.with_extension("toml.rommd-tmp");
    std::fs::write(&staged, text).with_context(|| format!("writing {}", staged.display()))?;
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
        let template = crate::config::DEFAULT_CONFIG_TEMPLATE;
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
}
