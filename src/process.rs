use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::process::Child;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::EmulatorConfig;
use crate::emulator::{Emulator, GameSaveDir, LaunchContext, SaveFilter};
use crate::romm::RommClient;
use crate::sync::{self, SyncCtx, SyncStore};

#[derive(Debug)]
pub struct GameProcess {
    pub rom_id: i64,
    pub emulator: String,
    /// Emulator-specific game identifier used for profile snapshots and status display.
    /// For DuckStation this is the disc serial; other emulators may leave it None.
    pub game_id: Option<String>,
    /// Filename prefixes used to scope save/savestate sync to this game.
    pub save_filter: SaveFilter,
    pub rom_path: PathBuf,
    pub started_at: SystemTime,
    pub launch_ctx: LaunchContext,
    /// For emulators with directory saves (Switch): the resolved save dir. When
    /// set, whole-directory zip sync is used instead of flat-file sync.
    pub dir_save: Option<GameSaveDir>,
}

#[derive(Debug, Clone)]
pub struct RunningGame {
    pub pid: u32,
    pub rom_id: i64,
    pub emulator: String,
    pub game_id: Option<String>,
    pub rom_path: PathBuf,
    pub started_at: SystemTime,
}

#[derive(Clone)]
pub struct ProcessManager {
    inner: Arc<Mutex<HashMap<u32, GameProcess>>>,
}

impl ProcessManager {
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub async fn add(&self, pid: u32, process: GameProcess) {
        self.inner.lock().await.insert(pid, process);
    }

    pub async fn remove(&self, pid: u32) -> Option<GameProcess> {
        self.inner.lock().await.remove(&pid)
    }

    pub async fn list(&self) -> Vec<RunningGame> {
        self.inner
            .lock()
            .await
            .iter()
            .map(|(pid, p)| RunningGame {
                pid: *pid,
                rom_id: p.rom_id,
                emulator: p.emulator.clone(),
                game_id: p.game_id.clone(),
                rom_path: p.rom_path.clone(),
                started_at: p.started_at,
            })
            .collect()
    }

    /// Return the game_id of the first running game on the given emulator.
    pub async fn game_id_for_emulator(&self, emulator: &str) -> Option<String> {
        self.inner
            .lock()
            .await
            .values()
            .find(|p| p.emulator == emulator)
            .and_then(|p| p.game_id.clone())
    }

    pub async fn kill(&self, pid: u32) -> Result<()> {
        kill_pid(pid)
    }
}

/// Spawn a background task that waits for the child to exit, then syncs
/// saves/states to RomM and cleans up any launch artifacts.
pub fn monitor(
    pid: u32,
    mut child: Child,
    manager: ProcessManager,
    romm: Option<Arc<RommClient>>,
    emulator: Arc<dyn Emulator>,
    emu_config: EmulatorConfig,
    saves_dir: PathBuf,
    states_dir: Option<PathBuf>,
    store: Arc<SyncStore>,
) {
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) => info!("game pid={} exited: {}", pid, status),
            Err(e) => warn!("error waiting for pid={}: {}", pid, e),
        }
        wait_for_process_group(pid).await;

        if let Some(game) = manager.remove(pid).await {
            for path in &game.launch_ctx.temp_files {
                if let Err(e) = tokio::fs::remove_file(path).await {
                    warn!("could not remove temp file {}: {}", path.display(), e);
                }
            }
            for dir in &game.launch_ctx.temp_dirs {
                if let Err(e) = tokio::fs::remove_dir_all(dir).await {
                    warn!("could not remove temp dir {}: {}", dir.display(), e);
                }
            }

            if let Some(romm) = romm {
                let ctx = SyncCtx {
                    romm: &romm,
                    store: &store,
                    rom_id: game.rom_id,
                    emulator: emulator.as_ref(),
                };
                // Ryujinx only creates a game's save slot on first boot, so a
                // first session starts without one; look again now.
                let (dir_save, created_this_session) = match (&game.dir_save, &game.game_id) {
                    (Some(s), _) => (Some(s.clone()), false),
                    (None, Some(id)) => (emulator.game_saves_dir(&emu_config, id).await, true),
                    (None, None) => (None, false),
                };
                if let Some(ref save) = dir_save {
                    sync::sync_dir_save(&ctx, save, &game.rom_path, created_this_session).await;
                } else {
                    // Re-derive the save filter now that the emulator has exited.
                    // DuckStation (and others) write their gamelist cache on shutdown,
                    // so a first-session game that had no title entry at launch time
                    // will have one now — giving us the proper memcard filename prefix.
                    let save_filter = {
                        let fresh = emulator.save_filter(
                            &emu_config,
                            &game.rom_path,
                            game.game_id.as_deref(),
                        );
                        if !fresh.saves.is_empty() { fresh } else { game.save_filter.clone() }
                    };
                    sync::sync_flat(&ctx, &saves_dir, states_dir.as_deref(), &save_filter).await;
                }
            }
        }
    });
}

/// Stop a game gracefully, then force it after 5 s. Signals go to the whole
/// process group (see `emulator_command`), so emulators behind a launcher
/// process are stopped too.
fn kill_pid(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let pgid = Pid::from_raw(pid as i32);
        killpg(pgid, Signal::SIGTERM)
            .map_err(|e| anyhow::anyhow!("killpg(SIGTERM) failed: {}", e))?;
        // Some emulators show a confirmation popup on SIGTERM (DuckStation) or
        // ignore it while a game runs (Eden). After 5 s we force-kill so the
        // monitor task can complete and sync saves.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let _ = killpg(pgid, Signal::SIGKILL);
        });
    }
    #[cfg(windows)]
    {
        // Post WM_CLOSE to the process tree (graceful, like SIGTERM).
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/PID", &pid.to_string()])
            .output();
        // Force-kill after 5 s in case the process shows a confirmation dialog.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .output();
        });
    }
    Ok(())
}

/// Wait until no process of the game's process group is left. The spawned
/// process can exit before the emulator does (a uruntime AppImage parent that
/// was killed), and syncing then would read saves the emulator may still write.
async fn wait_for_process_group(pid: u32) {
    #[cfg(unix)]
    {
        use nix::sys::signal::killpg;
        use nix::unistd::Pid;
        let pgid = Pid::from_raw(pid as i32);
        let mut logged = false;
        while killpg(pgid, None).is_ok() {
            if !logged {
                info!("game pid={} exited; waiting for the rest of its process group", pid);
                logged = true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
    #[cfg(windows)]
    let _ = pid;
}
