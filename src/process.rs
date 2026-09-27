use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::process::Child;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::EmulatorConfig;
use crate::emulator::{Emulator, GameSaveDir, LaunchContext, SaveFilter};
use crate::romm::RommClient;
use crate::romm::types::SaveSchema;
use crate::save_archive;

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
    pub folder_save: Option<GameSaveDir>,
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
    // Pre-launch uploads only bootstrap files not yet in RomM. Files already in
    // RomM are downloaded if RomM is newer, otherwise left alone — the post-exit
    // sync uploads anything the game actually modified during the session. This
    // prevents a ping-pong where downloading a file advances its local mtime past
    // RomM's updated_at, triggering an upload next launch, and so on.
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
                }
                // local newer or equal: skip — post-exit sync handles modified files
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

/// RomM slot for directory saves; matches argosy-launcher's latest-save channel.
const AUTOSAVE_SLOT: &str = "autosave";
const BACKUPS_KEPT: usize = 5;

/// Newest RomM directory-save zip that `emulator` can restore, from any
/// emulator in its save family.
fn pick_folder_save<'a>(saves: &'a [SaveSchema], emulator: &dyn Emulator) -> Option<&'a SaveSchema> {
    saves
        .iter()
        .filter(|s| is_folder_save(s) && emulator.accepts_saves_from(s.emulator.as_deref()))
        .max_by_key(|s| parse_romm_time(&s.updated_at))
}

fn is_folder_save(s: &SaveSchema) -> bool {
    s.file_name.to_lowercase().ends_with(".zip")
        && s.slot.as_deref().map_or(true, |slot| slot == AUTOSAVE_SLOT)
}

/// Pre-launch sync for directory saves (Switch). Same rules as flat files:
/// bootstrap-upload only when RomM has nothing, download when RomM is newer,
/// otherwise leave it to the post-exit sync.
pub async fn sync_folder_save_pre_launch(
    romm: &RommClient,
    rom_id: i64,
    emulator: &dyn Emulator,
    save: &GameSaveDir,
    upload_name: &str,
) {
    let romm_saves = match romm.get_saves(rom_id).await {
        Ok(s) => s,
        Err(e) => {
            warn!("could not fetch RomM saves for rom {}: {}", rom_id, e);
            return;
        }
    };
    let local_mtime = save_archive::newest_mtime(&save.dir);

    match (pick_folder_save(&romm_saves, emulator), local_mtime) {
        (None, None) => info!("pre-launch: no save locally or in RomM for {}", save.archive_root),
        (None, Some(_)) => {
            match upload_folder_save(romm, rom_id, emulator.id(), save, upload_name, None).await {
                Ok(()) => info!("pre-launch: uploaded {} (not in RomM)", save.dir.display()),
                Err(e) => warn!("pre-launch upload failed for {}: {:#}", save.dir.display(), e),
            }
        }
        (Some(remote), local) => {
            let romm_newer = match local {
                None => true,
                Some(l) => parse_romm_time(&remote.updated_at).map_or(false, |t| t > l),
            };
            if romm_newer {
                match restore_folder_save(romm, emulator.id(), remote, save).await {
                    Ok(()) => info!(
                        "pre-launch: restored {} from RomM save id={} ({})",
                        save.dir.display(),
                        remote.id,
                        remote.emulator.as_deref().unwrap_or("?")
                    ),
                    Err(e) => warn!("pre-launch restore failed for {}: {:#}", save.dir.display(), e),
                }
            }
        }
    }
}

