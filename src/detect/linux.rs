//! Linux: emulators come as distribution packages, Flatpaks and AppImages,
//! and where an emulator keeps its data depends on which. Each candidate is
//! recognised from its own files:
//!
//! - a package by its executable's name, but only when root owns the file
//!   (the package manager put it there);
//! - a Flatpak by its app ID, installed under a Flatpak installation;
//! - an AppImage by the app ID it declares in its image (root desktop file or
//!   icon), or, for images whose files can't be read, its update information.
//!
//! Desktop entries only point at candidates (AppImages integrated by
//! AppImageLauncher, Gear Lever, EmuDeck or by hand); they never identify one.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{Detection, Found, Kind, appimage, sorted_dirs, sorted_entries};
use crate::emulator::Emulator;

/// How an emulator ships on Linux.
#[derive(Debug, Clone, Copy)]
pub struct LinuxApp {
    /// Reverse-DNS app IDs: Flatpak IDs, and the IDs AppImages name their
    /// root desktop file or icon after. Several when the emulator's ID
    /// changed or differs between its builds.
    pub app_ids: &'static [&'static str],
    /// Executables its distribution packages install.
    pub executables: &'static [&'static str],
    /// Text only this emulator's AppImages carry in their update information,
    /// for images whose files can't be read (DwarFS).
    pub update_info: &'static [&'static str],
    /// Its data folder (`config_dir`) in the XDG base directories.
    pub data_dir: XdgDir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XdgDir {
    /// `$XDG_DATA_HOME/<name>` (`~/.local/share/<name>`).
    Data(&'static str),
    /// `$XDG_CONFIG_HOME/<name>` (`~/.config/<name>`).
    Config(&'static str),
    /// `$XDG_CONFIG_HOME/<name>` when that variable is set (as it always is
    /// in a Flatpak), else `~/.local/share/<name>`: DuckStation's rule.
    ConfigHomeElseData(&'static str),
}

/// The XDG base directories an install sees.
struct Xdg {
    data_home: PathBuf,
    config_home: PathBuf,
    config_home_set: bool,
}

impl XdgDir {
    fn resolve(self, xdg: &Xdg) -> PathBuf {
        match self {
            XdgDir::Data(name) => xdg.data_home.join(name),
            XdgDir::Config(name) => xdg.config_home.join(name),
            XdgDir::ConfigHomeElseData(name) if xdg.config_home_set => xdg.config_home.join(name),
            XdgDir::ConfigHomeElseData(name) => xdg.data_home.join(name),
        }
    }
}

/// Where a scan looks: from the environment, or a test's temp tree.
pub struct Places {
    home: PathBuf,
    data_home: PathBuf,
    data_home_set: bool,
    config_home: PathBuf,
    config_home_set: bool,
    /// Searched for packaged executables.
    path: Vec<PathBuf>,
    /// Flatpak installations, the user's first.
    flatpak: Vec<PathBuf>,
    /// XDG user directories by their Flatpak name (`xdg-download` →
    /// `download`); Desktop, Downloads and Documents are also searched with
    /// `everywhere`.
    user_dirs: Vec<(&'static str, PathBuf)>,
    /// Who owns files the package manager installed: root.
    package_owner: u32,
}

impl Places {
    fn from_env(home: PathBuf) -> Self {
        let var = |name| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        let data_home = var("XDG_DATA_HOME");
        let config_home = var("XDG_CONFIG_HOME");
        let mut path: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        // A service's PATH can be short; packages install here.
        for dir in ["/usr/local/bin", "/usr/bin", "/usr/games"] {
            path.push(dir.into());
        }
        let mut places = Places {
            data_home_set: data_home.is_some(),
            data_home: data_home.unwrap_or_else(|| home.join(".local/share")),
            config_home_set: config_home.is_some(),
            config_home: config_home.unwrap_or_else(|| home.join(".config")),
            path,
            flatpak: Vec::new(),
            user_dirs: [
                ("desktop", dirs::desktop_dir()),
                ("download", dirs::download_dir()),
                ("documents", dirs::document_dir()),
                ("music", dirs::audio_dir()),
                ("pictures", dirs::picture_dir()),
                ("videos", dirs::video_dir()),
                ("templates", dirs::template_dir()),
                ("public-share", dirs::public_dir()),
            ]
            .into_iter()
            .filter_map(|(name, dir)| Some((name, dir.filter(|d| *d != home)?)))
            .collect(),
            package_owner: 0,
            home,
        };
        places.flatpak = flatpak_installations(&places.data_home);
        places
    }

    fn xdg(&self) -> Xdg {
        Xdg {
            data_home: self.data_home.clone(),
            config_home: self.config_home.clone(),
            config_home_set: self.config_home_set,
        }
    }

    /// Where people keep AppImages (AppImageLauncher, EmuDeck: Applications;
    /// Gear Lever: AppImages).
    fn appimage_dirs(&self) -> Vec<PathBuf> {
        ["Applications", "AppImages", ".local/bin", "bin"]
            .iter()
            .map(|d| self.home.join(d))
            .collect()
    }

    fn elsewhere(&self) -> Vec<PathBuf> {
        self.user_dirs
            .iter()
            .filter(|(name, _)| matches!(*name, "desktop" | "download" | "documents"))
            .map(|(_, dir)| dir.clone())
            .collect()
    }

    fn user_dir(&self, name: &str) -> Option<&Path> {
        self.user_dirs
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, d)| d.as_path())
    }
}

/// The user's installation, the system one, and any configured in
/// `/etc/flatpak/installations.d`.
fn flatpak_installations(data_home: &Path) -> Vec<PathBuf> {
    let mut found = vec![data_home.join("flatpak"), PathBuf::from("/var/lib/flatpak")];
    for conf in sorted_entries(Path::new("/etc/flatpak/installations.d")) {
        let text = std::fs::read_to_string(&conf).unwrap_or_default();
        found.extend(
            text.lines()
                .filter_map(|l| l.trim().strip_prefix("Path="))
                .map(|p| PathBuf::from(p.trim())),
        );
    }
    found
}

pub fn detect(emulators: &[Arc<dyn Emulator>], everywhere: bool) -> Detection {
    let Some(home) = dirs::home_dir() else {
        return Detection::default();
    };
    scan(&Places::from_env(home), emulators, everywhere)
}

pub fn detect_at(path: &Path, emulators: &[Arc<dyn Emulator>]) -> Detection {
    let Some(home) = dirs::home_dir() else {
        return Detection::default();
    };
    scan_at(&Places::from_env(home), path, emulators)
}

/// In order of preference: packages, AppImages in the usual folders or
/// launched by a desktop entry, Flatpaks; with `everywhere` also AppImages
/// in Desktop, Downloads, Documents and the rest of `PATH`.
pub fn scan(places: &Places, emulators: &[Arc<dyn Emulator>], everywhere: bool) -> Detection {
    let mut found = Vec::new();
    for dir in unique_dirs(&places.path) {
        for emulator in emulators {
            let Some(app) = emulator.linux_app() else {
                continue;
            };
            for exe in app.executables {
                found.extend(package(&dir.join(exe), emulator, &app, places));
            }
        }
    }
    let mut seen = HashSet::new();
    let mut appimages = |files: Vec<PathBuf>, installed: bool, found: &mut Vec<Found>| {
        for file in files {
            if seen.insert(file.canonicalize().unwrap_or_else(|_| file.clone())) {
                found.extend(identify_appimage(&file, emulators, places, installed));
            }
        }
    };
    let launched = desktop_programs(places);
    for dir in places.appimage_dirs() {
        appimages(files_in(&dir), true, &mut found);
    }
    appimages(launched, true, &mut found);
    for installation in &places.flatpak {
        for emulator in emulators {
            let Some(app) = emulator.linux_app() else {
                continue;
            };
            for id in app.app_ids {
                found.extend(flatpak(installation, id, emulator, &app, places));
            }
        }
    }
    if everywhere {
        for dir in places.elsewhere() {
            appimages(files_in(&dir), false, &mut found);
        }
        // Packages don't come as AppImages; only the user's own folders
        // could hold one.
        for dir in unique_dirs(&places.path)
            .into_iter()
            .filter(|d| d.starts_with(&places.home))
        {
            appimages(top_level_files(&dir), true, &mut found);
        }
    }
    Detection::from_found(dedup(found))
}

/// What is at `path`, a program or a folder of them, which the user named.
fn scan_at(places: &Places, path: &Path, emulators: &[Arc<dyn Emulator>]) -> Detection {
    let launched: Vec<PathBuf> = desktop_programs(places)
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();
    let install_dirs = places.appimage_dirs();
    let files = if path.is_dir() {
        files_in(path)
    } else {
        vec![path.to_owned()]
    };
    let mut found = Vec::new();
    for file in files {
        let installed = install_dirs.iter().any(|d| file.starts_with(d))
            || file.canonicalize().is_ok_and(|c| launched.contains(&c));
        found.extend(identify_appimage(&file, emulators, places, installed));
        for emulator in emulators {
            let Some(app) = emulator.linux_app() else {
                continue;
            };
            if file
                .file_name()
                .is_some_and(|n| app.executables.iter().any(|e| n == *e))
            {
                found.extend(package(&file, emulator, &app, places));
            }
            if let Some((installation, id)) = flatpak_export(&file)
                && app.app_ids.contains(&id.as_str())
            {
                found.extend(flatpak(&installation, &id, emulator, &app, places));
            }
        }
    }
    Detection::from_found(dedup(found))
}

/// One entry per binary, wherever else it was found too.
fn dedup(found: Vec<Found>) -> Vec<Found> {
    let mut seen = HashSet::new();
    found
        .into_iter()
        .filter(|f| {
            seen.insert((
                f.emulator,
                f.binary.canonicalize().unwrap_or_else(|_| f.binary.clone()),
            ))
        })
        .collect()
}

fn unique_dirs(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    dirs.iter()
        .filter(|d| {
            d.is_absolute() && seen.insert(d.canonicalize().unwrap_or_else(|_| (*d).clone()))
        })
        .cloned()
        .collect()
}

/// A packaged executable: on `PATH` (or named), owned by root.
fn package(
    file: &Path,
    emulator: &Arc<dyn Emulator>,
    app: &LinuxApp,
    places: &Places,
) -> Option<Found> {
    if !owned_executable(file, places.package_owner) {
        return None;
    }
    Some(Found::new(
        emulator.as_ref(),
        Kind::Package,
        file.to_owned(),
        file.to_owned(),
        app.data_dir.resolve(&places.xdg()),
        true,
    ))
}

#[cfg(unix)]
fn owned_executable(file: &Path, owner: u32) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::metadata(file)
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0 && m.uid() == owner)
}

