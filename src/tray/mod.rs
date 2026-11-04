// SPDX-License-Identifier: MPL-2.0
//! `rommd tray`: an icon in the system tray, a client of the running daemon
//! like `rommd play`. It shows what the daemon is doing (a game running, a
//! ROM downloading), offers the games played last and the update, and runs
//! a Steam shortcut sync. A process of its own, never part of the daemon:
//! the daemon runs without a display, and stops nothing when the tray quits.
//!
//! This module is what the tray shows and does; `linux.rs` puts it on
//! screen. Windows and macOS have no tray yet.

#[cfg(target_os = "linux")]
mod linux;

use anyhow::{Context, Result};
use std::time::Duration;

use crate::client::{Daemon, Launched};
use crate::launches::{LaunchInfo, LaunchState};
use crate::recent::RecentGame;
use crate::server::types::StatusResponse;
use crate::update::UpdateStatus;

/// How often the tray asks the daemon for news; also whenever its menu opens.
const REFRESH: Duration = Duration::from_secs(10);

/// What the tray knows of the daemon.
#[derive(Debug, Clone, Default)]
pub struct View {
    /// `None` while the daemon can't be reached.
    pub status: Option<StatusResponse>,
    pub recent: Vec<RecentGame>,
    pub update: Option<UpdateStatus>,
    /// How the last thing the user asked for went.
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Start a game again as it was started last.
    Play {
        rom_id: i64,
        emulator: String,
        fullscreen: Option<bool>,
        profile: Option<crate::profile::ProfileBlob>,
    },
    Stop {
        pid: u32,
    },
    CancelDownload {
        launch_id: u64,
    },
    CheckUpdate,
    InstallUpdate,
    SteamSync,
    OpenConfig,
    Refresh,
    Quit,
}

/// One line of the menu: a label, what choosing it does (`None`: only a
/// label), or a submenu.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Item {
        label: String,
        action: Option<Action>,
    },
    Submenu {
        label: String,
        entries: Vec<Entry>,
    },
    Separator,
}

fn item(label: impl Into<String>, action: Action) -> Entry {
    Entry::Item {
        label: label.into(),
        action: Some(action),
    }
}

fn text(label: impl Into<String>) -> Entry {
    Entry::Item {
        label: label.into(),
        action: None,
    }
}

/// A game's name by its ROM ID, from the launches or the games played last.
fn game_name(view: &View, status: &StatusResponse, rom_id: i64) -> String {
    status
        .launches
        .iter()
        .find(|l| l.rom_id == rom_id)
        .map(|l| l.name.clone())
        .or_else(|| {
            view.recent
                .iter()
                .find(|g| g.rom_id == rom_id)
                .map(|g| g.name.clone())
        })
        .unwrap_or_else(|| format!("ROM {rom_id}"))
}

fn download_label(launch: &LaunchInfo) -> Option<String> {
    match &launch.state {
        LaunchState::Preparing => Some(format!("Preparing {}…", launch.name)),
        LaunchState::Downloading {
            downloaded_bytes,
            total_bytes: Some(total),
        } if *total > 0 => Some(format!(
            "Downloading {}… {}%",
            launch.name,
            downloaded_bytes * 100 / total
        )),
        LaunchState::Downloading { .. } => Some(format!("Downloading {}…", launch.name)),
        LaunchState::Starting | LaunchState::Ready { .. } => {
            Some(format!("Starting {}…", launch.name))
        }
        _ => None,
    }
}

