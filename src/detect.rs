//! Finding installed emulators. Each emulator declares how its install is
//! recognised (`Emulator::mac_app`); this module looks for them.
//!
//! Nothing here runs a program: an install is recognised from its files
//! alone, so the same check can vet a path that a client merely suggests (see
//! the config API item in dev/backlog.md).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::emulator::Emulator;

/// How an emulator ships on macOS.
#[derive(Debug, Clone, Copy)]
pub struct MacApp {
    /// `CFBundleIdentifier` in the bundle's `Contents/Info.plist`.
    pub bundle_id: &'static str,
    /// `CFBundleExecutable` to require as well, for a fork that kept its
    /// parent's bundle ID.
    pub executable: Option<&'static str>,
    /// The emulator's data folder (`config_dir`), relative to the home folder.
    pub data_dir: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub emulator: &'static str,
    /// Where it was found, e.g. the `.app` bundle.
    pub install: PathBuf,
    pub binary: PathBuf,
    pub config_dir: PathBuf,
    /// Whether `config_dir` holds the emulator's files; false until it has
    /// run once.
    pub config_dir_in_use: bool,
}

#[derive(Debug, Default)]
pub struct Detection {
    /// At most one install per emulator.
    pub found: Vec<Found>,
    /// Further installs of an emulator in `found`, which a config can't hold
    /// (it has one entry per emulator ID).
    pub also: Vec<Found>,
}

/// Look in the places this OS installs apps. Only macOS is covered so far.
pub fn detect(emulators: &[Arc<dyn Emulator>]) -> Detection {
    let Some(home) = dirs::home_dir() else {
        return Detection::default();
    };
    if cfg!(target_os = "macos") {
        scan_mac(
            &[PathBuf::from("/Applications"), home.join("Applications")],
            &home,
            emulators,
        )
    } else {
        Detection::default()
    }
}

/// `.app` bundles in `app_dirs` and one folder level below (people group
/// emulators in e.g. `/Applications/Emulators`). Earlier dirs win when an
/// emulator is installed twice.
pub fn scan_mac(app_dirs: &[PathBuf], home: &Path, emulators: &[Arc<dyn Emulator>]) -> Detection {
    let mut detection = Detection::default();
    for bundle in app_dirs.iter().flat_map(|d| app_bundles(d)) {
        for emulator in emulators {
            let Some(app) = emulator.mac_app() else {
                continue;
            };
            let Some(binary) = identify_mac_app(&bundle, &app) else {
                continue;
            };
            let config_dir = home.join(app.data_dir);
            let markers = emulator.config_dir_markers();
            let found = Found {
                emulator: emulator.id(),
                config_dir_in_use: config_dir.is_dir()
                    && (markers.is_empty() || markers.iter().any(|m| config_dir.join(m).exists())),
                install: bundle.clone(),
                binary,
                config_dir,
            };
            if detection.found.iter().any(|f| f.emulator == found.emulator) {
                detection.also.push(found);
            } else {
                detection.found.push(found);
            }
        }
    }
    detection
}

fn app_bundles(dir: &Path) -> Vec<PathBuf> {
    let is_app = |p: &Path| p.extension().is_some_and(|e| e == "app");
    let mut bundles = Vec::new();
    for entry in sorted_dirs(dir) {
        if is_app(&entry) {
            bundles.push(entry);
        } else {
            bundles.extend(sorted_dirs(&entry).into_iter().filter(|p| is_app(p)));
        }
    }
    bundles
}

fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// The executable of `bundle` if it is an install of `app`.
pub fn identify_mac_app(bundle: &Path, app: &MacApp) -> Option<PathBuf> {
    let plist = std::fs::read_to_string(bundle.join("Contents/Info.plist")).ok()?;
    if plist_string(&plist, "CFBundleIdentifier")? != app.bundle_id {
        return None;
    }
    let executable = plist_string(&plist, "CFBundleExecutable")?;
    if executable.contains(['/', '\\']) || executable.starts_with('.') {
        return None;
    }
    if app.executable.is_some_and(|e| e != executable) {
        return None;
    }
    let binary = bundle.join("Contents/MacOS").join(executable);
    binary.is_file().then_some(binary)
}