#[cfg(not(unix))]
fn owned_executable(_file: &Path, _owner: u32) -> bool {
    false
}

/// The Flatpak `id` if `installation` has it, run through its exported
/// launcher (`exports/bin/<id>`, a script calling `flatpak run`).
fn flatpak(
    installation: &Path,
    id: &str,
    emulator: &Arc<dyn Emulator>,
    app: &LinuxApp,
    places: &Places,
) -> Option<Found> {
    let deployed = installation.join("app").join(id);
    let binary = installation.join("exports/bin").join(id);
    if !deployed.join("current/active/metadata").is_file() || !binary.is_file() {
        return None;
    }
    let base = places.home.join(".var/app").join(id);
    let xdg = Xdg {
        data_home: base.join("data"),
        config_home: base.join("config"),
        config_home_set: true,
    };
    let mut found = Found::new(
        emulator.as_ref(),
        Kind::Flatpak,
        deployed.clone(),
        binary,
        app.data_dir.resolve(&xdg),
        true,
    );
    found.sandbox = Some(Sandbox::read(&deployed, id, places));
    Some(found)
}

/// `(installation, app ID)` for an exported launcher,
/// `<installation>/exports/bin/<app id>`.
fn flatpak_export(file: &Path) -> Option<(PathBuf, String)> {
    let bin = file.parent()?;
    let exports = bin.parent()?;
    if bin.file_name()? != "bin" || exports.file_name()? != "exports" {
        return None;
    }
    Some((
        exports.parent()?.to_owned(),
        file.file_name()?.to_str()?.to_owned(),
    ))
}

