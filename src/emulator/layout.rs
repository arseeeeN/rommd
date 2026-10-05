//! Where a game's saves and save states live on disk — what each emulator
//! reports through `Emulator::save_layout` and the sync engine works from.

use std::io::Read;
use std::path::{Path, PathBuf};

/// A game's saves and save states. `None` for either means that kind is not
/// synced for this game (unsupported, or the location can't be determined).
#[derive(Debug, Clone, Default)]
pub struct SaveLayout {
    pub saves: Option<SaveSet>,
    pub states: Option<FileSet>,
}

impl SaveLayout {
    /// Prefer this layout's parts, falling back to `other`'s where this one has
    /// none. Used after exit: the layout found then is more complete (e.g.
    /// DuckStation's title cache or Ryujinx's save slot now exist), but a part
    /// that can no longer be resolved is still synced from the launch layout.
    pub fn or(self, other: SaveLayout) -> SaveLayout {
        SaveLayout {
            saves: self.saves.or(other.saves),
            states: self.states.or(other.states),
        }
    }
}

/// How a game's saves are stored, and so how they map onto RomM entries.
#[derive(Debug, Clone)]
pub enum SaveSet {
    /// Directory trees synced as one zip, each under its own root folder
    /// (Switch, Wii; 3DS with its title data and extdata).
    Dir(GameSaveDir),
    /// Individual files, each synced as its own RomM entry (memory cards,
    /// battery saves).
    Files(FileSet),
    /// Several files synced together as one flat zip (GameCube `.gci` saves).
    Bundle(FileSet),
}

/// A game's save stored as whole directories.
#[derive(Debug, Clone)]
pub struct GameSaveDir {
    /// Directory whose contents are the game's save.
    pub dir: PathBuf,
    /// Root folder name inside the uploaded zip for `dir`.
    pub archive_root: String,
    /// Further directories archived alongside `dir`, each under its own root
    /// folder: (root, directory). A restored zip may only contain these roots
    /// and `archive_root`; each root it contains replaces its directory, and a
    /// root it lacks leaves that directory untouched.
    pub extra_roots: Vec<(String, PathBuf)>,
    /// Directories that must receive the same contents when a save is restored
    /// (Ryujinx's working copy `1/` alongside the committed `0/`).
    pub mirrors: Vec<PathBuf>,
    /// Subdirectory of `dir` that must hold files for `dir` to count as part
    /// of the save.
    /// A Wii title folder gets `content/title.tmd` on first boot, before the
    /// game has saved anything to `data/`.
    pub required_subdir: Option<&'static str>,
}

/// The files in one directory (not recursive) that belong to a game.
#[derive(Debug, Clone)]
pub struct FileSet {
    pub dir: PathBuf,
    pub matcher: FileMatcher,
    /// Extensions other clients store the same file format under, mapped to
    /// the local one, e.g. RomM's web player uploads GBA battery saves as
    /// `.srm` where mGBA uses `.sav`.
    pub remote_extensions: &'static [(&'static str, &'static str)],
}

impl FileSet {
    /// Files named after `stem`, or `None` when the stem is empty — an empty
    /// stem would attribute every file in the directory to this game.
    pub fn stem(dir: PathBuf, stem: impl Into<String>) -> Option<Self> {
        let stem = stem.into();
        (!stem.is_empty()).then_some(Self { dir, matcher: FileMatcher::Stem(stem), remote_extensions: &[] })
    }

    pub fn gci(dir: PathBuf, game_id: impl Into<String>) -> Self {
        Self { dir, matcher: FileMatcher::Gci(game_id.into()), remote_extensions: &[] }
    }

    pub fn with_remote_extensions(mut self, map: &'static [(&'static str, &'static str)]) -> Self {
        self.remote_extensions = map;
        self
    }

    /// The local file name for a file another client stored as `name`.
    pub fn local_name(&self, name: &str) -> String {
        if let Some((stem, ext)) = name.rsplit_once('.') {
            for (remote, local) in self.remote_extensions {
                if ext.eq_ignore_ascii_case(remote) {
                    return format!("{}.{}", stem, local);
                }
            }
        }
        name.to_owned()
    }

    /// The matching files present now, sorted.
    pub fn files(&self) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file())
            .filter(|p| {
                let name = p.file_name().unwrap_or_default().to_string_lossy();
                !name.starts_with('.') && self.matcher.matches_name(&name) && self.matcher.matches_file(p)
            })
            .collect();
        files.sort();
        files
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileMatcher {
    /// `<stem>` followed by `.` or `_`, or exactly `<stem>`: `Game.sav` and
    /// `Game_1.mcd` belong to "Game", `Game Plus.sav` does not.
    Stem(String),
    /// A GameCube save (`*.gci`) whose header starts with this 6-character game
    /// ID (4-character game code + 2-character maker code).
    Gci(String),
}

impl FileMatcher {
    /// Name-only check; enough to decide where a RomM entry would be restored.
    pub fn matches_name(&self, name: &str) -> bool {
        match self {
            FileMatcher::Stem(stem) => name
                .strip_prefix(stem.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', '_'])),
            FileMatcher::Gci(_) => name.to_ascii_lowercase().ends_with(".gci"),
        }
    }

    /// Full check of a file on disk, including its contents where the name is
    /// not enough.
    pub fn matches_file(&self, path: &Path) -> bool {
        match self {
            FileMatcher::Stem(_) => true,
            FileMatcher::Gci(id) => {
                let mut header = [0u8; 6];
                std::fs::File::open(path)
                    .and_then(|mut f| f.read_exact(&mut header))
                    .is_ok_and(|_| header == id.as_bytes())
            }
        }
    }

    /// Short description, part of the sync-state key for bundles.
    pub fn label(&self) -> String {
        match self {
            FileMatcher::Stem(stem) => stem.clone(),
            FileMatcher::Gci(id) => format!("gci:{}", id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_matches_on_a_boundary() {
        let m = FileMatcher::Stem("Mario".into());
        assert!(m.matches_name("Mario.sav"));
        assert!(m.matches_name("Mario_1.mcd"));
        assert!(m.matches_name("Mario"));
        assert!(!m.matches_name("Mario Kart.sav"));
        assert!(!m.matches_name("MarioKart.sav"));
        assert!(FileSet::stem(PathBuf::from("/x"), "").is_none());
    }

    #[test]
    fn maps_remote_extensions() {
        let set = FileSet::stem(PathBuf::from("/x"), "anguna").unwrap().with_remote_extensions(&[("srm", "sav")]);
        assert_eq!(set.local_name("anguna.srm"), "anguna.sav");
        assert_eq!(set.local_name("anguna.SRM"), "anguna.sav");
        assert_eq!(set.local_name("anguna.ss1"), "anguna.ss1");
    }

    #[test]
    fn gci_matches_by_header() {
        let dir = std::env::temp_dir().join(format!("rommd-gci-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("01-GALE-SuperSmashBros.gci"), b"GALE01\x00rest").unwrap();
        std::fs::write(dir.join("01-GALE-renamed-other.gci"), b"GMSE01\x00rest").unwrap();
        std::fs::write(dir.join("notes.txt"), b"GALE01").unwrap();
        let set = FileSet::gci(dir.clone(), "GALE01");
        assert_eq!(set.files(), vec![dir.join("01-GALE-SuperSmashBros.gci")]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
