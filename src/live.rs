//! The running config, reloaded when its file changes.
//!
//! The config directory is watched with the OS's file events (inotify,
//! FSEvents, ReadDirectoryChangesW via `notify`), so rommd does no work while
//! the file is untouched. The directory rather than the file is watched,
//! because many editors save by writing a new file and renaming it over the
//! old one. Events are debounced, then the file is read and only applied if
//! its contents changed and parse. A file that doesn't parse leaves the
//! current config in place; the error is logged and shown by `GET /health`.
//! Where file events are unavailable, the file is polled instead.
//!
//! Most settings apply to the next request. A game that is already running
//! keeps the emulator config and RomM client it was launched with. The port
//! needs a restart.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tracing::{info, warn};

use crate::config::Config;
use crate::romm::RommClient;

const DEBOUNCE: Duration = Duration::from_millis(300);
const POLL_FALLBACK: Duration = Duration::from_secs(5);

/// The config and what is built from it, swapped as one.
#[derive(Clone)]
pub struct Snapshot {
    pub config: Arc<Config>,
    pub romm: Option<Arc<RommClient>>,
    /// Web origins allowed to call the API (see `server::security`).
    pub origins: Arc<Vec<String>>,
}

impl Snapshot {
    fn build(config: Config, previous: Option<&Snapshot>) -> Self {
        // Keep the RomM client (and its login) when its settings are unchanged.
        let romm = match previous {
            Some(prev) if prev.config.romm == config.romm => prev.romm.clone(),
            _ => config.romm.as_ref().map(|c| Arc::new(RommClient::new(c))),
        };
        let origins = Arc::new(crate::server::security::allowed_origins(&config));
        Self {
            config: Arc::new(config),
            romm,
            origins,
        }
    }
}

/// What a reload did.
#[derive(Debug, PartialEq)]
pub enum Reload {
    Unchanged,
    /// The new config is active; these sections changed.
    Applied(Vec<Change>),
    /// The file could not be read or parsed; the previous config stays.
    Rejected(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Romm,
    Port,
    AllowedOrigins,
    RomsDir,
    Updates,
    Controllers,
    /// Emulator IDs added, removed or edited.
    Emulators(Vec<String>),
}

pub struct Live {
    path: PathBuf,
    current: RwLock<Snapshot>,
    /// Contents last read, applied or not, so an unchanged file is ignored
    /// and a broken one is reported once.
    last_text: Mutex<Option<String>>,
    error: RwLock<Option<String>>,
    #[cfg(test)]
    reads: std::sync::atomic::AtomicUsize,
}

impl Live {
    pub fn new(path: PathBuf, config: Config) -> Self {
        let last_text = std::fs::read_to_string(&path).ok();
        Self {
            path,
            current: RwLock::new(Snapshot::build(config, None)),
            last_text: Mutex::new(last_text),
            error: RwLock::new(None),
            #[cfg(test)]
            reads: Default::default(),
        }
    }

    pub fn get(&self) -> Snapshot {
        self.current.read().unwrap().clone()
    }

    pub fn config(&self) -> Arc<Config> {
        self.current.read().unwrap().config.clone()
    }

    pub fn romm(&self) -> Option<Arc<RommClient>> {
        self.current.read().unwrap().romm.clone()
    }

    /// Why the config file on disk is not the active config, if it isn't.
    pub fn error(&self) -> Option<String> {
        self.error.read().unwrap().clone()
    }

    pub fn reload(&self) -> Reload {
        #[cfg(test)]
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Some editors delete before writing; the next event brings it back.
                return Reload::Unchanged;
            }
            Err(e) => return self.reject(format!("reading {}: {}", self.path.display(), e)),
        };
        {
            let mut last = self.last_text.lock().unwrap();
            if last.as_deref() == Some(text.as_str()) {
                return Reload::Unchanged;
            }
            *last = Some(text.clone());
        }
        let config = match Config::parse(&text, &self.path) {
            Ok(c) => c,
            Err(e) => return self.reject(format!("{:#}", e)),
        };
        *self.error.write().unwrap() = None;

        let mut current = self.current.write().unwrap();
        let changes = changes(&current.config, &config);
        if changes.is_empty() {
            return Reload::Unchanged;
        }
        let next = Snapshot::build(config, Some(&current));
        *current = next;
        Reload::Applied(changes)
    }

    fn reject(&self, error: String) -> Reload {
        *self.error.write().unwrap() = Some(error.clone());
        Reload::Rejected(error)
    }
}