fn identify_appimage(
    file: &Path,
    emulators: &[Arc<dyn Emulator>],
    places: &Places,
    installed: bool,
) -> Vec<Found> {
    let Some(image) = appimage::read(file) else {
        return Vec::new();
    };
    emulators
        .iter()
        .filter_map(|emulator| {
            let app = emulator.linux_app()?;
            let declared = app.app_ids.iter().any(|id| image.declares(id));
            let updates = !image.update_info.is_empty()
                && app
                    .update_info
                    .iter()
                    .any(|u| image.update_info.contains(u));
            (declared || updates).then(|| {
                Found::new(
                    emulator.as_ref(),
                    Kind::AppImage,
                    file.to_owned(),
                    file.to_owned(),
                    app.data_dir.resolve(&appimage_xdg(file, places)),
                    installed,
                )
            })
        })
        .collect()
}

/// An AppImage with a `<name>.home` folder next to it runs with that as its
/// home, and with `<name>.config` as its `XDG_CONFIG_HOME` (the runtime's
/// `--appimage-portable-home` / `--appimage-portable-config`).
fn appimage_xdg(file: &Path, places: &Places) -> Xdg {
    let beside = |suffix: &str| {
        let mut name = file.as_os_str().to_owned();
        name.push(suffix);
        Some(PathBuf::from(name)).filter(|p| p.is_dir())
    };
    let home = beside(".home").unwrap_or_else(|| places.home.clone());
    let (config_home, config_home_set) = match beside(".config") {
        Some(dir) => (dir, true),
        None if places.config_home_set => (places.config_home.clone(), true),
        None => (home.join(".config"), false),
    };
    Xdg {
        data_home: if places.data_home_set {
            places.data_home.clone()
        } else {
            home.join(".local/share")
        },
        config_home,
        config_home_set,
    }
}

