//! Where a game's saves and save states live on disk — what each emulator
//! reports through `Emulator::save_layout` and the sync engine works from.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    /// An N64 cartridge's save files, synced as argosy-launcher's unit (`n64`).
    N64(N64Files),
}

/// A game's N64 save files (`.eep`, `.sra`, `.fla`, `.mpk`) in one folder.
#[derive(Debug, Clone)]
pub struct N64Files {
    pub set: FileSet,
    /// What a file the emulator hasn't created yet is named, without its
    /// extension; `None` when it can't be spelled. A file of the game
    /// already there gives the name first.
    pub new_stem: Option<String>,
    /// SRAM and flash are stored with each 32-bit word reversed (mupen64plus).
    pub word_swapped: bool,
}

/// A game's save stored as whole directories.
#[derive(Debug, Clone)]
pub struct GameSaveDir {
    /// Directory whose contents are the game's save.
    pub dir: PathBuf,
    /// Root folder inside the uploaded zip for `dir`; several segments for a
    /// folder deeper in the zip (a Wii save's `<code>/data`).
    pub archive_root: String,
    /// Further directories archived alongside `dir`, each under its own root
    /// folder: (root, directory). A restored zip may only contain these roots
    /// and `archive_root`; each root it contains replaces its directory, and a
    /// root it lacks leaves that directory untouched.
    pub extra_roots: Vec<(String, PathBuf)>,
    /// Directories that must receive the same contents when a save is restored
    /// (Ryujinx's working copy `1/` alongside the committed `0/`).
    pub mirrors: Vec<PathBuf>,
    /// Other prefixes a restored zip may spell a root with, as (root,
    /// prefix): argosy-launcher's Sigil units, which restore prefers when a
    /// zip has both. Uploads keep the roots above, the shape argosy's
    /// clients for RomM before 5.5 read.
    pub alternatives: Vec<(String, String)>,
    /// Prefixes of a restored zip that hold saves this one doesn't (a Switch
    /// account save's unit can also carry `device/<title>/`); skipped.
    pub ignored: Vec<String>,
    /// Makes `dir` exist before a save is restored into it, for emulators
    /// where a save location is more than a folder (a Ryujinx slot must also
    /// be registered in its save index). Only called when `dir` is missing.
    pub create: Option<CreateDir>,
}

/// See `GameSaveDir::create`.
#[derive(Clone)]
pub struct CreateDir(pub Arc<dyn Fn() -> anyhow::Result<()> + Send + Sync>);