/// The tray's menu for `view`.
pub fn menu(view: &View) -> Vec<Entry> {
    let mut entries = Vec::new();
    match &view.status {
        None => entries.push(text("rommd isn't running")),
        Some(status) => {
            for game in &status.running {
                let name = game_name(view, status, game.rom_id);
                entries.push(text(format!("Playing {name}")));
                entries.push(item(format!("Stop {name}"), Action::Stop { pid: game.pid }));
            }
            for launch in &status.launches {
                if let Some(label) = download_label(launch) {
                    entries.push(text(label));
                    if matches!(
                        launch.state,
                        LaunchState::Downloading { .. } | LaunchState::Preparing
                    ) {
                        entries.push(item(
                            format!("Cancel {}", launch.name),
                            Action::CancelDownload {
                                launch_id: launch.id,
                            },
                        ));
                    }
                }
            }
            if status.running.is_empty()
                && !status.launches.iter().any(|l| download_label(l).is_some())
            {
                entries.push(text("No game running"));
            }
        }
    }
    if !view.recent.is_empty() {
        let games = view
            .recent
            .iter()
            .map(|g| {
                item(
                    format!("{} ({})", g.name, g.platform),
                    Action::Play {
                        rom_id: g.rom_id,
                        emulator: g.emulator.clone(),
                        fullscreen: g.fullscreen,
                        profile: g.profile.clone(),
                    },
                )
            })
            .collect();
        entries.push(Entry::Submenu {
            label: "Play again".into(),
            entries: games,
        });
    }
    entries.push(Entry::Separator);
    match &view.update {
        Some(u) if u.update_available => {
            let version = u.latest_version.as_deref().unwrap_or("the new version");
            entries.push(item(
                format!("Install rommd {version}"),
                Action::InstallUpdate,
            ));
        }
        Some(_) => entries.push(item("Check for updates", Action::CheckUpdate)),
        None => {}
    }
    entries.push(item("Update Steam shortcuts", Action::SteamSync));
    entries.push(item("Open the config file", Action::OpenConfig));
    entries.push(Entry::Separator);
    if let Some(note) = &view.note {
        entries.push(text(note.clone()));
    }
    entries.push(item("Quit the tray", Action::Quit));
    entries
}

/// One line for the icon's tooltip.
pub fn tooltip(view: &View) -> String {
    match &view.status {
        None => "rommd isn't running".into(),
        Some(status) => {
            if let Some(game) = status.running.first() {
                format!("Playing {}", game_name(view, status, game.rom_id))
            } else if let Some(label) = status.launches.iter().find_map(download_label) {
                label
            } else {
                "rommd".into()
            }
        }
    }
}

async fn refresh(daemon: &Daemon, view: &mut View) {
    view.status = daemon.status().await.ok();
    if view.status.is_some() {
        view.recent = daemon.recent().await.unwrap_or_default();
        view.update = daemon.update(false).await.ok();
    }
}

/// Do what the user chose; a line for the menu on how it went.
async fn perform(daemon: &Daemon, action: Action) -> Option<String> {
    let result: Result<Option<String>> = match action {
        Action::Play {
            rom_id,
            emulator,
            fullscreen,
            profile,
        } => match daemon
            .launch(rom_id, &emulator, false, fullscreen, profile.as_ref())
            .await
        {
            Ok(Launched::Started(_) | Launched::Downloading(_)) => Ok(None),
            Err(e) => Err(e),
        },
        Action::Stop { pid } => daemon.stop(pid).await.map(|_| None),
        Action::CancelDownload { launch_id } => daemon.cancel(launch_id).await.map(|cancelled| {
            (!cancelled).then(|| "Too late to cancel: the download had finished".to_owned())
        }),
        Action::CheckUpdate => daemon.update(true).await.map(|u| {
            Some(match (&u.last_error, u.update_available) {
                (Some(e), _) => format!("Update check failed: {e}"),
                (None, true) => format!(
                    "rommd {} is available",
                    u.latest_version.as_deref().unwrap_or("")
                ),
                (None, false) => "rommd is up to date".to_owned(),
            })
        }),
        Action::InstallUpdate => daemon
            .install_update()
            .await
            .map(|_| Some("Installed; rommd restarts".to_owned())),
        Action::SteamSync => {
            let options = crate::steam::Options::default();
            crate::steam::sync_from_config(false, options)
                .await
                .map(|(changes, _)| {
                    let total = changes.added.len() + changes.kept.len();
                    Some(match (changes.added.len(), changes.removed.len()) {
                        (0, 0) => format!("Steam: {total} shortcuts, up to date"),
                        (added, removed) => format!(
                            "Steam: {total} shortcuts ({added} added, {removed} removed); start Steam to see them"
                        ),
                    })
                })
        }
        Action::OpenConfig => open(&crate::config::Config::path()).map(|_| None),
        Action::Refresh | Action::Quit => Ok(None),
    };
    result.unwrap_or_else(|e| Some(format!("{e:#}")))
}

