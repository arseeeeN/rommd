use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::process::Child;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::EmulatorConfig;
use crate::emulator::{Emulator, LaunchContext, SaveLayout};
use crate::romm::RommClient;
use crate::sync::{self, SyncCtx, SyncStore};

#[derive(Debug)]
pub struct GameProcess {
    pub rom_id: i64,
    pub emulator: String,
    /// Emulator-specific game identifier used for profile snapshots and status display.
    /// For DuckStation this is the disc serial; other emulators may leave it None.
    pub game_id: Option<String>,
    pub rom_path: PathBuf,
    pub started_at: SystemTime,
    pub launch_ctx: LaunchContext,
    /// Where the game's saves were found at launch.
    pub save_layout: SaveLayout,
    /// Sync-state keys of the saves that existed locally at launch; anything
    /// else found after exit was created during the session.
    pub present_at_launch: HashSet<String>,
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
                // Resolve the layout again: the session may have created what
                // was missing at launch (Ryujinx's save slot, DuckStation's
                // title cache entry that names memory cards).
                let layout = emulator
                    .save_layout(&emu_config, &game.rom_path, game.game_id.as_deref())
                    .await
                    .or(game.save_layout.clone());
                sync::sync_layout(&ctx, &layout, &game.rom_path, Some(&game.present_at_launch)).await;
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