/// Files in `dir` and one folder level below (people group emulators, e.g.
/// `~/Applications/Emulators`), leaving out hidden ones (AppImageLauncher
/// keeps removed AppImages in `~/Applications/.trash`).
fn files_in(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in sorted_entries(dir).into_iter().filter(|p| !hidden(p)) {
        if entry.is_dir() {
            files.extend(top_level_files(&entry));
        } else if entry.is_file() {
            files.push(entry);
        }
    }
    files
}

fn top_level_files(dir: &Path) -> Vec<PathBuf> {
    sorted_entries(dir)
        .into_iter()
        .filter(|p| !hidden(p) && p.is_file())
        .collect()
}

fn hidden(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

/// The programs the user's desktop entries launch, where named by path.
fn desktop_programs(places: &Places) -> Vec<PathBuf> {
    let applications = places.data_home.join("applications");
    let mut entries = sorted_entries(&applications);
    entries.extend(
        sorted_dirs(&applications)
            .iter()
            .flat_map(|d| sorted_entries(d)),
    );
    entries
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "desktop"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|text| desktop_program(&text, &places.home))
        .collect()
}

/// The program a desktop entry's `Exec` runs, if it names one by path.
fn desktop_program(entry: &str, home: &Path) -> Option<PathBuf> {
    let mut in_main_group = false;
    let mut exec = None;
    for line in entry.lines().map(str::trim) {
        if line.starts_with('[') {
            in_main_group = line == "[Desktop Entry]";
        } else if in_main_group
            && let Some(value) = line
                .strip_prefix("Exec")
                .and_then(|v| v.trim_start().strip_prefix('='))
        {
            exec = Some(value.trim());
        }
    }
    let args = exec_args(&unescape(exec?));
    let mut args = args.iter().map(String::as_str).peekable();
    if args
        .peek()
        .is_some_and(|a| *a == "env" || a.ends_with("/env"))
    {
        args.next();
        while args
            .peek()
            .is_some_and(|a| a.contains('=') && !a.starts_with('/'))
        {
            args.next();
        }
    }
    let program = args.next()?;
    let program = match program.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(program),
    };
    program.is_absolute().then_some(program)
}