/// Open `path` in the desktop's default program.
fn open(path: &std::path::Path) -> Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program).arg(path).spawn()?;
    Ok(())
}

// ── The icon ──────────────────────────────────────────────────────────────────

/// rommd's tray icon: one colour, which KDE's tray recolours to the panel's
/// text colour (the SVG's `ColorScheme-Text` class); white elsewhere.
const ICON: &str = include_str!("../../assets/rommd-tray.svg");

/// The folder holding rommd's icon as an icon theme (`<dir>/hicolor/
/// scalable/apps/rommd.svg`), written when it is missing or outdated.
fn icon_theme() -> Result<std::path::PathBuf> {
    let theme = crate::sync::data_dir().join("icons");
    let file = icon_file(&theme);
    if std::fs::read_to_string(&file).ok().as_deref() != Some(ICON) {
        std::fs::create_dir_all(file.parent().unwrap_or(&theme))?;
        std::fs::write(&file, ICON)?;
    }
    Ok(theme)
}

fn icon_file(theme: &std::path::Path) -> std::path::PathBuf {
    theme.join("hicolor/scalable/apps/rommd.svg")
}

// ── Starting at login ─────────────────────────────────────────────────────────

/// Whether this OS has the tray.
pub fn supported() -> bool {
    cfg!(target_os = "linux")
}

/// Whether there is a desktop to show a tray icon on.
pub fn desktop_session() -> bool {
    supported()
        && ["WAYLAND_DISPLAY", "DISPLAY"]
            .iter()
            .any(|v| std::env::var_os(v).is_some())
}

/// The XDG autostart entry desktops (KDE, GNOME, Xfce) start at login.
fn autostart_entry() -> Option<std::path::PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(config.join("autostart/rommd-tray.desktop"))
}

/// Start the tray at login, or with `enabled` false no longer; for
/// `[tray] enabled`. Nothing on an OS without the tray.
pub fn apply_autostart(enabled: bool) -> Result<()> {
    if !supported() {
        return Ok(());
    }
    let entry = autostart_entry().context("finding the autostart folder")?;
    if !enabled {
        match std::fs::remove_file(&entry) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => return Ok(()),
        }
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let icon = icon_theme().map_or_else(
        |_| "input-gaming".into(),
        |t| icon_file(&t).display().to_string(),
    );
    let text = format!(
        "[Desktop Entry]\nType=Application\nName=rommd\nComment=rommd in the system tray\n\
         Exec=\"{}\" tray\nIcon={icon}\nTerminal=false\nNoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n",
        exe.display()
    );
    if let Some(dir) = entry.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&entry, text)?;
    Ok(())
}

/// Whether another `rommd tray` runs.
fn other_tray_running() -> bool {
    let Ok(out) = std::process::Command::new("pgrep")
        .args(["-f", "rommd tray$"])
        .output()
    else {
        return false;
    };
    let me = std::process::id().to_string();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|pid| pid.trim() != me)
}

/// Start the tray now, apart from this process, when there is a desktop and
/// it doesn't run yet.
pub fn start_now() -> Result<()> {
    if !desktop_session() || other_tray_running() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("tray")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn().context("starting the tray")?;
    Ok(())
}

