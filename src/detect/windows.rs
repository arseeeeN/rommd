//! Windows: emulators come as installers, zips extracted anywhere, winget
//! and Scoop packages. A candidate is any program with the emulator's file
//! name in a place programs live; it is recognised from its own file, never
//! by running it:
//!
//! - by the `ProductName` in its version resource, for builds that carry one;
//! - otherwise by the project's web address in the file (Dolphin, Azahar and
//!   Eden ship no version resource, but their programs link to their sites).
//!
//! Uninstall entries and shortcuts only point at candidates; they never
//! identify one. The data folder follows each emulator's own rules: a
//! portable marker next to the program wins over AppData.
//!
//! Everything but the registry access builds on every OS, so the scan's
//! tests run everywhere.

#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{Detection, Found, Kind, sorted_dirs, sorted_entries};
use crate::emulator::Emulator;

/// How an emulator ships on Windows.
#[derive(Debug, Clone, Copy)]
pub struct WindowsApp {
    /// The program's file name, compared ignoring case; a `*` stands for any
    /// text (DuckStation names its builds per CPU).
    pub executables: &'static [&'static str],
    /// What the program file must contain to be this emulator.
    pub mark: Mark,
    pub data_dir: WindowsDataDir,
}

#[derive(Debug, Clone, Copy)]
pub enum Mark {
    /// `ProductName` in the program's version resource.
    Product(&'static str),
    /// Text in the program file, for builds without a version resource.
    Text(&'static str),
}

/// Where the emulator keeps its data (`config_dir`), in order: portable next
/// to the program, then `registry` (Dolphin's scheme), then a folder in
/// Documents that older versions used if it exists, then AppData.
#[derive(Debug, Clone, Copy)]
pub struct WindowsDataDir {
    pub portable: Portable,
    /// A key under `HKEY_CURRENT_USER` with Dolphin's values:
    /// `LocalUserConfig` (nonzero: portable) and `UserConfigPath`.
    pub registry: Option<&'static str>,
    pub documents: Option<&'static str>,
    pub app_data: AppData,
}

#[derive(Debug, Clone, Copy)]
pub enum Portable {
    /// The program's own folder, when one of these files is next to it.
    Here(&'static [&'static str]),
    /// This folder next to the program, when it exists.
    Folder(&'static str),
    /// This folder next to the program, when one of these files is there
    /// (or the registry says so).
    FolderIf(&'static str, &'static [&'static str]),
}

#[derive(Debug, Clone, Copy)]
pub enum AppData {
    /// `%APPDATA%\<name>`.
    Roaming(&'static str),
    /// `%LOCALAPPDATA%\<name>`.
    Local(&'static str),
}

/// Dolphin's registry values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserDirKey {
    pub local: bool,
    pub path: Option<PathBuf>,
}

/// Where a scan looks: from the environment, or a test's temp tree.
pub struct Places {
    roaming: PathBuf,
    local: PathBuf,
    documents: PathBuf,
    /// Not install folders: a program here is easily moved or deleted.
    transient: Vec<PathBuf>,
    /// Program Files and `%LOCALAPPDATA%\Programs`, where installers and
    /// people put programs.
    program_dirs: Vec<PathBuf>,
    /// winget's portable packages.
    winget: Vec<PathBuf>,
    /// Scoop's `apps` folders.
    scoop: Vec<PathBuf>,
    /// Folders the uninstall entries name.
    uninstall: Vec<PathBuf>,
    /// Start Menu folders (searched with subfolders) and Desktops (without).
    shortcuts: Vec<(PathBuf, bool)>,
    /// Desktop, Downloads and Documents, searched with `everywhere`.
    elsewhere: Vec<PathBuf>,
    /// Registry values of the emulators that keep their folder there.
    registry: Vec<(&'static str, UserDirKey)>,
}

/// How deep below Program Files programs are looked for: a folder holding
/// the program, sometimes in a subfolder (Ryujinx's `publish\`), or a folder
/// grouping emulators.
const PROGRAM_DEPTH: usize = 2;
/// Desktop, Downloads and Documents also hold zips extracted into a folder
/// named after the zip, with the emulator's own folder inside.
const ELSEWHERE_DEPTH: usize = 3;

impl Places {
    #[cfg(not(windows))]
    fn from_env(_emulators: &[Arc<dyn Emulator>]) -> Option<Self> {
        None
    }

    #[cfg(windows)]
    fn from_env(emulators: &[Arc<dyn Emulator>]) -> Option<Self> {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let roaming = dirs::config_dir()?;
        let local = dirs::data_local_dir()?;
        let home = dirs::home_dir()?;
        let program_data = var("ProgramData").unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        let mut program_dirs: Vec<PathBuf> = Vec::new();
        for dir in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
            .into_iter()
            .filter_map(var)
        {
            if !program_dirs
                .iter()
                .any(|d| under(d, &dir) && under(&dir, d))
            {
                program_dirs.push(dir);
            }
        }
        let winget: Vec<PathBuf> = std::iter::once(local.join(r"Microsoft\WinGet\Packages"))
            .chain(program_dirs.first().map(|p| p.join(r"WinGet\Packages")))
            .collect();
        program_dirs.push(local.join("Programs"));
        let scoop = vec![
            var("SCOOP")
                .unwrap_or_else(|| home.join("scoop"))
                .join("apps"),
            var("SCOOP_GLOBAL")
                .unwrap_or_else(|| program_data.join("scoop"))
                .join("apps"),
        ];
        let desktop = dirs::desktop_dir();
        let downloads = dirs::download_dir();
        let documents = dirs::document_dir().unwrap_or_else(|| home.join("Documents"));
        let start_menu = r"Microsoft\Windows\Start Menu\Programs";
        let mut shortcuts = vec![
            (roaming.join(start_menu), true),
            (program_data.join(start_menu), true),
        ];
        shortcuts.extend(desktop.iter().map(|d| (d.clone(), false)));
        if let Some(public) = var("PUBLIC") {
            shortcuts.push((public.join("Desktop"), false));
        }
        let mut registry = Vec::new();
        for key in emulators
            .iter()
            .filter_map(|e| e.windows_app()?.data_dir.registry)
        {
            registry.push((key, registry::user_dir_key(key)));
        }
        Some(Places {
            transient: desktop
                .iter()
                .chain(downloads.iter())
                .cloned()
                .chain([std::env::temp_dir()])
                .collect(),
            elsewhere: desktop
                .into_iter()
                .chain(downloads)
                .chain([documents.clone()])
                .collect(),
            uninstall: registry::uninstall_folders(),
            roaming,
            local,
            documents,
            program_dirs,
            winget,
            scoop,
            shortcuts,
            registry,
        })
    }

    fn app_data(&self, dir: AppData) -> PathBuf {
        match dir {
            AppData::Roaming(name) => self.roaming.join(name),
            AppData::Local(name) => self.local.join(name),
        }
    }

    fn installed(&self, program: &Path) -> bool {
        !self.transient.iter().any(|t| under(program, t))
            && !program
                .components()
                .any(|c| c.as_os_str().eq_ignore_ascii_case("$Recycle.Bin"))
    }

    /// How the program got where it is.
    fn kind(&self, program: &Path) -> Kind {
        if self.winget.iter().any(|d| under(program, d)) {
            Kind::Winget
        } else if self.scoop.iter().any(|d| under(program, d)) {
            Kind::Scoop
        } else if self.uninstall.iter().any(|d| under(program, d)) {
            Kind::Installer
        } else {
            Kind::Folder
        }
    }
}

pub fn detect(emulators: &[Arc<dyn Emulator>], everywhere: bool) -> Detection {
    match Places::from_env(emulators) {
        Some(places) => scan(&places, emulators, everywhere),
        None => Detection::default(),
    }
}

pub fn detect_at(path: &Path, emulators: &[Arc<dyn Emulator>]) -> Detection {
    match Places::from_env(emulators) {
        Some(places) => scan_at(&places, path, emulators),
        None => Detection::default(),
    }
}

/// In order of preference: what the uninstall entries name, Program Files,
/// winget and Scoop packages, shortcut targets; with `everywhere` also
/// Desktop, Downloads and Documents.
pub fn scan(places: &Places, emulators: &[Arc<dyn Emulator>], everywhere: bool) -> Detection {
    let mut programs = Vec::new();
    // Installers put the program in the folder they register.
    for dir in &places.uninstall {
        programs.extend(programs_in(dir, 0, emulators));
    }
    for dir in &places.program_dirs {
        programs.extend(programs_in(dir, PROGRAM_DEPTH, emulators));
    }
    for dir in &places.winget {
        programs.extend(programs_in(dir, 2, emulators));
    }
    // Only `current`, which Scoop keeps pointing at the installed version;
    // the version folders next to it go away with the next update.
    for apps in &places.scoop {
        for app in sorted_dirs(apps) {
            programs.extend(programs_in(&app.join("current"), 1, emulators));
        }
    }
    for (dir, recursive) in &places.shortcuts {
        for link in shortcuts(dir, *recursive) {
            if let Some(target) = std::fs::read(&link)
                .ok()
                .and_then(|b| shortcut_target(&b, &link))
                .filter(|t| program_name_matches(t, emulators))
            {
                programs.push(target);
            }
        }
    }
    if everywhere {
        for dir in &places.elsewhere {
            programs.extend(programs_in(dir, ELSEWHERE_DEPTH, emulators));
        }
    }
    identify_all(places, &programs, emulators)
}

/// What is at `path`, a program, a shortcut or a folder of them, which the
/// user named.
pub fn scan_at(places: &Places, path: &Path, emulators: &[Arc<dyn Emulator>]) -> Detection {
    let programs = if path.is_dir() {
        programs_in(path, ELSEWHERE_DEPTH, emulators)
    } else if is_shortcut(path) {
        std::fs::read(path)
            .ok()
            .and_then(|b| shortcut_target(&b, path))
            .into_iter()
            .collect()
    } else {
        vec![path.to_owned()]
    };
    identify_all(places, &programs, emulators)
}

fn identify_all(
    places: &Places,
    programs: &[PathBuf],
    emulators: &[Arc<dyn Emulator>],
) -> Detection {
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for program in programs {
        let key = program
            .canonicalize()
            .unwrap_or_else(|_| program.clone())
            .to_string_lossy()
            .to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        for emulator in emulators {
            let Some(app) = emulator.windows_app() else {
                continue;
            };
            if !name_matches(program, &app) || !has_mark(program, app.mark) {
                continue;
            }
            let Some(dir) = program.parent() else {
                continue;
            };
            found.push(Found::new(
                emulator.as_ref(),
                places.kind(program),
                dir.to_owned(),
                program.clone(),
                data_dir(&app, dir, places),
                places.installed(program),
            ));
        }
    }
    Detection::from_found(found)
}

/// Where the emulator at `dir` keeps its data, by its own rules.
fn data_dir(app: &WindowsApp, dir: &Path, places: &Places) -> PathBuf {
    let rules = &app.data_dir;
    let key = rules.registry.and_then(|k| {
        places
            .registry
            .iter()
            .find(|(name, _)| *name == k)
            .map(|(_, v)| v)
    });
    match rules.portable {
        Portable::Here(files) if files.iter().any(|f| dir.join(f).is_file()) => {
            return dir.to_owned();
        }
        Portable::Folder(name) if dir.join(name).is_dir() => return dir.join(name),
        Portable::FolderIf(name, files)
            if key.is_some_and(|k| k.local) || files.iter().any(|f| dir.join(f).is_file()) =>
        {
            return dir.join(name);
        }
        _ => {}
    }
    if let Some(path) = key.and_then(|k| k.path.clone()) {
        return path;
    }
    if let Some(name) = rules.documents {
        let legacy = places.documents.join(name);
        if legacy.is_dir() {
            return legacy;
        }
    }
    places.app_data(rules.app_data)
}

/// Programs named like an emulator's in `dir` and up to `depth` folders
/// below it. The listing says what is a folder, so nothing else is read;
/// links and junctions aren't followed.
fn programs_in(dir: &Path, depth: usize, emulators: &[Arc<dyn Emulator>]) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<(PathBuf, bool)> = entries
        .flatten()
        .filter_map(|e| Some((e.path(), e.file_type().ok()?.is_dir())))
        .collect();
    entries.sort();
    let mut programs = Vec::new();
    for (entry, is_dir) in entries {
        if is_dir {
            if depth > 0 {
                programs.extend(programs_in(&entry, depth - 1, emulators));
            }
        } else if program_name_matches(&entry, emulators) {
            programs.push(entry);
        }
    }
    programs
}

fn program_name_matches(path: &Path, emulators: &[Arc<dyn Emulator>]) -> bool {
    emulators
        .iter()
        .filter_map(|e| e.windows_app())
        .any(|app| name_matches(path, &app))
}

fn name_matches(path: &Path, app: &WindowsApp) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    app.executables.iter().any(|pattern| {
        let pattern = pattern.to_ascii_lowercase();
        match pattern.split_once('*') {
            Some((start, end)) => {
                name.len() >= start.len() + end.len()
                    && name.starts_with(start)
                    && name.ends_with(end)
            }
            None => name == pattern,
        }
    })
}

fn has_mark(program: &Path, mark: Mark) -> bool {
    match mark {
        Mark::Product(product) => version_strings(program)
            .is_some_and(|s| s.iter().any(|(k, v)| k == "ProductName" && v == product)),
        Mark::Text(text) => file_contains(program, text.as_bytes()),
    }
}

/// Whether `dir` is `path` or one of its ancestors, ignoring case as
/// Windows does.
fn under(path: &Path, dir: &Path) -> bool {
    let mut parts = path.components();
    dir.components().all(|d| {
        parts
            .next()
            .is_some_and(|p| p.as_os_str().eq_ignore_ascii_case(d.as_os_str()))
    })
}

fn is_shortcut(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
}

fn shortcuts(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut links = Vec::new();
    for entry in sorted_entries(dir) {
        if entry.is_dir() {
            if recursive {
                links.extend(shortcuts(&entry, true));
            }
        } else if is_shortcut(&entry) {
            links.push(entry);
        }
    }
    links
}

fn file_contains(path: &Path, needle: &[u8]) -> bool {
    const CHUNK: usize = 4 << 20;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let finder = memchr::memmem::Finder::new(needle);
    let mut buf = vec![0u8; CHUNK + needle.len()];
    let mut kept = 0;
    loop {
        let Ok(n) = file.read(&mut buf[kept..]) else {
            return false;
        };
        if n == 0 {
            return false;
        }
        let len = kept + n;
        if finder.find(&buf[..len]).is_some() {
            return true;
        }
        // Keep the tail, so text split across two reads is still found.
        kept = (needle.len() - 1).min(len);
        buf.copy_within(len - kept..len, 0);
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The `StringFileInfo` values of a program's version resource
/// (`ProductName`, `OriginalFilename`, …), read from the file: the PE
/// headers lead to the resource section, and its directory to `RT_VERSION`.
pub fn version_strings(path: &Path) -> Option<Vec<(String, String)>> {
    const RT_VERSION: u32 = 16;
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; 64 * 1024];
    let n = file.read(&mut head).ok()?;
    head.truncate(n);
    let pe = u32_at(&head, 0x3C)? as usize;
    if head.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let coff = pe + 4;
    let sections = u16_at(&head, coff + 2)? as usize;
    let optional = coff + 20;
    let directories = match u16_at(&head, optional)? {
        0x10b => optional + 96,
        0x20b => optional + 112,
        _ => return None,
    };
    let resources_rva = u32_at(&head, directories + 2 * 8)?;
    if resources_rva == 0 {
        return None;
    }
    let table = optional + u16_at(&head, coff + 16)? as usize;
    let (section_rva, section) = (0..sections)
        .find_map(|i| {
            let s = table + i * 40;
            let virtual_size = u32_at(&head, s + 8)?;
            let rva = u32_at(&head, s + 12)?;
            let raw_size = u32_at(&head, s + 16)?;
            let raw_at = u32_at(&head, s + 20)?;
            let contains = resources_rva >= rva && resources_rva - rva < virtual_size.max(raw_size);
            contains.then_some((rva, raw_at, raw_size))
        })
        .map(|(rva, raw_at, raw_size)| -> Option<(u32, Vec<u8>)> {
            if raw_size > 64 << 20 {
                return None;
            }
            let mut section = vec![0u8; raw_size as usize];
            file.seek(SeekFrom::Start(raw_at.into())).ok()?;
            file.read_exact(&mut section).ok()?;
            Some((rva, section))
        })??;

    let root = (resources_rva - section_rva) as usize;
    // The offset of the entry with `id` (any entry for `None`) in the
    // resource directory at `dir`; directories set the high bit.
    let entry = |dir: usize, id: Option<u32>| -> Option<u32> {
        let count = u16_at(&section, dir + 12)? as usize + u16_at(&section, dir + 14)? as usize;
        (0..count).find_map(|i| {
            let e = dir + 16 + i * 8;
            let name = u32_at(&section, e)?;
            id.is_none_or(|id| name == id)
                .then(|| u32_at(&section, e + 4))
                .flatten()
        })
    };
    let subdir =
        |offset: u32| (offset & 0x8000_0000 != 0).then(|| root + (offset & 0x7FFF_FFFF) as usize);
    let types = subdir(entry(root, Some(RT_VERSION))?)?;
    let languages = subdir(entry(types, None)?)?;
    let data_entry = root + entry(languages, None)? as usize;
    let data_rva = u32_at(&section, data_entry)?;
    let size = u32_at(&section, data_entry + 4)? as usize;
    let start = data_rva.checked_sub(section_rva)? as usize;
    Some(version_info_strings(section.get(start..start + size)?))
}

/// The `String` entries of every `StringTable` in a `VS_VERSIONINFO`.
fn version_info_strings(info: &[u8]) -> Vec<(String, String)> {
    struct Block<'a> {
        key: String,
        value: &'a [u8],
        children: &'a [u8],
    }
    fn align(at: usize) -> usize {
        (at + 3) & !3
    }
    fn utf16(bytes: &[u8]) -> String {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .take_while(|&u| u != 0)
            .collect();
        String::from_utf16_lossy(&units)
    }
    fn block(data: &[u8]) -> Option<(Block<'_>, usize)> {
        let len = (u16_at(data, 0)? as usize).min(data.len());
        let value_len = u16_at(data, 2)? as usize;
        let text = u16_at(data, 4)? == 1;
        let key_end = (6..len).step_by(2).find(|&i| u16_at(data, i) == Some(0))?;
        let value_at = align(key_end + 2);
        let value_end = (value_at + if text { value_len * 2 } else { value_len }).min(len);
        let children_at = align(value_end).min(len);
        Some((
            Block {
                key: utf16(&data[6..key_end]),
                value: data.get(value_at..value_end).unwrap_or_default(),
                children: &data[children_at..len],
            },
            align(len).max(2),
        ))
    }
    fn children(mut data: &[u8]) -> Vec<Block<'_>> {
        let mut out = Vec::new();
        while let Some((b, len)) = block(data) {
            out.push(b);
            data = data.get(len..).unwrap_or_default();
        }
        out
    }
    let Some((root, _)) = block(info) else {
        return Vec::new();
    };
    children(root.children)
        .into_iter()
        .filter(|b| b.key == "StringFileInfo")
        .flat_map(|b| children(b.children))
        .flat_map(|table| children(table.children))
        .map(|s| (s.key, utf16(s.value)))
        .collect()
}

/// The program a `.lnk` shortcut points at: its local base path (with any
/// common path suffix), or else its relative path from the shortcut's folder,
/// which is stored as Unicode and so also covers paths the ANSI base path
/// can't spell.
pub fn shortcut_target(lnk: &[u8], link_path: &Path) -> Option<PathBuf> {
    const HAS_ID_LIST: u32 = 0x1;
    const HAS_LINK_INFO: u32 = 0x2;
    const HAS_NAME: u32 = 0x4;
    const HAS_RELATIVE_PATH: u32 = 0x8;
    const IS_UNICODE: u32 = 0x80;
    if u32_at(lnk, 0)? != 0x4C {
        return None;
    }
    let flags = u32_at(lnk, 0x14)?;
    let mut at = 0x4C;
    if flags & HAS_ID_LIST != 0 {
        at += 2 + u16_at(lnk, at)? as usize;
    }
    let mut base = None;
    if flags & HAS_LINK_INFO != 0 {
        let info = at;
        let info_size = u32_at(lnk, info)? as usize;
        let header_size = u32_at(lnk, info + 4)?;
        let info_flags = u32_at(lnk, info + 8)?;
        let ansi = |offset: u32| -> Option<String> {
            let start = info + offset as usize;
            let end = start + lnk.get(start..)?.iter().position(|&b| b == 0)?;
            let bytes = &lnk[start..end];
            bytes
                .is_ascii()
                .then(|| String::from_utf8_lossy(bytes).into_owned())
        };
        let unicode = |offset: u32| -> Option<String> {
            let start = info + offset as usize;
            let units: Vec<u16> = lnk
                .get(start..)?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .take_while(|&u| u != 0)
                .collect();
            Some(String::from_utf16_lossy(&units))
        };
        if info_flags & 0x1 != 0 {
            base = if header_size >= 0x24 {
                Some(unicode(u32_at(lnk, info + 28)?)? + &unicode(u32_at(lnk, info + 32)?)?)
            } else {
                ansi(u32_at(lnk, info + 16)?)
                    .zip(ansi(u32_at(lnk, info + 24)?))
                    .map(|(b, s)| b + &s)
            };
        }
        at += info_size;
    }
    if let Some(base) = base.filter(|b| !b.is_empty()) {
        return Some(PathBuf::from(base));
    }
    // The StringData that follow: NAME, then RELATIVE_PATH.
    if flags & HAS_RELATIVE_PATH == 0 {
        return None;
    }
    let wide = flags & IS_UNICODE != 0;
    let string = |at: &mut usize| -> Option<String> {
        let count = u16_at(lnk, *at)? as usize;
        let start = *at + 2;
        let text = if wide {
            let units: Vec<u16> = lnk
                .get(start..start + count * 2)?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            *at = start + count * 2;
            String::from_utf16_lossy(&units)
        } else {
            *at = start + count;
            String::from_utf8_lossy(lnk.get(start..start + count)?).into_owned()
        };
        Some(text)
    };
    if flags & HAS_NAME != 0 {
        string(&mut at)?;
    }
    let relative = string(&mut at)?;
    Some(resolve_dots(&link_path.parent()?.join(relative)))
}

/// `path` with its `.` and `..` parts resolved. Not `canonicalize`, whose
/// `\\?\C:\…` paths would end up in the config.
fn resolve_dots(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut resolved = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

#[cfg(windows)]
mod registry {
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_DWORD,
        RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW,
        RegOpenKeyExW,
    };

    use super::UserDirKey;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A string value (`REG_EXPAND_SZ` expanded).
    pub fn string(root: HKEY, key: &str, name: &str) -> Option<String> {
        let (key, name) = (wide(key), wide(name));
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
        let mut size = 0u32;
        // SAFETY: valid NUL-terminated strings; a null buffer asks for the size.
        let status = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                name.as_ptr(),
                flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS || size == 0 {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        // SAFETY: the buffer holds `size` bytes, as the call reported.
        let status = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                name.as_ptr(),
                flags,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let len = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    pub fn dword(root: HKEY, key: &str, name: &str) -> Option<u32> {
        let (key, name) = (wide(key), wide(name));
        let mut value = 0u32;
        let mut size = 4u32;
        // SAFETY: a 4-byte buffer for a DWORD.
        let status = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&raw mut value).cast(),
                &mut size,
            )
        };
        (status == ERROR_SUCCESS).then_some(value)
    }

    fn subkeys(root: HKEY, key: &str) -> Vec<String> {
        let key = wide(key);
        let mut handle: HKEY = std::ptr::null_mut();
        // SAFETY: a valid key name; the handle is closed below.
        if unsafe { RegOpenKeyExW(root, key.as_ptr(), 0, KEY_READ, &mut handle) } != ERROR_SUCCESS {
            return Vec::new();
        }
        let mut names = Vec::new();
        for index in 0.. {
            let mut buf = [0u16; 256];
            let mut len = buf.len() as u32;
            // SAFETY: the buffer holds `len` characters.
            let status = unsafe {
                RegEnumKeyExW(
                    handle,
                    index,
                    buf.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if status != ERROR_SUCCESS {
                break;
            }
            names.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        // SAFETY: opened above.
        unsafe { RegCloseKey(handle) };
        names
    }

    /// The folders installers registered for uninstalling: `InstallLocation`,
    /// else the folder of the program `DisplayIcon` names.
    pub fn uninstall_folders() -> Vec<PathBuf> {
        const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
        const UNINSTALL_32: &str =
            r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";
        let mut folders = Vec::new();
        for (root, key) in [
            (HKEY_CURRENT_USER, UNINSTALL),
            (HKEY_LOCAL_MACHINE, UNINSTALL),
            (HKEY_LOCAL_MACHINE, UNINSTALL_32),
        ] {
            for name in subkeys(root, key) {
                let entry = format!(r"{key}\{name}");
                let location = string(root, &entry, "InstallLocation")
                    .map(|l| PathBuf::from(l.trim().trim_matches('"')))
                    .filter(|l| !l.as_os_str().is_empty())
                    .or_else(|| {
                        let icon = string(root, &entry, "DisplayIcon")?;
                        let icon = icon.trim();
                        let path = icon
                            .rsplit_once(',')
                            .map_or(icon, |(p, _)| p)
                            .trim_matches('"');
                        Some(PathBuf::from(path).parent()?.to_owned())
                    });
                folders.extend(location.filter(|l| l.is_dir()));
            }
        }
        folders
    }

    pub fn user_dir_key(key: &str) -> UserDirKey {
        UserDirKey {
            local: dword(HKEY_CURRENT_USER, key, "LocalUserConfig").is_some_and(|v| v != 0),
            path: string(HKEY_CURRENT_USER, key, "UserConfigPath")
                .map(|p| p.replace('/', "\\"))
                .map(|p| PathBuf::from(p.trim_end_matches('\\')))
                .filter(|p| !p.as_os_str().is_empty()),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::emulator::{
        azahar::Azahar, dolphin::Dolphin, duckstation::DuckStation, eden::Eden, mgba::MGba,
    };
    use crate::testing::{TempDir, write};

    fn emulators() -> Vec<Arc<dyn Emulator>> {
        vec![
            Arc::new(DuckStation),
            Arc::new(MGba),
            Arc::new(Dolphin),
            Arc::new(Eden),
            Arc::new(Azahar),
        ]
    }

    fn utf16z(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn pad4(b: &mut Vec<u8>) {
        while !b.len().is_multiple_of(4) {
            b.push(0);
        }
    }

    /// A `VS_VERSIONINFO`-style block.
    fn block(key: &str, value: &[u8], value_len: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
        let mut b = vec![0, 0];
        b.extend(value_len.to_le_bytes());
        b.extend(u16::from(text).to_le_bytes());
        b.extend(utf16z(key));
        pad4(&mut b);
        b.extend(value);
        for child in children {
            pad4(&mut b);
            b.extend(child);
        }
        let len = b.len() as u16;
        b[..2].copy_from_slice(&len.to_le_bytes());
        b
    }

    /// A minimal 64-bit PE file whose only section holds a version resource
    /// with these strings, followed by `text`.
    pub fn fake_pe(strings: &[(&str, &str)], text: &[u8]) -> Vec<u8> {
        let entries: Vec<Vec<u8>> = strings
            .iter()
            .map(|(k, v)| {
                let value = utf16z(v);
                block(k, &value, (value.len() / 2) as u16, true, &[])
            })
            .collect();
        let table = block("040904b0", &[], 0, true, &entries);
        let file_info = block("StringFileInfo", &[], 0, true, &[table]);
        let mut fixed = vec![0u8; 52];
        fixed[..4].copy_from_slice(&0xFEEF_04BDu32.to_le_bytes());
        let info = block("VS_VERSION_INFO", &fixed, 52, false, &[file_info]);

        const RVA: u32 = 0x1000;
        let dir = |id: u32, offset: u32| {
            let mut d = vec![0u8; 14];
            d.extend(1u16.to_le_bytes());
            d.extend(id.to_le_bytes());
            d.extend(offset.to_le_bytes());
            d
        };
        let mut rsrc = dir(16, 0x8000_0000 | 0x18);
        rsrc.extend(dir(1, 0x8000_0000 | 0x30));
        rsrc.extend(dir(0x409, 0x48));
        rsrc.extend((RVA + 0x58).to_le_bytes());
        rsrc.extend((info.len() as u32).to_le_bytes());
        rsrc.extend([0u8; 8]);
        rsrc.extend(&info);

        let mut pe = vec![0u8; 0x200];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        pe[0x40..0x44].copy_from_slice(b"PE\0\0");
        let coff = 0x44;
        pe[coff..coff + 2].copy_from_slice(&0x8664u16.to_le_bytes());
        pe[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
        pe[coff + 16..coff + 18].copy_from_slice(&240u16.to_le_bytes());
        let optional = coff + 20;
        pe[optional..optional + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        let resources = optional + 112 + 2 * 8;
        pe[resources..resources + 4].copy_from_slice(&RVA.to_le_bytes());
        pe[resources + 4..resources + 8].copy_from_slice(&(rsrc.len() as u32).to_le_bytes());
        let section = optional + 240;
        pe[section..section + 5].copy_from_slice(b".rsrc");
        for (at, v) in [
            (8, rsrc.len() as u32),
            (12, RVA),
            (16, rsrc.len() as u32),
            (20, 0x200),
        ] {
            pe[section + at..section + at + 4].copy_from_slice(&v.to_le_bytes());
        }
        pe.extend(rsrc);
        pe.extend(text);
        pe
    }

    /// A `.lnk` whose LinkInfo holds `target` as its local base path.
    fn lnk_with_base_path(target: &str) -> Vec<u8> {
        let mut lnk = vec![0u8; 0x4C];
        lnk[..4].copy_from_slice(&0x4Cu32.to_le_bytes());
        lnk[0x14..0x18].copy_from_slice(&0x2u32.to_le_bytes());
        let volume = [0x10u8, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0];
        let base_at = 0x1C + volume.len() as u32;
        let suffix_at = base_at + target.len() as u32 + 1;
        let mut info = Vec::new();
        for v in [0, 0x1C, 1, 0x1C, base_at, 0, suffix_at] {
            info.extend(u32::to_le_bytes(v));
        }
        info.extend(volume);
        info.extend(target.as_bytes());
        info.extend([0, 0]);
        let size = info.len() as u32;
        info[..4].copy_from_slice(&size.to_le_bytes());
        lnk.extend(info);
        lnk
    }

    /// A `.lnk` with only a Unicode relative path (and a name before it).
    fn lnk_with_relative_path(name: &str, relative: &str) -> Vec<u8> {
        let mut lnk = vec![0u8; 0x4C];
        lnk[..4].copy_from_slice(&0x4Cu32.to_le_bytes());
        lnk[0x14..0x18].copy_from_slice(&(0x4u32 | 0x8 | 0x80).to_le_bytes());
        for s in [name, relative] {
            let units: Vec<u16> = s.encode_utf16().collect();
            lnk.extend((units.len() as u16).to_le_bytes());
            lnk.extend(units.iter().flat_map(|u| u.to_le_bytes()));
        }
        lnk
    }

    fn places(root: &Path) -> Places {
        let home = root.join("Users/me");
        Places {
            roaming: home.join("AppData/Roaming"),
            local: home.join("AppData/Local"),
            documents: home.join("Documents"),
            transient: vec![home.join("Desktop"), home.join("Downloads")],
            program_dirs: vec![
                root.join("Program Files"),
                home.join("AppData/Local/Programs"),
            ],
            winget: vec![home.join("AppData/Local/Microsoft/WinGet/Packages")],
            scoop: vec![home.join("scoop/apps")],
            uninstall: Vec::new(),
            shortcuts: vec![
                (
                    home.join("AppData/Roaming/Microsoft/Windows/Start Menu/Programs"),
                    true,
                ),
                (home.join("Desktop"), false),
            ],
            elsewhere: vec![
                home.join("Desktop"),
                home.join("Downloads"),
                home.join("Documents"),
            ],
            registry: Vec::new(),
        }
    }

    fn summary(found: &[Found]) -> Vec<(&'static str, Kind, bool)> {
        found
            .iter()
            .map(|f| (f.emulator, f.kind, f.installed))
            .collect()
    }

    #[test]
    fn reads_the_version_resource() {
        let dir = TempDir::new("pe-version");
        let exe = dir.path().join("mGBA.exe");
        write(
            &exe,
            &fake_pe(&[("CompanyName", "endrift"), ("ProductName", "mGBA")], b""),
        );
        assert_eq!(
            version_strings(&exe).unwrap(),
            vec![
                ("CompanyName".to_owned(), "endrift".to_owned()),
                ("ProductName".to_owned(), "mGBA".to_owned())
            ]
        );
        write(&exe, b"MZ not a PE file at all");
        assert_eq!(version_strings(&exe), None);
    }

    #[test]
    fn finds_programs_by_their_marks_wherever_they_were_put() {
        let root = TempDir::new("detect-windows");
        let mut places = places(root.path());
        let home = root.path().join("Users/me");
        let pf = root.path().join("Program Files");
        write(
            &pf.join("Dolphin/Dolphin.exe"),
            b"...https://dolphin-emu.org/...",
        );
        write(
            &pf.join("mGBA/mGBA.exe"),
            &fake_pe(&[("ProductName", "mGBA")], b""),
        );
        write(&pf.join("mGBA/mgba-sdl.exe"), b"");
        places.uninstall.push(pf.join("mGBA"));
        let winget = home.join("AppData/Local/Microsoft/WinGet/Packages/Stenzek.DuckStation_x");
        write(
            &winget.join("duckstation-qt-x64-ReleaseLTCG.exe"),
            &fake_pe(&[("ProductName", "DuckStation")], b""),
        );
        write(
            &winget.join("updater.exe"),
            &fake_pe(&[("ProductName", "DuckStation")], b""),
        );
        let scoop = home.join("scoop/apps/azahar");
        write(&scoop.join("current/azahar.exe"), b"azahar-emu.org");
        write(&scoop.join("2126.2/azahar.exe"), b"azahar-emu.org");
        write(
            &home.join("Downloads/Eden-Windows/eden.exe"),
            b"eden-emu.dev",
        );

        let found = scan(&places, &emulators(), false);
        assert_eq!(
            summary(&found.found),
            vec![
                ("duckstation", Kind::Winget, true),
                ("mgba", Kind::Installer, true),
                ("dolphin", Kind::Folder, true),
                ("azahar", Kind::Scoop, true),
            ],
            "Downloads only with everywhere"
        );
        assert_eq!(found.found[3].binary, scoop.join("current/azahar.exe"));
        assert_eq!(
            found.found[0].config_dir,
            home.join("AppData/Local/DuckStation")
        );
        assert_eq!(
            found.found[2].config_dir,
            home.join("AppData/Roaming/Dolphin Emulator")
        );

        let found = scan(&places, &emulators(), true);
        let eden = found.found.iter().find(|f| f.emulator == "eden").unwrap();
        assert_eq!((eden.kind, eden.installed), (Kind::Folder, false));
        assert_eq!(eden.config_dir, home.join("AppData/Roaming/eden"));
    }

    #[test]
    fn shortcuts_lead_to_programs_anywhere() {
        let root = TempDir::new("detect-windows-lnk");
        let places = places(root.path());
        let home = root.path().join("Users/me");
        let emus = root.path().join("D/Emulators");
        let dolphin = emus.join("Dolphin/Dolphin.exe");
        let eden = emus.join("Émulateurs/eden/eden.exe");
        write(&dolphin, b"dolphin-emu.org");
        write(&eden, b"eden-emu.dev");
        write(
            &home.join("AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Games/Dolphin.lnk"),
            &lnk_with_base_path(&dolphin.to_string_lossy()),
        );
        // Not ASCII, so only the relative path spells it.
        let relative = Path::new("../../../D/Emulators/Émulateurs/eden/eden.exe");
        write(
            &home.join("Desktop/Eden.lnk"),
            &lnk_with_relative_path("Eden", &relative.to_string_lossy()),
        );

        let found = scan(&places, &emulators(), false);
        assert_eq!(
            summary(&found.found),
            vec![
                ("dolphin", Kind::Folder, true),
                ("eden", Kind::Folder, true)
            ]
        );
        assert_eq!(found.found[0].binary, dolphin);
        assert_eq!(
            found.found[1].binary, eden,
            "the shortcut's folder resolved"
        );
    }

    #[test]
    fn data_folders_follow_each_emulators_rules() {
        let root = TempDir::new("detect-windows-data");
        let mut places = places(root.path());
        let home = root.path().join("Users/me");
        let dir = root.path().join("Program Files/Emu");
        let app = |e: &dyn Emulator| e.windows_app().unwrap();
        let dolphin = || data_dir(&app(&Dolphin), &dir, &places);

        assert_eq!(dolphin(), home.join("AppData/Roaming/Dolphin Emulator"));
        write(
            &home.join("Documents/Dolphin Emulator/Config/Dolphin.ini"),
            b"",
        );
        assert_eq!(
            dolphin(),
            home.join("Documents/Dolphin Emulator"),
            "the legacy Documents folder"
        );
        places.registry.push((
            r"Software\Dolphin Emulator",
            UserDirKey {
                local: false,
                path: Some(root.path().join("custom")),
            },
        ));
        let dolphin = |places: &Places| data_dir(&app(&Dolphin), &dir, places);
        assert_eq!(
            dolphin(&places),
            root.path().join("custom"),
            "UserConfigPath"
        );
        places.registry[0].1.local = true;
        assert_eq!(dolphin(&places), dir.join("User"), "LocalUserConfig");
        places.registry.clear();
        write(&dir.join("portable.txt"), b"");
        assert_eq!(dolphin(&places), dir.join("User"), "portable.txt");

        assert_eq!(
            data_dir(&app(&DuckStation), &dir, &places),
            dir,
            "portable.txt"
        );
        assert_eq!(
            data_dir(&app(&MGba), &dir, &places),
            home.join("AppData/Roaming/mGBA")
        );
        write(&dir.join("portable.ini"), b"");
        assert_eq!(data_dir(&app(&MGba), &dir, &places), dir);

        assert_eq!(
            data_dir(&app(&Eden), &dir, &places),
            home.join("AppData/Roaming/eden")
        );
        std::fs::create_dir_all(dir.join("user")).unwrap();
        assert_eq!(data_dir(&app(&Eden), &dir, &places), dir.join("user"));
        assert_eq!(data_dir(&app(&Azahar), &dir, &places), dir.join("user"));
    }

    #[test]
    fn refuses_programs_that_only_look_right() {
        let root = TempDir::new("detect-windows-refuse");
        let places = places(root.path());
        let pf = root.path().join("Program Files");
        write(&pf.join("Fake/Dolphin.exe"), b"no web address in here");
        write(
            &pf.join("Other/mGBA.exe"),
            &fake_pe(&[("ProductName", "Not mGBA")], b""),
        );
        write(&pf.join("Renamed/dolphin-copy.exe"), b"dolphin-emu.org");
        write(&pf.join("Duck/duckstation-qt.exe"), b"no version resource");
        assert!(scan(&places, &emulators(), true).found.is_empty());
    }

    #[test]
    fn a_named_path_finds_the_program_from_its_folder_file_or_shortcut() {
        let root = TempDir::new("detect-windows-at");
        let places = places(root.path());
        let folder = root.path().join("Users/me/Downloads/dolphin-2609-x64");
        let exe = folder.join("Dolphin-x64/Dolphin.exe");
        write(&exe, b"dolphin-emu.org");
        let lnk = root.path().join("Dolphin.lnk");
        write(&lnk, &lnk_with_base_path(&exe.to_string_lossy()));
        for named in [&folder, &exe, &lnk] {
            let found = scan_at(&places, named, &emulators()).found;
            assert_eq!(found.len(), 1, "{}", named.display());
            assert_eq!(found[0].binary, exe);
            assert!(!found[0].installed, "Downloads is no place to keep it");
        }
    }

    #[test]
    fn text_split_across_reads_is_found() {
        let dir = TempDir::new("detect-windows-text");
        let file = dir.path().join("big.exe");
        let mut data = vec![b'x'; (4 << 20) - 5];
        data.extend(b"eden-emu.dev");
        write(&file, &data);
        assert!(file_contains(&file, b"eden-emu.dev"));
        assert!(!file_contains(&file, b"azahar-emu.org"));
    }
}