/// Post-exit sync for directory saves: upload if anything changed during the
/// session. Each emulator keeps its own RomM entry, updated in place.
async fn sync_folder_save(romm: &RommClient, game: &GameProcess, save: &GameSaveDir) {
    match save_archive::newest_mtime(&save.dir) {
        Some(t) if t > game.started_at => {}
        _ => {
            info!("save for {} unchanged this session", save.archive_root);
            return;
        }
    }
    let romm_saves = match romm.get_saves(game.rom_id).await {
        Ok(s) => s,
        Err(e) => {
            warn!("could not fetch RomM saves for rom {}: {}", game.rom_id, e);
            return;
        }
    };
    let own = romm_saves
        .iter()
        .filter(|s| is_folder_save(s) && s.emulator.as_deref() == Some(game.emulator.as_str()))
        .max_by_key(|s| parse_romm_time(&s.updated_at))
        .map(|s| s.id);
    let name = folder_save_upload_name(&game.rom_path);
    match upload_folder_save(romm, game.rom_id, &game.emulator, save, &name, own).await {
        Ok(()) => info!("synced {} to RomM", save.dir.display()),
        Err(e) => warn!("failed to sync {}: {:#}", save.dir.display(), e),
    }
}

/// RomM file name for a directory save: `<rom file stem>.zip`, as argosy does.
pub fn folder_save_upload_name(rom_path: &Path) -> String {
    let stem = rom_path.file_stem().unwrap_or_default().to_string_lossy();
    format!("{}.zip", stem)
}

async fn upload_folder_save(
    romm: &RommClient,
    rom_id: i64,
    emulator: &str,
    save: &GameSaveDir,
    upload_name: &str,
    existing_id: Option<i64>,
) -> Result<()> {
    let work = scratch_dir()?;
    let zip_path = work.join(upload_name);
    let result = async {
        let (dir, root, zp) = (save.dir.clone(), save.archive_root.clone(), zip_path.clone());
        tokio::task::spawn_blocking(move || save_archive::zip_dir(&dir, &root, &zp)).await??;
        match existing_id {
            Some(id) => romm.update_save(id, &zip_path).await,
            None => romm.upload_save(rom_id, emulator, Some(AUTOSAVE_SLOT), &zip_path).await,
        }
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&work).await;
    result
}

async fn restore_folder_save(
    romm: &RommClient,
    emulator: &str,
    remote: &SaveSchema,
    save: &GameSaveDir,
) -> Result<()> {
    let work = scratch_dir()?;
    let zip_path = work.join("download.zip");
    let result = async {
        romm.download_save(remote.id, &zip_path).await?;
        let backups = backups_root().join(emulator).join(&save.archive_root);
        let backup = backups.join(chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string());
        let mut targets = vec![save.dir.clone()];
        targets.extend(save.mirrors.iter().cloned());
        let root = save.archive_root.clone();
        let zp = zip_path.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            save_archive::restore_zip(&zp, &root, &targets, &backup)?;
            prune_backups(&backups);
            Ok(())
        })
        .await?
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&work).await;
    result
}

fn backups_root() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("rommd")
        .join("save-backups")
}

fn prune_backups(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && !p.file_name().unwrap_or_default().to_string_lossy().starts_with('.'))
        .collect();
    entries.sort();
    let excess = entries.len().saturating_sub(BACKUPS_KEPT);
    for old in entries.into_iter().take(excess) {
        let _ = std::fs::remove_dir_all(old);
    }
}

fn scratch_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "rommd-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_nanos()
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
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
    states_dir: Option<PathBuf>,
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
                // Ryujinx only creates a game's save slot on first boot, so a
                // first session starts without one; look again now.
                let folder_save = match (&game.folder_save, &game.game_id) {
                    (Some(s), _) => Some(s.clone()),
                    (None, Some(id)) => emulator.game_saves_dir(&emu_config, id).await,
                    (None, None) => None,
                };
                if let Some(ref save) = folder_save {
                    sync_folder_save(&romm, &game, save).await;
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
                    sync_saves(&romm, &game, &save_filter, &saves_dir, states_dir.as_deref()).await;
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
    states_dir: Option<&Path>,
) {
    if filter.is_unscoped() {
        warn!("skipping save sync: no file prefix available for rom_id={}", game.rom_id);
        return;
    }
    sync_dir(romm, game.rom_id, &game.emulator, saves_dir, &filter.saves, true, game.started_at).await;
    if let Some(sd) = states_dir {
        sync_dir(romm, game.rom_id, &game.emulator, sd, &filter.states, false, game.started_at).await;
    }
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