impl std::fmt::Debug for CreateDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CreateDir")
    }
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
        (!stem.is_empty()).then_some(Self {
            dir,
            matcher: FileMatcher::Stem(stem),
            remote_extensions: &[],
        })
    }

    /// Files named after `stem` that end in `.<extension>`, for emulators
    /// that keep other files with the same names beside them (PCSX2's
    /// `.p2s.backup`). `None` for an empty stem, as with `stem`.
    pub fn stem_with_extension(
        dir: PathBuf,
        stem: impl Into<String>,
        extension: &'static str,
    ) -> Option<Self> {
        Self::stem(dir, stem).map(|mut set| {
            if let FileMatcher::Stem(stem) = set.matcher {
                set.matcher = FileMatcher::StemExtension(stem, extension);
            }
            set
        })
    }

    /// Files whose names end in `tag` and one of `extensions`, for emulators
    /// that name a game's files after a ROM hash. `None` for an empty tag.
    pub fn tagged(
        dir: PathBuf,
        tag: impl Into<String>,
        extensions: &'static [&'static str],
    ) -> Option<Self> {
        let tag = tag.into();
        (!tag.is_empty()).then_some(Self {
            dir,
            matcher: FileMatcher::Tagged(tag, extensions),
            remote_extensions: &[],
        })
    }

    pub fn gci(dir: PathBuf, game_id: impl Into<String>) -> Self {
        Self {
            dir,
            matcher: FileMatcher::Gci(game_id.into()),
            remote_extensions: &[],
        }
    }

    /// The extension of a bundle that travels as one bare file when it holds
    /// a single file: a GameCube game's only save is a raw `.gci`, as
    /// argosy-launcher uploads it. `None` when it is always a zip.
    pub fn lone_extension(&self) -> Option<&'static str> {
        matches!(self.matcher, FileMatcher::Gci(_)).then_some("gci")
    }

    /// The name a bare file of this bundle (`lone_extension`) takes when
    /// restored: the game's file holding the same save, else the name Dolphin
    /// would give it.
    pub fn lone_name(&self, data: &[u8]) -> Option<String> {
        let FileMatcher::Gci(_) = self.matcher else {
            return None;
        };
        let identity = gci_identity(data)?;
        let existing = self
            .files()
            .into_iter()
            .find(|p| std::fs::read(p).is_ok_and(|d| gci_identity(&d) == Some(identity)));
        Some(match existing {
            Some(path) => path.file_name()?.to_string_lossy().into_owned(),
            None => gci_file_name(data),
        })
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
                !name.starts_with('.')
                    && self.matcher.matches_name(&name)
                    && self.matcher.matches_file(p)
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
    /// As `Stem`, and the name ends in `.<extension>` (case-insensitive).
    StemExtension(String, &'static str),
    /// `<anything><tag>.<extension>`, the extension one of these
    /// (case-insensitive): `Zelda-5BF45B7E.eep` for the tag `-5BF45B7E`.
    Tagged(String, &'static [&'static str]),
    /// A GameCube save (`*.gci`) whose header starts with the 4-character
    /// game code of this game ID (the disc's 6-character ID): a game's saves,
    /// as argosy-launcher and its Sigil group them, whatever their maker code.
    Gci(String),
}

