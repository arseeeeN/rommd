use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::process::Child;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::EmulatorConfig;
use crate::emulator::{Emulator, LaunchContext, SaveFilter};
use crate::romm::RommClient;

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
    /// For emulators with profile-dir saves (e.g. Ryujinx): the resolved save
    /// slot directory. When set, profile-aware sync is used instead of the
    /// standard flat-file sync.
    pub slot_dir: Option<PathBuf>,
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

/// Bidirectional save sync run before a session starts.
///
/// For each local save file:
///   - not in RomM → upload
///   - RomM is newer → download (so the game starts with the latest save)
///   - local is newer or same → upload
///
/// For saves present in RomM but missing locally → download.
pub async fn sync_saves_pre_launch(
    romm: &RommClient,
    rom_id: i64,
    emulator: &str,
    saves_dir: &Path,
    filter: &SaveFilter,
) {
    if filter.saves.is_empty() {
        warn!("skipping pre-launch save sync: no file prefix for rom_id={}", rom_id);
        return;
    }

    let romm_saves = match romm.get_saves(rom_id).await {
        Ok(s) => s,
        Err(e) => {
            warn!("could not fetch RomM saves for rom {}: {}", rom_id, e);
            return;
        }
    };

    let game_id = &filter.saves;
    let local = collect_save_files(saves_dir, game_id).await;

    // ── local files ────────────────────────────────────────────────────────────
    let mut local_names: HashSet<String> = HashSet::new();
    for (path, local_mtime) in &local {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        local_names.insert(name.clone());

        let romm_save = romm_saves.iter().find(|s| s.file_name == name);
        match romm_save {
            None => {
                let slot = extract_slot(&name);
                match romm.upload_save(rom_id, emulator, slot.as_deref(), path).await {
                    Ok(()) => info!("pre-launch: uploaded {} (not in RomM)", path.display()),
                    Err(e) => warn!("pre-launch upload failed for {}: {}", path.display(), e),
                }
            }
            Some(save) => {
                let romm_newer = parse_romm_time(&save.updated_at)
                    .map_or(false, |t| t > *local_mtime);
                if romm_newer {
                    match romm.download_save(save.id, path).await {
                        Ok(()) => info!("pre-launch: downloaded {} (RomM newer)", path.display()),
                        Err(e) => warn!("pre-launch download failed for {}: {}", path.display(), e),
                    }
                } else {
                    let slot = extract_slot(&name);
                    match romm.upload_save(rom_id, emulator, slot.as_deref(), path).await {
                        Ok(()) => info!("pre-launch: uploaded {} (local newer)", path.display()),
                        Err(e) => warn!("pre-launch upload failed for {}: {}", path.display(), e),
                    }
                }
            }
        }
    }

    // ── RomM saves not present locally ────────────────────────────────────────
    for save in &romm_saves {
        if local_names.contains(&save.file_name) {
            continue;
        }
        if !game_id.is_empty() && !save.file_name.starts_with(game_id) {
            continue;
        }
        let dest = saves_dir.join(&save.file_name);
        match romm.download_save(save.id, &dest).await {
            Ok(()) => info!("pre-launch: downloaded {} (not present locally)", dest.display()),
            Err(e) => warn!("pre-launch download failed for {}: {}", dest.display(), e),
        }
    }
}

/// Bidirectional state sync run before a session starts. Same logic as
/// `sync_saves_pre_launch` but against the states endpoint (no slot field).
pub async fn sync_states_pre_launch(
    romm: &RommClient,
    rom_id: i64,
    emulator: &str,
    states_dir: &Path,
    filter: &SaveFilter,
) {
    if filter.states.is_empty() {
        warn!("skipping pre-launch state sync: no file prefix for rom_id={}", rom_id);
        return;
    }

    let romm_states = match romm.get_states(rom_id).await {
        Ok(s) => s,
        Err(e) => {
            warn!("could not fetch RomM states for rom {}: {}", rom_id, e);
            return;
        }
    };

    let game_id = &filter.states;
    let local = collect_save_files(states_dir, game_id).await;

    let mut local_names: HashSet<String> = HashSet::new();
    for (path, local_mtime) in &local {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        local_names.insert(name.clone());

        let romm_state = romm_states.iter().find(|s| s.file_name == name);
        match romm_state {
            None => {
                match romm.upload_state(rom_id, emulator, path).await {
                    Ok(()) => info!("pre-launch: uploaded state {} (not in RomM)", path.display()),
                    Err(e) => warn!("pre-launch state upload failed for {}: {}", path.display(), e),
                }
            }
            Some(state) => {
                let romm_newer = parse_romm_time(&state.updated_at)
                    .map_or(false, |t| t > *local_mtime);
                if romm_newer {
                    match romm.download_state(state.id, path).await {
                        Ok(()) => info!("pre-launch: downloaded state {} (RomM newer)", path.display()),
                        Err(e) => warn!("pre-launch state download failed for {}: {}", path.display(), e),
                    }
                } else {
                    match romm.upload_state(rom_id, emulator, path).await {
                        Ok(()) => info!("pre-launch: uploaded state {} (local newer)", path.display()),
                        Err(e) => warn!("pre-launch state upload failed for {}: {}", path.display(), e),
                    }
                }
            }
        }
    }

    for state in &romm_states {
        if local_names.contains(&state.file_name) {
            continue;
        }
        if !game_id.is_empty() && !state.file_name.starts_with(game_id) {
            continue;
        }
        let dest = states_dir.join(&state.file_name);
        match romm.download_state(state.id, &dest).await {
            Ok(()) => info!("pre-launch: downloaded state {} (not present locally)", dest.display()),
            Err(e) => warn!("pre-launch state download failed for {}: {}", dest.display(), e),
        }
    }
}