fn changes(old: &Config, new: &Config) -> Vec<Change> {
    let mut out = Vec::new();
    if old.romm != new.romm {
        out.push(Change::Romm);
    }
    if old.server.port != new.server.port {
        out.push(Change::Port);
    }
    if old.server.allowed_origins != new.server.allowed_origins {
        out.push(Change::AllowedOrigins);
    }
    if old.roms_dir != new.roms_dir {
        out.push(Change::RomsDir);
    }
    if old.updates != new.updates {
        out.push(Change::Updates);
    }
    if old.controllers != new.controllers {
        out.push(Change::Controllers);
    }
    let mut ids: Vec<String> = old
        .emulators
        .keys()
        .chain(new.emulators.keys())
        .filter(|id| old.emulators.get(*id) != new.emulators.get(*id))
        .cloned()
        .collect();
    ids.sort();
    ids.dedup();
    if !ids.is_empty() {
        out.push(Change::Emulators(ids));
    }
    out
}

/// Reload on every change to the config file, calling `on_applied` with the
/// new snapshot and what changed.
pub fn watch<F>(live: Arc<Live>, on_applied: F)
where
    F: Fn(&Snapshot, &[Change]) + Send + Sync + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let watcher = start_watcher(&live.path, tx);
    tokio::spawn(async move {
        // Owning the watcher keeps it alive as long as the task.
        let _watcher = watcher;
        let polling = _watcher.is_none();
        loop {
            if polling {
                tokio::time::sleep(POLL_FALLBACK).await;
            } else {
                if rx.recv().await.is_none() {
                    return;
                }
                // One save can produce several events; take them as one.
                tokio::time::sleep(DEBOUNCE).await;
                while rx.try_recv().is_ok() {}
            }
            match live.reload() {
                Reload::Unchanged => {}
                Reload::Rejected(e) => {
                    warn!(
                        "config change not applied, keeping the current config: {}",
                        e
                    )
                }
                Reload::Applied(changes) => {
                    info!("config reloaded: {}", describe(&changes));
                    on_applied(&live.get(), &changes);
                }
            }
        }
    });
}

fn start_watcher(
    path: &Path,
    tx: tokio::sync::mpsc::UnboundedSender<()>,
) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let dir = path.parent()?.to_path_buf();
    let name = path.file_name()?.to_owned();
    let handler = move |event: notify::Result<notify::Event>| {
        // Only events that can change the file: reading it (as every reload
        // does) raises access events too, and reacting to those would reload
        // in a loop.
        if let Ok(event) = event
            && matches!(
                event.kind,
                notify::EventKind::Create(_)
                    | notify::EventKind::Modify(_)
                    | notify::EventKind::Remove(_)
            )
            && event.paths.iter().any(|p| p.file_name() == Some(&name))
        {
            let _ = tx.send(());
        }
    };
    let result = notify::recommended_watcher(handler)
        .and_then(|mut w| w.watch(&dir, RecursiveMode::NonRecursive).map(|_| w));
    match result {
        Ok(w) => Some(w),
        Err(e) => {
            warn!(
                "cannot watch {} for changes ({}); checking it every {}s instead",
                dir.display(),
                e,
                POLL_FALLBACK.as_secs()
            );
            None
        }
    }
}

