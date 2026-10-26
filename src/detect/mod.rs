//! Finding installed emulators. Each emulator declares how its install is
//! recognised on each OS (`Emulator::mac_app`, `Emulator::linux_app`,
//! `Emulator::windows_app`); the OS modules look for them.
//!
//! Nothing here runs a program: an install is recognised from its files
//! alone, so the same check can vet a path that a client merely suggests (see
//! the config API item in dev/backlog.md).

mod appimage;
mod linux;
mod mac;
mod windows;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use linux::{LinuxApp, Sandbox, XdgDir};
pub use mac::MacApp;
pub use windows::{AppData, Mark, Portable, WindowsApp, WindowsDataDir};

use crate::emulator::Emulator;

/// How the install was found, which also decides where its data lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A macOS `.app` bundle.
    App,
    AppImage,
    Flatpak,
    /// A distribution package (root-owned, on `PATH`).
    Package,
    /// Windows: put in place by an installer (it has an uninstall entry).
    Installer,
    /// Windows: a folder of its own, e.g. an extracted zip.
    Folder,
    /// Windows: a winget portable package.
    Winget,
    /// Windows: a Scoop package.
    Scoop,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::App => "app",
            Kind::AppImage => "AppImage",
            Kind::Flatpak => "Flatpak",
            Kind::Package => "package",
            Kind::Installer => "installer",
            Kind::Folder => "folder",
            Kind::Winget => "winget",
            Kind::Scoop => "Scoop",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub emulator: &'static str,
    pub name: &'static str,
    pub kind: Kind,
    /// Where it was found, e.g. the `.app` bundle or the AppImage.
    pub install: PathBuf,
    pub binary: PathBuf,
    pub config_dir: PathBuf,
    /// Whether `config_dir` holds the emulator's files; false until it has
    /// run once.
    pub config_dir_in_use: bool,
    /// False for an app found elsewhere (Desktop, Downloads, another drive),
    /// where it is easily moved or deleted, breaking the config. On Linux an
    /// AppImage counts as installed in an install folder or once a desktop
    /// entry launches it.
    pub installed: bool,
    /// What a Flatpak may read outside its sandbox.
    pub sandbox: Option<Sandbox>,
}

impl Found {
    fn new(
        emulator: &dyn Emulator,
        kind: Kind,
        install: PathBuf,
        binary: PathBuf,
        config_dir: PathBuf,
        installed: bool,
    ) -> Self {
        let markers = emulator.config_dir_markers();
        Found {
            emulator: emulator.id(),
            name: emulator.display_name(),
            kind,
            config_dir_in_use: config_dir.is_dir()
                && (markers.is_empty() || markers.iter().any(|m| config_dir.join(m).exists())),
            install,
            binary,
            config_dir,
            installed,
            sandbox: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Detection {
    /// At most one install per emulator.
    pub found: Vec<Found>,
    /// Further installs of an emulator in `found`, which a config can't hold
    /// (it has one entry per emulator ID).
    pub also: Vec<Found>,
}

impl Detection {
    /// One install per emulator from `found`, in order of preference:
    /// installed before found elsewhere, then one that has run (its
    /// `config_dir` is in use) before one that hasn't, then as listed.
    ///
    /// Emulators come in `emulator::all()` order, never in the order the scan
    /// happened to meet them: written to a new config, that order decides
    /// which of two emulators for one platform (Ryujinx, Eden) runs it.
    fn from_found(found: Vec<Found>) -> Self {
        let mut detection = Detection::default();
        let mut emulators: Vec<&str> = crate::emulator::all().iter().map(|e| e.id()).collect();
        for f in &found {
            if !emulators.contains(&f.emulator) {
                emulators.push(f.emulator);
            }
        }
        for emulator in emulators {
            let mut installs: Vec<&Found> =
                found.iter().filter(|f| f.emulator == emulator).collect();
            installs.sort_by_key(|f| (!f.installed, !f.config_dir_in_use));
            let mut installs = installs.into_iter().cloned();
            detection.found.extend(installs.next());
            detection.also.extend(installs);
        }
        detection
    }
}

/// Platforms more than one of `found` runs, each with those emulators in
/// `found`'s order, so the first is the one a config listing them in that
/// order would use.
pub fn shared_platforms(found: &[Found]) -> Vec<(&'static str, Vec<&'static str>)> {
    let all = crate::emulator::all();
    let mut shared: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
    for f in found {
        let Some(emulator) = all.iter().find(|e| e.id() == f.emulator) else {
            continue;
        };
        for platform in emulator.platforms() {
            match shared.iter_mut().find(|(p, _)| p == platform) {
                Some((_, ids)) if !ids.contains(&f.emulator) => ids.push(f.emulator),
                Some(_) => {}
                None => shared.push((platform, vec![f.emulator])),
            }
        }
    }
    shared.retain(|(_, ids)| ids.len() > 1);
    shared
}

/// Whether this OS has detection.
pub fn supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux", windows))
}

/// Whether `emulator` is detected on this OS.
pub fn detectable(emulator: &dyn Emulator) -> bool {
    if cfg!(target_os = "macos") {
        emulator.mac_app().is_some()
    } else if cfg!(target_os = "linux") {
        emulator.linux_app().is_some()
    } else if cfg!(windows) {
        emulator.windows_app().is_some()
    } else {
        false
    }
}

/// Look in the places this OS installs apps, and with `everywhere` also
/// further afield (macOS: wherever Spotlight knows an emulator's app to be;
/// Linux and Windows: Desktop, Downloads, Documents, and on Linux `PATH`).
///
/// `everywhere` is for the CLI: reading an app on the Desktop or in Downloads
/// from the background daemon would make macOS ask the user, out of nowhere,
/// to let rommd access that folder.
pub fn detect(emulators: &[Arc<dyn Emulator>], everywhere: bool) -> Detection {
    if cfg!(target_os = "macos") {
        mac::detect(emulators, everywhere)
    } else if cfg!(target_os = "linux") {
        linux::detect(emulators, everywhere)
    } else if cfg!(windows) {
        windows::detect(emulators, everywhere)
    } else {
        Detection::default()
    }
}

/// Emulators at `path`, which the user named: an app or program, or a folder
/// holding them.
pub fn detect_at(path: &Path, emulators: &[Arc<dyn Emulator>]) -> Detection {
    if cfg!(target_os = "macos") {
        mac::detect_at(path, emulators)
    } else if cfg!(target_os = "linux") {
        linux::detect_at(path, emulators)
    } else if cfg!(windows) {
        windows::detect_at(path, emulators)
    } else {
        Detection::default()
    }
}

/// Directories in `dir`, sorted.
fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    sorted_entries(dir)
        .into_iter()
        .filter(|p| p.is_dir())
        .collect()
}

fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    entries
}