/// Pre-launch save sync for profile-dir emulators (Ryujinx).
///
/// `slot_dir` is the resolved save slot (e.g. `bis/user/save/0000000000000001/`).
/// Each immediate subdir of `slot_dir` is a profile (`0/`, `1/`); its name becomes
/// the RomM `slot` parameter. Files within each profile dir are synced flat.
pub async fn sync_profile_saves_pre_launch(
    romm: &RommClient,
    rom_id: i64,
    emulator: &str,
    slot_dir: &Path,
) {
    let romm_saves = match romm.get_saves(rom_id).await {
        Ok(s) => s,
        Err(e) => {
            warn!("could not fetch RomM saves for rom {}: {}", rom_id, e);
            return;
        }
    };

    let mut local_seen: HashSet<(String, String)> = HashSet::new(); // (profile, filename)

    // ── local files ────────────────────────────────────────────────────────────
    // If the slot dir doesn't exist yet (game never run before), there's nothing
    // to upload; we still fall through to download any saves already in RomM.
    if let Ok(mut rd) = tokio::fs::read_dir(slot_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let profile_dir = entry.path();
            if !profile_dir.is_dir() {
                continue;
            }
            let profile = entry.file_name().to_string_lossy().into_owned();

            for (path, local_mtime) in collect_save_files(&profile_dir, "").await {
                let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                local_seen.insert((profile.clone(), name.clone()));

                let romm_save = romm_saves.iter().find(|s| {
                    s.file_name == name && s.slot.as_deref() == Some(&profile)
                });
                match romm_save {
                    None => {
                        match romm.upload_save(rom_id, emulator, Some(&profile), &path).await {
                            Ok(()) => info!("pre-launch: uploaded {}/{} (not in RomM)", profile, name),
                            Err(e) => warn!("pre-launch upload failed for {}: {}", path.display(), e),
                        }
                    }
                    Some(save) => {
                        let romm_newer = parse_romm_time(&save.updated_at)
                            .map_or(false, |t| t > local_mtime);
                        if romm_newer {
                            match romm.download_save(save.id, &path).await {
                                Ok(()) => info!("pre-launch: downloaded {}/{} (RomM newer)", profile, name),
                                Err(e) => warn!("pre-launch download failed for {}: {}", path.display(), e),
                            }
                        } else {
                            match romm.upload_save(rom_id, emulator, Some(&profile), &path).await {
                                Ok(()) => info!("pre-launch: uploaded {}/{} (local newer)", profile, name),
                                Err(e) => warn!("pre-launch upload failed for {}: {}", path.display(), e),
                            }
                        }
                    }
                }
            }
        }
    }

    // ── RomM saves not present locally ────────────────────────────────────────
    for save in &romm_saves {
        if save.emulator.as_deref() != Some(emulator) {
            continue;
        }
        let profile = match &save.slot {
            Some(s) => s.clone(),
            None => continue,
        };
        if local_seen.contains(&(profile.clone(), save.file_name.clone())) {
            continue;
        }
        let profile_dir = slot_dir.join(&profile);
        if let Err(e) = tokio::fs::create_dir_all(&profile_dir).await {
            warn!("could not create profile dir {}: {}", profile_dir.display(), e);
            continue;
        }
        let dest = profile_dir.join(&save.file_name);
        match romm.download_save(save.id, &dest).await {
            Ok(()) => info!("pre-launch: downloaded {}/{} (not present locally)", profile, save.file_name),
            Err(e) => warn!("pre-launch download failed for {}: {}", dest.display(), e),
        }
    }
}