/// A desktop entry string's escapes (`\s`, `\n`, `\t`, `\r`, `\\`).
fn unescape(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// `Exec` split into arguments: whitespace separates them, double quotes
/// group them, and a backslash inside quotes escapes the next character.
fn exec_args(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut arg = String::new();
    let (mut quoted, mut started) = (false, false);
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => (quoted, started) = (!quoted, true),
            '\\' if quoted => arg.extend(chars.next()),
            c if c.is_whitespace() && !quoted => {
                if started || !arg.is_empty() {
                    args.push(std::mem::take(&mut arg));
                }
                started = false;
            }
            c => arg.push(c),
        }
    }
    if started || !arg.is_empty() {
        args.push(arg);
    }
    args
}

/// The host paths a Flatpak may read: its own `~/.var/app/<id>` plus the
/// `filesystems` of its metadata and of the user's and the system's
/// overrides (`flatpak override`), in the order Flatpak applies them.
#[derive(Debug, Clone, PartialEq)]
pub struct Sandbox {
    pub app_id: String,
    readable: Vec<PathBuf>,
}

impl Sandbox {
    fn read(deployed: &Path, id: &str, places: &Places) -> Self {
        let mut sources = vec![deployed.join("current/active/metadata")];
        for installation in places.flatpak.iter().rev() {
            sources.push(installation.join("overrides/global"));
            sources.push(installation.join(format!("overrides/{id}")));
        }
        let mut granted: Vec<String> = Vec::new();
        for source in sources {
            let text = std::fs::read_to_string(source).unwrap_or_default();
            for fs in context_filesystems(&text) {
                match fs.strip_prefix('!') {
                    Some(denied) => granted.retain(|g| g != denied),
                    None if !granted.contains(&fs) => granted.push(fs),
                    None => {}
                }
            }
        }
        let mut readable = vec![places.home.join(".var/app").join(id)];
        readable.extend(granted.iter().filter_map(|fs| sandbox_path(fs, places)));
        Sandbox {
            app_id: id.to_owned(),
            readable,
        }
    }

    /// Whether the app can see `path`. Unknown grants count as not given,
    /// so this may say no to a path the app can in fact read.
    pub fn can_read(&self, path: &Path) -> bool {
        let canonical = path.canonicalize().ok();
        self.readable.iter().any(|r| {
            path.starts_with(r)
                || canonical
                    .as_ref()
                    .is_some_and(|c| c.starts_with(r.canonicalize().as_ref().unwrap_or(r)))
        })
    }
}

/// `filesystems` in a keyfile's `[Context]` group, without their access
/// suffix (`:ro`, `:rw`, `:create`).
fn context_filesystems(keyfile: &str) -> Vec<String> {
    let mut in_context = false;
    let mut found = Vec::new();
    for line in keyfile.lines().map(str::trim) {
        if line.starts_with('[') {
            in_context = line == "[Context]";
        } else if in_context && let Some(value) = line.strip_prefix("filesystems=") {
            found.extend(
                value
                    .split(';')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(|v| match v.rsplit_once(':') {
                        Some((fs, "ro" | "rw" | "create")) => fs.to_owned(),
                        _ => v.to_owned(),
                    }),
            );
        }
    }
    found
}