/// The `<string>` following `<key>key</key>` in an XML property list. Binary
/// property lists aren't read; the emulators covered so far ship XML.
fn plist_string(xml: &str, key: &str) -> Option<String> {
    let after = xml.split_once(&format!("<key>{key}</key>"))?.1.trim_start();
    let value = after.strip_prefix("<string>")?.split_once("</string>")?.0;
    Some(
        value
            .trim()
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::{dolphin::Dolphin, eden::Eden, mgba::MGba, ryujinx::Ryujinx};
    use crate::testing::{TempDir, write};

    fn bundle(dir: &Path, name: &str, id: &str, executable: &str) -> PathBuf {
        let app = dir.join(name);
        write(
            &app.join("Contents/Info.plist"),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\
                 \t<key>CFBundleExecutable</key>\n\t<string>{executable}</string>\n\
                 \t<key>CFBundleIdentifier</key>\n\t<string>{id}</string>\n</dict>\n</plist>\n"
            )
            .as_bytes(),
        );
        write(&app.join("Contents/MacOS").join(executable), b"");
        app
    }

    fn emulators() -> Vec<Arc<dyn Emulator>> {
        vec![
            Arc::new(Dolphin),
            Arc::new(Ryujinx),
            Arc::new(MGba),
            Arc::new(Eden),
        ]
    }

    #[test]
    fn finds_apps_by_bundle_id_and_their_data_folders() {
        let root = TempDir::new("detect-mac");
        let (apps, user_apps, home) = (
            root.path().join("Applications"),
            root.path().join("home/Applications"),
            root.path().join("home"),
        );
        // Renamed, and in a subfolder: the bundle ID is what counts.
        bundle(
            &apps.join("Emulators"),
            "Dolphin Beta.app",
            "org.dolphin-emu.dolphin",
            "Dolphin",
        );
        bundle(&apps, "Ryujinx.app", "org.ryujinx.Ryujinx", "Ryujinx");
        bundle(&user_apps, "Ryujinx.app", "org.ryujinx.Ryujinx", "Ryujinx");
        bundle(&apps, "Safari.app", "com.apple.Safari", "Safari");
        write(
            &home.join("Library/Application Support/Ryujinx/Config.json"),
            b"{}",
        );

        let found = scan_mac(&[apps.clone(), user_apps.clone()], &home, &emulators());
        let ids: Vec<_> = found.found.iter().map(|f| f.emulator).collect();
        assert_eq!(ids, vec!["dolphin", "ryujinx"]);

        let dolphin = &found.found[0];
        assert_eq!(
            dolphin.binary,
            apps.join("Emulators/Dolphin Beta.app/Contents/MacOS/Dolphin")
        );
        assert_eq!(
            dolphin.config_dir,
            home.join("Library/Application Support/Dolphin")
        );
        assert!(!dolphin.config_dir_in_use, "Dolphin never ran");

        let ryujinx = &found.found[1];
        assert_eq!(
            ryujinx.install,
            apps.join("Ryujinx.app"),
            "/Applications wins"
        );
        assert!(ryujinx.config_dir_in_use);
        assert_eq!(found.also.len(), 1);
        assert_eq!(found.also[0].install, user_apps.join("Ryujinx.app"));
    }

    #[test]
    fn refuses_bundles_that_only_look_right() {
        let root = TempDir::new("detect-mac-refuse");
        let dolphin = Dolphin.mac_app().unwrap();
        let wrong_id = bundle(root.path(), "Dolphin.app", "org.example.dolphin", "Dolphin");
        assert_eq!(identify_mac_app(&wrong_id, &dolphin), None);

        let escaping = bundle(root.path(), "Evil.app", "org.dolphin-emu.dolphin", "x");
        write(
            &escaping.join("Contents/Info.plist"),
            b"<key>CFBundleIdentifier</key><string>org.dolphin-emu.dolphin</string>\
              <key>CFBundleExecutable</key><string>../../../bin/sh</string>",
        );
        assert_eq!(identify_mac_app(&escaping, &dolphin), None);

        let no_binary = bundle(
            root.path(),
            "Empty.app",
            "org.dolphin-emu.dolphin",
            "Dolphin",
        );
        std::fs::remove_file(no_binary.join("Contents/MacOS/Dolphin")).unwrap();
        assert_eq!(identify_mac_app(&no_binary, &dolphin), None);

        // A fork that kept its parent's bundle ID is told apart by executable.
        let fork = MacApp {
            bundle_id: "com.yuzu-emu.yuzu",
            executable: Some("eden"),
            data_dir: ".local/share/eden",
        };
        let parent = bundle(root.path(), "yuzu.app", "com.yuzu-emu.yuzu", "yuzu");
        assert_eq!(identify_mac_app(&parent, &fork), None);
        let child = bundle(root.path(), "Eden.app", "com.yuzu-emu.yuzu", "eden");
        assert_eq!(
            identify_mac_app(&child, &fork),
            Some(child.join("Contents/MacOS/eden"))
        );

        let binary_plist = bundle(
            root.path(),
            "Binary.app",
            "org.dolphin-emu.dolphin",
            "Dolphin",
        );
        write(
            &binary_plist.join("Contents/Info.plist"),
            b"bplist00\xd1\x01\x02",
        );
        assert_eq!(identify_mac_app(&binary_plist, &dolphin), None);
    }
}