/// Post-exit save sync for profile-dir emulators (Ryujinx).
/// Uploads files modified since session start from each profile subdir.
async fn sync_profile_saves(romm: &RommClient, game: &GameProcess, slot_dir: &Path) {
    let mut rd = match tokio::fs::read_dir(slot_dir).await {
        Ok(e) => e,
        Err(e) => {
            warn!("could not read slot dir {}: {}", slot_dir.display(), e);
            return;
        }
    };

    while let Ok(Some(entry)) = rd.next_entry().await {
        let profile_dir = entry.path();
        if !profile_dir.is_dir() {
            continue;
        }
        let profile = entry.file_name().to_string_lossy().into_owned();

        let mut files = match tokio::fs::read_dir(&profile_dir).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        while let Ok(Some(file_entry)) = files.next_entry().await {
            let path = file_entry.path();
            if path.is_dir() {
                continue;
            }
            let modified = match std::fs::metadata(&path).and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if modified <= game.started_at {
                continue;
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            match romm.upload_save(game.rom_id, &game.emulator, Some(&profile), &path).await {
                Ok(()) => info!("synced {}/{} to RomM", profile, name),
                Err(e) => warn!("failed to sync {}: {}", path.display(), e),
            }
        }
    }
}

async fn collect_save_files(dir: &Path, game_id: &str) -> Vec<(PathBuf, SystemTime)> {
    let mut result = Vec::new();
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return result,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !game_id.is_empty() && !name.starts_with(game_id) {
            continue;
        }
        if let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) {
            result.push((path, mtime));
        }
    }
    result
}

fn parse_romm_time(s: &str) -> Option<SystemTime> {
    use chrono::{DateTime, NaiveDateTime};
    // RFC 3339 / ISO 8601 with timezone (e.g. "2024-01-15T10:30:00Z")
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(SystemTime::from(dt));
    }
    // Python datetime without timezone — assume UTC (e.g. "2024-01-15T10:30:00.123456")
    for fmt in &["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(SystemTime::from(ndt.and_utc()));
        }
    }
    None
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
    states_dir: PathBuf,
) {
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) => info!("game pid={} exited: {}", pid, status),
            Err(e) => warn!("error waiting for pid={}: {}", pid, e),
        }

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
                if let Some(ref slot_dir) = game.slot_dir {
                    sync_profile_saves(&romm, &game, slot_dir).await;
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
                    sync_saves(&romm, &game, &save_filter, &saves_dir, &states_dir).await;
                }
            }
        }
    });
}

async fn sync_saves(
    romm: &RommClient,
    game: &GameProcess,
    filter: &SaveFilter,
    saves_dir: &Path,
    states_dir: &Path,
) {
    if filter.is_unscoped() {
        warn!("skipping save sync: no file prefix available for rom_id={}", game.rom_id);
        return;
    }
    sync_dir(romm, game.rom_id, &game.emulator, saves_dir, &filter.saves, true, game.started_at).await;
    sync_dir(romm, game.rom_id, &game.emulator, states_dir, &filter.states, false, game.started_at).await;
}

/// Upload files in `dir` modified after `since`. When `game_id` is non-empty,
/// only files whose name starts with it are considered (filters to current game).
async fn sync_dir(
    romm: &RommClient,
    rom_id: i64,
    emulator: &str,
    dir: &Path,
    game_id: &str,
    is_saves: bool,
    since: SystemTime,
) {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(e) => {
            warn!("could not read {}: {}", dir.display(), e);
            return;
        }
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }

        let name = path.file_name().unwrap_or_default().to_string_lossy();

        if !game_id.is_empty() && !name.starts_with(game_id) {
            tracing::trace!("sync skip (game_id) {}", path.display());
            continue;
        }

        let modified = match std::fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if modified <= since {
            tracing::debug!(
                "sync skip (not modified since session start) {}",
                path.display()
            );
            continue;
        }

        let result = if is_saves {
            let slot = extract_slot(&name);
            romm.upload_save(rom_id, emulator, slot.as_deref(), &path).await
        } else {
            romm.upload_state(rom_id, emulator, &path).await
        };

        match result {
            Ok(()) => info!("synced {} to RomM", path.display()),
            Err(e) => warn!("failed to sync {}: {}", path.display(), e),
        }
    }
}

/// Try to extract a slot from a filename: "SLUS-00255_1.mcd" → Some("1").
fn extract_slot(name: &str) -> Option<String> {
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    stem.rsplit_once('_').map(|(_, slot)| slot.to_owned())
}

fn kill_pid(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let nix_pid = Pid::from_raw(pid as i32);
        kill(nix_pid, Signal::SIGTERM)
            .map_err(|e| anyhow::anyhow!("kill(SIGTERM) failed: {}", e))?;
        // Some emulators (e.g. DuckStation) show a confirmation popup on SIGTERM
        // and won't exit until dismissed. After 5 s we force-kill so the monitor
        // task can complete and sync saves.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let _ = kill(nix_pid, Signal::SIGKILL);
        });
    }
    #[cfg(windows)]
    {
        // Post WM_CLOSE to all windows owned by the process (graceful, like SIGTERM).
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .output();
        // Force-kill after 5 s in case the process shows a confirmation dialog.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .output();
        });
    }
    Ok(())
}