impl FileMatcher {
    /// Name-only check; enough to decide where a RomM entry would be restored.
    pub fn matches_name(&self, name: &str) -> bool {
        match self {
            FileMatcher::Stem(stem) => name
                .strip_prefix(stem.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', '_'])),
            FileMatcher::StemExtension(stem, ext) => {
                FileMatcher::Stem(stem.clone()).matches_name(name)
                    && name
                        .rsplit_once('.')
                        .is_some_and(|(_, e)| e.eq_ignore_ascii_case(ext))
            }
            FileMatcher::Tagged(tag, extensions) => {
                name.rsplit_once('.').is_some_and(|(stem, ext)| {
                    stem.ends_with(tag.as_str())
                        && extensions.iter().any(|e| e.eq_ignore_ascii_case(ext))
                })
            }
            FileMatcher::Gci(_) => name.to_ascii_lowercase().ends_with(".gci"),
        }
    }

    /// Full check of a file on disk, including its contents where the name is
    /// not enough.
    pub fn matches_file(&self, path: &Path) -> bool {
        match self {
            FileMatcher::Stem(_) | FileMatcher::StemExtension(..) | FileMatcher::Tagged(..) => true,
            FileMatcher::Gci(id) => {
                let mut header = [0u8; 4];
                std::fs::File::open(path)
                    .and_then(|mut f| f.read_exact(&mut header))
                    .is_ok_and(|_| id.as_bytes().get(..4) == Some(&header[..]))
            }
        }
    }

    /// Short description, part of the sync-state key for bundles.
    pub fn label(&self) -> String {
        match self {
            FileMatcher::Stem(stem) => stem.clone(),
            FileMatcher::StemExtension(stem, ext) => format!("{}.{}", stem, ext),
            FileMatcher::Tagged(tag, _) => format!("*{}", tag),
            FileMatcher::Gci(id) => format!("gci:{}", id),
        }
    }
}

/// What tells two GameCube saves apart: game and maker code, and file name.
fn gci_identity(gci: &[u8]) -> Option<(&[u8], &[u8])> {
    Some((gci.get(..6)?, gci.get(0x08..0x28)?))
}

/// The file name Dolphin gives a `.gci` (`GCMemcardUtils::GenerateFilename`,
/// escaped as `NandPaths` `EscapeFileName` does): `<maker>-<game code>-<file
/// name>.gci`, with `__` doubled and characters a file name can't hold as
/// `__xx__`. Bytes above 0x7E are escaped too, as Sigil does (Dolphin
/// decodes them); Dolphin loads every `.gci` in the folder whatever its name.
fn gci_file_name(gci: &[u8]) -> String {
    let mut raw = Vec::with_capacity(0x28);
    raw.extend_from_slice(&gci[4..6]);
    raw.push(b'-');
    raw.extend_from_slice(&gci[..4]);
    raw.push(b'-');
    raw.extend(gci[0x08..0x28].iter().take_while(|&&b| b != 0));
    let mut out = String::new();
    let mut i = 0;
    while i < raw.len() {
        let c = raw[i];
        if c == b'_' && raw.get(i + 1) == Some(&b'_') {
            out.push_str("__5f____5f__");
            i += 1;
        } else if c <= 0x1F || c >= 0x7F || b"\"*/:<>?\\|".contains(&c) {
            out.push_str(&format!("__{c:02x}__"));
        } else {
            out.push(c as char);
        }
        i += 1;
    }
    out + ".gci"
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

        let m = FileMatcher::StemExtension("SLUS-20811 (CB99CD12)".into(), "p2s");
        assert!(m.matches_name("SLUS-20811 (CB99CD12).01.p2s"));
        assert!(m.matches_name("SLUS-20811 (CB99CD12).resume.p2s"));
        assert!(!m.matches_name("SLUS-20811 (CB99CD12).01.p2s.backup"));
        assert!(!m.matches_name("SLUS-20811 (00000000).01.p2s"));

        let m = FileMatcher::Tagged("-5BF45B7E".into(), &["eep", "mpk"]);
        assert!(m.matches_name("Legend of Zelda, The - Ocarina-5BF45B7E.eep"));
        assert!(m.matches_name("ZELDA-5BF45B7E.MPK"));
        assert!(!m.matches_name("ZELDA-5BF45B7E.st0"));
        assert!(!m.matches_name("ZELDA-5BF45B7F.eep"));
        assert!(FileSet::tagged(PathBuf::from("/x"), "", &["eep"]).is_none());
    }

    #[test]
    fn maps_remote_extensions() {
        let set = FileSet::stem(PathBuf::from("/x"), "anguna")
            .unwrap()
            .with_remote_extensions(&[("srm", "sav")]);
        assert_eq!(set.local_name("anguna.srm"), "anguna.sav");
        assert_eq!(set.local_name("anguna.SRM"), "anguna.sav");
        assert_eq!(set.local_name("anguna.ss1"), "anguna.ss1");
    }

    #[test]
    fn a_lone_gci_takes_dolphins_name_or_its_own_files() {
        let mut gci = vec![0u8; 0x40];
        gci[..6].copy_from_slice(b"GALE01");
        gci[0x08..0x08 + 16].copy_from_slice(b"SuperSmash__Bros");
        assert_eq!(
            gci_file_name(&gci),
            "01-GALE-SuperSmash__5f____5f__Bros.gci"
        );
        gci[0x08..0x0D].copy_from_slice(b"a/b:c");
        assert!(gci_file_name(&gci).starts_with("01-GALE-a__2f__b__3a__c"));

        let dir = std::env::temp_dir().join(format!("rommd-gci-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("renamed.gci"), &gci).unwrap();
        let set = FileSet::gci(dir.clone(), "GALE01");
        assert_eq!(set.lone_name(&gci).as_deref(), Some("renamed.gci"));
        gci[0x08] = b'z';
        assert!(set.lone_name(&gci).unwrap().starts_with("01-GALE-z"));
        let _ = std::fs::remove_dir_all(&dir);
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

        // A save of the game with another maker code is still the game's.
        std::fs::write(dir.join("other-maker.gci"), b"GALE8P\x00rest").unwrap();
        assert_eq!(set.files().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