/// Run the tray until the user quits it.
pub async fn run() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if other_tray_running() {
            return Ok(());
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let theme = icon_theme()
            .inspect_err(|e| tracing::warn!("writing the tray icon: {e:#}"))
            .ok();
        let tray = linux::Tray::spawn(tx, theme).await?;
        let daemon = Daemon::new()?;
        let mut view = View::default();
        let mut ticker = tokio::time::interval(REFRESH);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                action = rx.recv() => match action {
                    None | Some(Action::Quit) => break,
                    Some(action) => {
                        if action != Action::Refresh {
                            view.note = Some("Working…".into());
                            tray.show(view.clone()).await;
                            view.note = perform(&daemon, action).await;
                        }
                    }
                },
            }
            refresh(&daemon, &mut view).await;
            tray.show(view.clone()).await;
        }
        tray.shutdown().await;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("rommd tray only runs on Linux so far")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::types::RunningGameInfo;

    fn labels(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                Entry::Item { label, .. } | Entry::Submenu { label, .. } => label.clone(),
                Entry::Separator => "---".into(),
            })
            .collect()
    }

    fn recent() -> Vec<RecentGame> {
        vec![RecentGame {
            rom_id: 35,
            name: "Super Mario 64".into(),
            platform: "Nintendo 64".into(),
            emulator: "rmg".into(),
            played_at: 0,
            fullscreen: Some(true),
            profile: None,
        }]
    }

    #[test]
    fn without_the_daemon() {
        let view = View::default();
        assert_eq!(
            labels(&menu(&view)),
            [
                "rommd isn't running",
                "---",
                "Update Steam shortcuts",
                "Open the config file",
                "---",
                "Quit the tray"
            ]
        );
        assert_eq!(tooltip(&view), "rommd isn't running");
    }

    #[test]
    fn a_game_running_and_one_downloading() {
        let view = View {
            status: Some(StatusResponse {
                running: vec![RunningGameInfo {
                    pid: 42,
                    rom_id: 35,
                    emulator: "rmg".into(),
                    game_id: None,
                    rom_path: "/roms/sm64.z64".into(),
                    started_at: std::time::SystemTime::UNIX_EPOCH,
                }],
                launches: vec![LaunchInfo {
                    id: 7,
                    rom_id: 284,
                    name: "FIFA Street".into(),
                    emulator: "pcsx2".into(),
                    state: LaunchState::Downloading {
                        downloaded_bytes: 450,
                        total_bytes: Some(1000),
                    },
                }],
            }),
            recent: recent(),
            update: None,
            note: Some("Steam: 79 shortcuts, up to date".into()),
        };
        let entries = menu(&view);
        assert_eq!(
            labels(&entries),
            [
                "Playing Super Mario 64",
                "Stop Super Mario 64",
                "Downloading FIFA Street… 45%",
                "Cancel FIFA Street",
                "Play again",
                "---",
                "Update Steam shortcuts",
                "Open the config file",
                "---",
                "Steam: 79 shortcuts, up to date",
                "Quit the tray",
            ]
        );
        assert!(entries.contains(&item("Stop Super Mario 64", Action::Stop { pid: 42 })));
        let Some(Entry::Submenu { entries: games, .. }) =
            entries.iter().find(|e| matches!(e, Entry::Submenu { .. }))
        else {
            panic!()
        };
        assert_eq!(
            games,
            &[item(
                "Super Mario 64 (Nintendo 64)",
                Action::Play {
                    rom_id: 35,
                    emulator: "rmg".into(),
                    fullscreen: Some(true),
                    profile: None,
                }
            )]
        );
        assert_eq!(tooltip(&view), "Playing Super Mario 64");
    }

    #[test]
    fn an_update_to_install() {
        let view = View {
            status: Some(StatusResponse {
                running: vec![],
                launches: vec![],
            }),
            recent: vec![],
            update: Some(UpdateStatus {
                current_version: "0.4.0".into(),
                latest_version: Some("0.4.1".into()),
                update_available: true,
                release_url: None,
                mode: Default::default(),
                last_checked: None,
                last_error: None,
                install_blocker: None,
                installing: false,
            }),
            note: None,
        };
        let names = labels(&menu(&view));
        assert_eq!(names[0], "No game running");
        assert!(names.contains(&"Install rommd 0.4.1".to_owned()));
    }
}