/// The host path a `filesystems` grant opens.
fn sandbox_path(fs: &str, places: &Places) -> Option<PathBuf> {
    if fs == "host" {
        return Some(PathBuf::from("/"));
    }
    if fs == "home" || fs == "~" {
        return Some(places.home.clone());
    }
    if let Some(rest) = fs.strip_prefix("~/") {
        return Some(places.home.join(rest));
    }
    if fs.starts_with('/') {
        return Some(PathBuf::from(fs));
    }
    let (name, sub) = fs.split_once('/').unwrap_or((fs, ""));
    let base = match name.strip_prefix("xdg-")? {
        "data" => places.data_home.clone(),
        "config" => places.config_home.clone(),
        "cache" => places.home.join(".cache"),
        user_dir => places.user_dir(user_dir)?.to_owned(),
    };
    Some(if sub.is_empty() { base } else { base.join(sub) })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::detect::appimage::tests::{fake, fake_dwarfs};
    use crate::emulator::{
        azahar::Azahar, dolphin::Dolphin, duckstation::DuckStation, eden::Eden, mgba::MGba,
        ryujinx::Ryujinx,
    };
    use crate::testing::{TempDir, write};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn emulators() -> Vec<Arc<dyn Emulator>> {
        vec![
            Arc::new(Dolphin),
            Arc::new(DuckStation),
            Arc::new(MGba),
            Arc::new(Ryujinx),
            Arc::new(Eden),
            Arc::new(Azahar),
        ]
    }

    /// A home, `/usr/bin` and the system Flatpak installation under `root`,
    /// with the test's own user standing in for root as package owner.
    fn places(root: &Path) -> Places {
        let home = root.join("home");
        let data_home = home.join(".local/share");
        Places {
            config_home: home.join(".config"),
            config_home_set: false,
            path: vec![root.join("usr/bin")],
            flatpak: vec![data_home.join("flatpak"), root.join("var/lib/flatpak")],
            user_dirs: vec![
                ("desktop", home.join("Desktop")),
                ("download", home.join("Downloads")),
            ],
            package_owner: std::fs::metadata(root).unwrap().uid(),
            data_home,
            data_home_set: false,
            home,
        }
    }

    fn executable(path: &Path, content: &[u8]) {
        write(path, content);
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn appimage(path: &Path, app_id: &str) {
        executable(
            path,
            &fake(&["AppRun", &format!("{app_id}.desktop"), "usr"], "", true),
        );
    }

    fn flatpak_app(installation: &Path, id: &str, filesystems: &str) {
        write(
            &installation
                .join("app")
                .join(id)
                .join("current/active/metadata"),
            format!("[Application]\nname={id}\n\n[Context]\nfilesystems={filesystems}\n")
                .as_bytes(),
        );
        executable(
            &installation.join("exports/bin").join(id),
            format!("#!/bin/sh\nexec /usr/bin/flatpak run {id} \"$@\"\n").as_bytes(),
        );
    }

    fn summary(found: &[Found]) -> Vec<(&str, Kind, bool)> {
        found
            .iter()
            .map(|f| (f.emulator, f.kind, f.installed))
            .collect()
    }

    #[test]
    fn finds_packages_appimages_and_flatpaks_by_their_own_files() {
        let root = TempDir::new("detect-linux");
        let places = places(root.path());
        let home = &places.home;
        let bin = root.path().join("usr/bin");
        executable(&bin.join("dolphin-emu"), b"\x7fELF");
        // KDE's file manager, and a script that isn't executable.
        executable(&bin.join("dolphin"), b"\x7fELF");
        write(&bin.join("mgba-qt"), b"#!/bin/sh");

        let apps = home.join("Applications");
        appimage(
            &apps.join("DuckStation-x64.AppImage"),
            "org.duckstation.DuckStation",
        );
        executable(
            &apps.join("Emulators/Eden-Linux-amd64.AppImage"),
            &fake_dwarfs("zsync|https://nightly.eden-emu.dev/latest/Eden.AppImage.zsync"),
        );
        appimage(
            &apps.join(".trash/azahar.AppImage"),
            "org.azahar_emu.Azahar",
        );
        appimage(&apps.join("Blockbench.AppImage"), "blockbench");
        // Renamed: the ID inside counts, not the file name.
        appimage(
            &home.join("Games/dolphin.AppImage"),
            "org.azahar_emu.Azahar",
        );
        write(
            &places
                .data_home
                .join("applications/appimagekit_1234-Azahar.desktop"),
            format!(
                "[Desktop Entry]\nName=Azahar\nExec=\"{}\" %f\n\n[Desktop Action Remove]\n\
                 Exec=/usr/bin/remove-appimage\n",
                home.join("Games/dolphin.AppImage").display()
            )
            .as_bytes(),
        );
        appimage(&home.join("Downloads/mGBA.AppImage"), "io.mgba.mGBA");
        flatpak_app(
            &places.flatpak[0],
            "io.github.ryubing.Ryujinx",
            "xdg-download;",
        );

        let found = scan(&places, &emulators(), false);
        assert_eq!(
            summary(&found.found),
            vec![
                ("duckstation", Kind::AppImage, true),
                ("dolphin", Kind::Package, true),
                ("ryujinx", Kind::Flatpak, true),
                ("eden", Kind::AppImage, true),
                ("azahar", Kind::AppImage, true),
            ],
            "in emulator::all() order, not the order found"
        );
        assert!(found.also.is_empty(), "{:?}", found.also);
        let by_id = |id: &str| found.found.iter().find(|f| f.emulator == id).unwrap();
        assert_eq!(by_id("dolphin").binary, bin.join("dolphin-emu"));
        assert_eq!(
            by_id("dolphin").config_dir,
            home.join(".local/share/dolphin-emu")
        );
        assert_eq!(
            by_id("duckstation").config_dir,
            home.join(".local/share/duckstation")
        );
        assert_eq!(by_id("azahar").binary, home.join("Games/dolphin.AppImage"));
        let ryujinx = by_id("ryujinx");
        assert_eq!(
            ryujinx.binary,
            places.flatpak[0].join("exports/bin/io.github.ryubing.Ryujinx")
        );
        assert_eq!(
            ryujinx.config_dir,
            home.join(".var/app/io.github.ryubing.Ryujinx/config/Ryujinx")
        );
        assert!(
            ryujinx
                .sandbox
                .as_ref()
                .unwrap()
                .can_read(&home.join("Downloads/roms"))
        );

        let everywhere = scan(&places, &emulators(), true);
        let mgba = everywhere
            .found
            .iter()
            .find(|f| f.emulator == "mgba")
            .unwrap();
        assert_eq!(mgba.kind, Kind::AppImage);
        assert!(!mgba.installed, "Downloads is no install folder");
    }

    #[test]
    fn of_several_installs_one_that_has_run_wins() {
        let root = TempDir::new("detect-linux-prefer");
        let places = places(root.path());
        executable(&root.path().join("usr/bin/dolphin-emu"), b"\x7fELF");
        flatpak_app(&places.flatpak[1], "org.DolphinEmu.dolphin-emu", "");

        let found = scan(&places, &emulators(), false);
        assert_eq!(found.found[0].kind, Kind::Package, "packages first");
        assert_eq!(found.also[0].kind, Kind::Flatpak);

        let flatpak_data = places
            .home
            .join(".var/app/org.DolphinEmu.dolphin-emu/data/dolphin-emu");
        std::fs::create_dir_all(flatpak_data.join("GC")).unwrap();
        let found = scan(&places, &emulators(), false);
        assert_eq!(found.found[0].kind, Kind::Flatpak);
        assert_eq!(found.found[0].config_dir, flatpak_data);
        assert!(found.found[0].config_dir_in_use);
        assert_eq!(found.also[0].kind, Kind::Package);
    }

    #[test]
    fn data_dirs_follow_the_xdg_variables_portable_folders_and_flatpak() {
        let root = TempDir::new("detect-linux-xdg");
        let mut places = places(root.path());
        let home = places.home.clone();
        let image = home.join("Applications/DuckStation.AppImage");
        appimage(&image, "org.duckstation.DuckStation");
        let azahar = home.join("Applications/Azahar.AppImage");
        appimage(&azahar, "org.azahar_emu.Azahar");
        std::fs::create_dir_all(home.join("Applications/Azahar.AppImage.home")).unwrap();
        flatpak_app(&places.flatpak[0], "org.duckstation.DuckStation", "");
        let config_dir = |places: &Places, binary: &Path| {
            let found = scan(places, &emulators(), false);
            found
                .found
                .iter()
                .chain(&found.also)
                .find(|f| f.binary == binary)
                .unwrap()
                .config_dir
                .clone()
        };

        assert_eq!(
            config_dir(&places, &image),
            home.join(".local/share/duckstation")
        );
        assert_eq!(
            config_dir(&places, &azahar),
            home.join("Applications/Azahar.AppImage.home/.local/share/azahar-emu")
        );
        let flatpak = places.flatpak[0].join("exports/bin/org.duckstation.DuckStation");
        assert_eq!(
            config_dir(&places, &flatpak),
            home.join(".var/app/org.duckstation.DuckStation/config/duckstation"),
            "Flatpak sets XDG_CONFIG_HOME, which DuckStation prefers"
        );

        places.config_home = root.path().join("xdg-config");
        places.config_home_set = true;
        assert_eq!(
            config_dir(&places, &image),
            root.path().join("xdg-config/duckstation")
        );
        std::fs::create_dir_all(home.join("Applications/DuckStation.AppImage.config")).unwrap();
        assert_eq!(
            config_dir(&places, &image),
            home.join("Applications/DuckStation.AppImage.config/duckstation")
        );
    }

    #[test]
    fn a_flatpak_reads_what_its_metadata_and_overrides_grant() {
        let root = TempDir::new("detect-linux-sandbox");
        let places = places(root.path());
        let home = &places.home;
        let id = "org.DolphinEmu.dolphin-emu";
        flatpak_app(
            &places.flatpak[1],
            id,
            "xdg-download/roms:ro;/run/media;xdg-pictures",
        );
        write(
            &places.flatpak[0].join("overrides").join(id),
            b"[Context]\nfilesystems=!/run/media;~/Emulation/roms:ro;\n",
        );
        let sandbox = scan(&places, &emulators(), false).found[0]
            .sandbox
            .clone()
            .unwrap();
        assert_eq!(sandbox.app_id, id);
        assert!(sandbox.can_read(&home.join("Emulation/roms/gc/game.rvz")));
        assert!(sandbox.can_read(&home.join("Downloads/roms")));
        assert!(!sandbox.can_read(&home.join("Downloads")));
        assert!(
            !sandbox.can_read(Path::new("/run/media/deck/sd/roms")),
            "revoked"
        );
        assert!(!sandbox.can_read(&home.join("Emulation")));
        assert!(sandbox.can_read(&home.join(".var/app").join(id).join("data")));
        // Unknown user dirs grant nothing rather than everything.
        assert!(!sandbox.can_read(home));
    }

    #[test]
    fn desktop_entries_name_their_program() {
        let home = Path::new("/home/me");
        let program =
            |exec: &str| desktop_program(&format!("[Desktop Entry]\nExec={exec}\n"), home);
        assert_eq!(
            program("/opt/Ryujinx.AppImage %f"),
            Some("/opt/Ryujinx.AppImage".into())
        );
        assert_eq!(
            program(r#""/home/me/My Apps/eden.AppImage" %u"#),
            Some("/home/me/My Apps/eden.AppImage".into())
        );
        assert_eq!(
            program("env DESKTOPINTEGRATION=1 QT_QPA_PLATFORM=xcb ~/Applications/azahar.AppImage"),
            Some("/home/me/Applications/azahar.AppImage".into())
        );
        assert_eq!(
            program("dolphin-emu %f"),
            None,
            "PATH programs are packages"
        );
        assert_eq!(
            desktop_program("[Desktop Action Remove]\nExec=/usr/bin/x\n", home),
            None,
            "actions aren't the app"
        );
    }

    #[test]
    fn a_named_path_is_checked_like_any_candidate() {
        let root = TempDir::new("detect-linux-at");
        let places = places(root.path());
        let downloads = places.home.join("Downloads");
        appimage(&downloads.join("Ryujinx.AppImage"), "app.ryujinx.Ryujinx");
        executable(&downloads.join("Dolphin.AppImage"), b"\x7fELF not really");
        flatpak_app(&places.flatpak[1], "io.mgba.mGBA", "");

        for named in [downloads.clone(), downloads.join("Ryujinx.AppImage")] {
            let found = scan_at(&places, &named, &emulators());
            assert_eq!(
                summary(&found.found),
                vec![("ryujinx", Kind::AppImage, false)]
            );
            assert_eq!(
                found.found[0].config_dir,
                places.home.join(".config/Ryujinx")
            );
        }
        let export = places.flatpak[1].join("exports/bin/io.mgba.mGBA");
        let found = scan_at(&places, &export, &emulators());
        assert_eq!(summary(&found.found), vec![("mgba", Kind::Flatpak, true)]);
        assert_eq!(
            found.found[0].config_dir,
            places.home.join(".var/app/io.mgba.mGBA/config/mgba")
        );
        assert!(
            scan_at(&places, &downloads.join("Dolphin.AppImage"), &emulators())
                .found
                .is_empty()
        );
    }
}