pub fn describe(changes: &[Change]) -> String {
    changes
        .iter()
        .map(|c| match c {
            Change::Romm => "RomM connection".to_owned(),
            Change::Port => "port (takes effect after a restart)".to_owned(),
            Change::AllowedOrigins => "allowed origins".to_owned(),
            Change::RomsDir => "roms_dir".to_owned(),
            Change::Updates => "updates".to_owned(),
            Change::Controllers => "controllers".to_owned(),
            Change::Emulators(ids) => format!("emulators ({})", ids.join(", ")),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TempDir, write};

    const BASE: &str = r#"
[romm]
base_url = "https://romm.example.com"
api_token = "t"

[emulators.mgba]
binary = "/usr/bin/mgba"
config_dir = "/home/u/.config/mgba"
"#;

    fn live(dir: &TempDir, text: &str) -> Live {
        let path = dir.path().join("config.toml");
        write(&path, text.as_bytes());
        Live::new(path.clone(), Config::read(&path).unwrap())
    }

    #[test]
    fn applies_changes_and_keeps_the_romm_client_when_romm_is_unchanged() {
        let dir = TempDir::new("live");
        let live = live(&dir, BASE);
        let client = live.romm().unwrap();
        assert_eq!(live.reload(), Reload::Unchanged);

        let edited = BASE.replace("/usr/bin/mgba", "/opt/mgba/mgba")
            + "\n[emulators.dolphin]\nbinary = \"/usr/bin/dolphin-emu\"\nconfig_dir = \"/d\"\n";
        write(&dir.path().join("config.toml"), edited.as_bytes());
        assert_eq!(
            live.reload(),
            Reload::Applied(vec![Change::Emulators(vec![
                "dolphin".into(),
                "mgba".into()
            ])])
        );
        let config = live.config();
        assert_eq!(
            config.emulators["mgba"].binary,
            PathBuf::from("/opt/mgba/mgba")
        );
        assert!(
            Arc::ptr_eq(&client, &live.romm().unwrap()),
            "same RomM client"
        );

        let moved = edited.replace("romm.example.com", "games.example.com");
        write(&dir.path().join("config.toml"), moved.as_bytes());
        assert_eq!(live.reload(), Reload::Applied(vec![Change::Romm]));
        assert!(
            !Arc::ptr_eq(&client, &live.romm().unwrap()),
            "new RomM client"
        );
        assert_eq!(*live.get().origins, ["https://games.example.com"]);
    }

    #[test]
    fn a_broken_file_keeps_the_current_config_until_fixed() {
        let dir = TempDir::new("live-broken");
        let live = live(&dir, BASE);
        write(
            &dir.path().join("config.toml"),
            b"[emulators.mgba\nbinary = ",
        );
        assert!(matches!(live.reload(), Reload::Rejected(_)));
        assert!(live.error().is_some());
        assert!(live.config().emulators.contains_key("mgba"));
        // The same broken contents are not reported again.
        assert_eq!(live.reload(), Reload::Unchanged);

        write(
            &dir.path().join("config.toml"),
            (BASE.to_owned() + "\n[server]\nport = 7600\n").as_bytes(),
        );
        assert_eq!(live.reload(), Reload::Applied(vec![Change::Port]));
        assert!(live.error().is_none());
    }

    #[test]
    fn whitespace_and_comment_edits_change_nothing() {
        let dir = TempDir::new("live-comments");
        let live = live(&dir, BASE);
        write(
            &dir.path().join("config.toml"),
            (BASE.to_owned() + "\n# a note\n").as_bytes(),
        );
        assert_eq!(live.reload(), Reload::Unchanged);
    }

    #[tokio::test]
    async fn file_events_trigger_a_reload() {
        let dir = TempDir::new("live-watch");
        let live = Arc::new(live(&dir, BASE));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        watch(live.clone(), move |snapshot, changes| {
            let _ = tx.send((snapshot.config.emulators.len(), changes.to_vec()));
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Save the way many editors do: write a new file, rename it over the old one.
        let tmp = dir.path().join("config.toml.tmp");
        write(
            &tmp,
            BASE.replace("[emulators.mgba]", "[emulators.mgba2]")
                .as_bytes(),
        );
        std::fs::rename(&tmp, dir.path().join("config.toml")).unwrap();
        let (count, changes) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("reload within 5s")
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            changes,
            vec![Change::Emulators(vec!["mgba".into(), "mgba2".into()])]
        );
    }

    #[tokio::test]
    async fn a_reload_does_not_trigger_another_one() {
        // Reading the file raises access events; reacting to them would make
        // rommd re-read the file (and wake up) forever.
        let dir = TempDir::new("live-quiet");
        let live = Arc::new(live(&dir, BASE));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        watch(live.clone(), move |_, _| {
            let _ = tx.send(());
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        write(
            &dir.path().join("config.toml"),
            BASE.replace("/usr/bin/mgba", "/opt/mgba").as_bytes(),
        );
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("reload within 5s");
        let reads = live.reads.load(std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(DEBOUNCE * 5).await;
        assert_eq!(
            live.reads.load(std::sync::atomic::Ordering::SeqCst),
            reads,
            "the config file kept being re-read after the reload"
        );
    }
}
