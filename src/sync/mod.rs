//! Save and state sync with RomM.
//!
//! Every save is a *unit* paired with at most one RomM entry: a single file, a
//! directory (Switch, Wii) or a bundle of files (GameCube), the latter two as
//! one argosy-compatible zip. Each unit is synced
//! by a three-way comparison of its local content hash, the RomM entry, and
//! the state both were in at the last sync (`SyncStore`). The same logic runs
//! before launch and after exit.
//!
//! Hashes follow RomM's `content_hash` scheme, so identical content is
//! recognised as in sync even without history (e.g. a save uploaded by
//! argosy-launcher). Real conflicts are resolved newest-wins by timestamp, and
//! the losing version is always kept in the backup dir.

pub mod decide;
#[cfg(test)]
mod scenarios;
pub mod state;

use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::{debug, info, warn};

use crate::emulator::{Emulator, FileSet, GameSaveDir, SaveLayout, SaveSet};
use crate::romm::RommClient;
use crate::romm::types::{SaveSchema, StateSchema};
use crate::save_archive;
use decide::{Action, RemoteView, decide};
pub use state::{ConflictRecord, SyncBase, SyncStore};

/// RomM slot for archive saves; matches argosy-launcher's latest-save channel.
/// A slot is a named save history: each upload adds a version, and the newest
/// one is the slot's current save.
const AUTOSAVE_SLOT: &str = "autosave";
/// Versions RomM keeps per slot, as argosy-launcher does.
const SLOT_VERSIONS_KEPT: u32 = 10;
const BACKUPS_KEPT: usize = 5;

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("rommd")
}

pub struct SyncCtx<'a> {
    pub romm: &'a RommClient,
    pub store: &'a SyncStore,
    pub rom_id: i64,
    pub emulator: &'a dyn Emulator,
}

/// Sync every save and state in `layout`. Returns the sync-state keys of the
/// units that exist locally afterwards.
///
/// Pass the set returned before launch as `present_at_launch` when syncing
/// after exit: a unit missing from it had no local save when the game started
/// (a first boot, or a restore that failed while offline), so whatever the game
/// wrote is a fresh start. On conflict such a save never wins over an existing
/// RomM save, however new it is.
pub async fn sync_layout(
    ctx: &SyncCtx<'_>,
    layout: &SaveLayout,
    rom_path: &Path,
    present_at_launch: Option<&HashSet<String>>,
) -> HashSet<String> {
    let mut units = Vec::new();
    let mut listing_failed = false;
    if let Some(saves) = &layout.saves {
        match units_for_saves(ctx, saves, rom_path).await {
            Ok(u) => units.extend(u),
            Err(u) => {
                units.extend(u);
                listing_failed = true;
            }
        }
    }
    if let Some(states) = &layout.states {
        match flat_units(ctx, Kind::State, states).await {
            Ok(u) => units.extend(u),
            Err(u) => {
                units.extend(u);
                listing_failed = true;
            }
        }
    }

    let mut present = HashSet::new();
    for mut unit in units {
        let key = unit_key(ctx, &unit);
        unit.local_is_fresh = present_at_launch.is_some_and(|p| !p.contains(&key));
        // Without the RomM listing nothing can be decided, but local presence
        // still matters for the post-exit pass.
        let exists = if listing_failed && unit.remote.is_none() {
            local_hash(&unit.local).await.ok().flatten().is_some()
        } else {
            sync_unit(ctx, &unit, &key).await
        };
        if exists {
            present.insert(key);
        }
    }
    present
}

/// Units for the game's saves. On a RomM listing error, returns (as `Err`) the
/// local units without remotes so presence can still be recorded.
async fn units_for_saves(ctx: &SyncCtx<'_>, saves: &SaveSet, rom_path: &Path) -> Result<Vec<Unit>, Vec<Unit>> {
    let local = match saves {
        SaveSet::Files(set) => return flat_units(ctx, Kind::Save, set).await,
        SaveSet::Dir(dir) => Local::Dir(dir.clone()),
        SaveSet::Bundle(set) => Local::Bundle(set.clone()),
    };
    let mut unit = Unit {
        kind: Kind::Save,
        local,
        remote: None,
        own_remote: None,
        slot: Some(AUTOSAVE_SLOT.to_owned()),
        keep_versions: Some(SLOT_VERSIONS_KEPT),
        upload_name: archive_upload_name(rom_path),
        local_is_fresh: false,
    };
    let remotes = match Kind::Save.list(ctx).await {
        Ok(r) => r,
        Err(e) => {
            warn!("could not fetch RomM saves for rom {}: {:#}", ctx.rom_id, e);
            return Err(vec![unit]);
        }
    };
    // One archive per game: the slot's newest zip from any compatible emulator
    // is the current save. Uploads add a version to the slot.
    unit.remote = newest(
        remotes
            .iter()
            .filter(|r| is_archive_save(r) && ctx.emulator.accepts_saves_from(r.emulator.as_deref())),
    )
    .cloned();
    Ok(vec![unit])
}

/// One unit per file: the local files in `set`, plus RomM entries for this game
/// that have no local file yet.
async fn flat_units(ctx: &SyncCtx<'_>, kind: Kind, set: &FileSet) -> Result<Vec<Unit>, Vec<Unit>> {
    let local_files = {
        let set = set.clone();
        tokio::task::spawn_blocking(move || set.files()).await.unwrap_or_default()
    };
    let unit = |path: PathBuf, group: &[Remote], history: bool| {
        let own = newest(group.iter().filter(|r| r.emulator.as_deref() == Some(ctx.emulator.id())));
        Unit {
            kind,
            upload_name: path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            local: Local::File(path),
            remote: newest(group.iter()).cloned(),
            own_remote: own.map(|r| r.id),
            slot: history.then(|| AUTOSAVE_SLOT.to_owned()),
            keep_versions: history.then_some(SLOT_VERSIONS_KEPT),
            local_is_fresh: false,
        }
    };

    let remotes = match kind.list(ctx).await {
        Ok(r) => r,
        Err(e) => {
            warn!("could not fetch RomM {}s for rom {}: {:#}", kind.label(), ctx.rom_id, e);
            return Err(local_files.into_iter().map(|p| unit(p, &[], false)).collect());
        }
    };
    let local_name = |r: &Remote| local_file_name(&r.file_name).map(|n| set.local_name(&n));
    // Names already in the autosave slot, from any emulator or client: RomM's
    // cleanup counts them all together.
    let slot_names: BTreeSet<String> = remotes
        .iter()
        .filter(|r| r.slot.as_deref() == Some(AUTOSAVE_SLOT))
        .filter_map(local_name)
        .collect();
    let local_count = local_files.len();
    let history = |name: &str| kind == Kind::Save && single_file_history_ok(&slot_names, local_count, name);
    // RomM tags stored names with an upload timestamp; group entries by the
    // name the emulator actually uses. Older rommd versions created duplicates,
    // so a group can hold several entries — the newest one is used.
    let mut groups: BTreeMap<String, Vec<Remote>> = BTreeMap::new();
    for r in remotes {
        if !ctx.emulator.accepts_saves_from(r.emulator.as_deref()) {
            continue;
        }
        match local_name(&r) {
            Some(name) => groups.entry(name).or_default().push(r),
            None => warn!("ignoring RomM {} with unusable name {:?}", kind.label(), r.file_name),
        }
    }

    let mut units = Vec::new();
    for path in local_files {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let group = groups.remove(&name).unwrap_or_default();
        units.push(unit(path, &group, history(&name)));
    }
    for (name, group) in groups {
        if set.matcher.matches_name(&name) {
            units.push(unit(set.dir.join(&name), &group, history(&name)));
        }
    }
    Ok(units)
}

/// Whether a single-file save can keep a version history in the autosave
/// slot. RomM's cleanup keeps the newest N saves per ROM and slot regardless of
/// file name, so this is only safe while the game has one save file and the
/// slot holds no other file — otherwise one file's versions could delete
/// another file's only copy. When it isn't, the file updates in place.
fn single_file_history_ok(slot_names: &BTreeSet<String>, local_files: usize, name: &str) -> bool {
    local_files <= 1 && slot_names.iter().all(|n| n == name)
}

/// RomM file name for an archive save: `<rom file stem>.zip`, as argosy does.
fn archive_upload_name(rom_path: &Path) -> String {
    let stem = rom_path.file_stem().unwrap_or_default().to_string_lossy();
    format!("{}.zip", stem)
}

// ── Units ─────────────────────────────────────────────────────────────────────

struct Unit {
    kind: Kind,
    local: Local,
    /// The RomM entry this unit syncs against: the newest compatible one.
    remote: Option<Remote>,
    /// For single files: this emulator's own entry, updated in place on
    /// upload. Other emulators' entries are never overwritten.
    own_remote: Option<i64>,
    slot: Option<String>,
    /// For archive saves: upload a new version into `slot`, keeping this many.
    /// Single files never use slot history: several files of one game would
    /// share the slot, and RomM's cleanup would delete one file's only copy to
    /// keep another's versions.
    keep_versions: Option<u32>,
    upload_name: String,
    /// The local save is a fresh start and must not win a conflict.
    local_is_fresh: bool,
}

#[derive(Debug, Clone)]
enum Local {
    File(PathBuf),
    Dir(GameSaveDir),
    Bundle(FileSet),
}

impl Local {
    fn path(&self) -> &Path {
        match self {
            Local::File(p) => p,
            Local::Dir(s) => &s.dir,
            Local::Bundle(set) => &set.dir,
        }
    }

    /// Archive entries (local file, name in the zip) for archive saves.
    fn entries(&self) -> Result<Vec<(PathBuf, String)>> {
        match self {
            Local::File(_) => unreachable!("single files are not archived"),
            Local::Dir(s) => {
                let primary_present = s
                    .required_subdir
                    .is_none_or(|sub| save_archive::newest_mtime(&s.dir.join(sub)).is_some());
                let mut entries = if primary_present {
                    save_archive::dir_entries(&s.dir, &s.archive_root)?
                } else {
                    Vec::new()
                };
                for (root, dir) in &s.extra_roots {
                    entries.extend(save_archive::dir_entries(dir, root)?);
                }
                Ok(entries)
            }
            Local::Bundle(set) => Ok(set
                .files()
                .into_iter()
                .map(|p| {
                    let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    (p, name)
                })
                .collect()),
        }
    }

    /// Empty directories to keep in a directory-save zip.
    fn empty_dirs(&self) -> Result<Vec<String>> {
        let Local::Dir(s) = self else { return Ok(Vec::new()) };
        let mut dirs = save_archive::dir_empty_dirs(&s.dir, &s.archive_root)?;
        for (root, dir) in &s.extra_roots {
            dirs.extend(save_archive::dir_empty_dirs(dir, root)?);
        }
        Ok(dirs)
    }

    fn hash(&self) -> Result<Option<String>> {
        match self {
            Local::File(p) if p.is_file() => Ok(Some(save_archive::file_md5(p)?)),
            Local::File(_) => Ok(None),
            _ => save_archive::entries_hash(&self.entries()?),
        }
    }

    fn mtime(&self) -> Option<SystemTime> {
        match self {
            Local::File(p) => std::fs::metadata(p).and_then(|m| m.modified()).ok(),
            _ => save_archive::entries_mtime(&self.entries().ok()?),
        }
    }
}

async fn local_hash(local: &Local) -> Result<Option<String>> {
    let local = local.clone();
    tokio::task::spawn_blocking(move || local.hash()).await?
}

fn unit_key(ctx: &SyncCtx<'_>, unit: &Unit) -> String {
    let local = match &unit.local {
        Local::Bundle(set) => format!("{}#{}", set.dir.display(), set.matcher.label()),
        other => other.path().display().to_string(),
    };
    format!("{}:{}:{}:{}", ctx.emulator.id(), ctx.rom_id, unit.kind.label(), local)
}

/// Sync one unit. Returns whether it exists locally afterwards.
async fn sync_unit(ctx: &SyncCtx<'_>, unit: &Unit, key: &str) -> bool {
    let path = unit.local.path().display().to_string();
    let hash = match local_hash(&unit.local).await {
        Ok(h) => h,
        Err(e) => {
            warn!("could not read local {} {}: {:#}", unit.kind.label(), path, e);
            return true;
        }
    };
    let base = ctx.store.base(key).await;
    let view = unit.remote.as_ref().map(|r| RemoteView {
        id: r.id,
        hash: r.content_hash.as_deref(),
        updated_at: &r.updated_at,
    });

    let had_local = hash.is_some();
    let mut action = decide(hash.as_deref(), view, base.as_ref());
    if action == Action::Conflict && base.is_none()
        && let (Some(h), Some(r)) = (&hash, &unit.remote)
            && r.content_hash.is_none() && remote_matches(ctx, unit, r, h).await {
                action = Action::InSync;
            }
    let result = match action {
        Action::Nothing => {
            debug!("{} unchanged", path);
            Ok(had_local)
        }
        Action::InSync => {
            let (Some(h), Some(r)) = (hash, &unit.remote) else { unreachable!("in sync needs both sides") };
            ctx.store.set_base(key, r.base_for(h)).await;
            debug!("{} already in sync with RomM id={}", path, r.id);
            Ok(true)
        }
        Action::Upload => upload(ctx, unit, key, hash.unwrap()).await.map(|_| true),
        Action::Download => download(ctx, unit, key, unit.remote.as_ref().unwrap()).await.map(|_| true),
        Action::Conflict => resolve_conflict(ctx, unit, key, hash.unwrap()).await.map(|_| true),
    };
    match result {
        Ok(exists) => exists,
        Err(e) => {
            warn!("{} sync failed for {}: {:#}", unit.kind.label(), path, e);
            had_local
        }
    }
}

/// Whether RomM's copy has this content, by downloading and hashing it. Only
/// used when RomM gives no `content_hash` (save states) and there is no sync
/// history: without it every state on a new device would be a conflict.
async fn remote_matches(ctx: &SyncCtx<'_>, unit: &Unit, remote: &Remote, local_hash: &str) -> bool {
    let Ok(work) = scratch_dir() else { return false };
    let file = work.join("probe");
    let matches = match unit.kind.download(ctx, remote.id, &file).await {
        Ok(()) => {
            let local = unit.local.clone();
            tokio::task::spawn_blocking(move || match local {
                Local::File(_) => save_archive::file_md5(&file).ok(),
                _ => save_archive::zip_content_hash(&file).ok(),
            })
            .await
            .ok()
            .flatten()
            .is_some_and(|h| h == local_hash)
        }
        Err(e) => {
            debug!("could not fetch RomM {} {} to compare: {:#}", unit.kind.label(), remote.id, e);
            false
        }
    };
    let _ = tokio::fs::remove_dir_all(&work).await;
    matches
}

async fn upload(ctx: &SyncCtx<'_>, unit: &Unit, key: &str, hash: String) -> Result<()> {
    let work = scratch_dir()?;
    let result = async {
        let file = match &unit.local {
            Local::File(p) => p.clone(),
            archive => {
                let zip = work.join(&unit.upload_name);
                let (local, dest) = (archive.clone(), zip.clone());
                tokio::task::spawn_blocking(move || {
                    save_archive::zip_entries_with_dirs(&local.entries()?, &local.empty_dirs()?, &dest)
                })
                .await??;
                zip
            }
        };
        // A save with history adds a version; otherwise update in place.
        let remote = match (unit.keep_versions, unit.own_remote) {
            (None, Some(id)) => unit.kind.update(ctx, id, &file).await?,
            _ => unit.kind.create(ctx, unit.slot.as_deref(), unit.keep_versions, &file).await?,
        };
        if remote.content_hash.as_deref().is_some_and(|h| h != hash) {
            debug!("RomM hashed {} as {:?}, local hash {}", file.display(), remote.content_hash, hash);
        }
        info!("uploaded {} to RomM id={}", unit.local.path().display(), remote.id);
        ctx.store.set_base(key, remote.base_for(hash)).await;
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&work).await;
    result
}

/// Replace the local save with `remote`, backing up the current one first.
/// Returns where the previous local version was backed up, if there was one.
async fn download(ctx: &SyncCtx<'_>, unit: &Unit, key: &str, remote: &Remote) -> Result<Option<PathBuf>> {
    let work = scratch_dir()?;
    let result = async {
        let tmp = work.join("download");
        unit.kind.download(ctx, remote.id, &tmp).await?;
        let backup = backup_dir(ctx);
        let (local, tmp2, backup2, expected) =
            (unit.local.clone(), tmp.clone(), backup.clone(), remote.content_hash.clone());
        let backed_up = tokio::task::spawn_blocking(move || apply_download(&local, &tmp2, &backup2, expected.as_deref()))
            .await??;
        let hash = local_hash(&unit.local)
            .await?
            .ok_or_else(|| anyhow::anyhow!("restored save is empty"))?;
        ctx.store.set_base(key, remote.base_for(hash)).await;
        info!(
            "restored {} from RomM id={} ({})",
            unit.local.path().display(),
            remote.id,
            remote.emulator.as_deref().unwrap_or("unknown emulator")
        );
        Ok(backed_up.then_some(backup))
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&work).await;
    result
}

/// Blocking part of a download: verify, back up, and put the files in place.
fn apply_download(local: &Local, downloaded: &Path, backup: &Path, expected_hash: Option<&str>) -> Result<bool> {
    let actual = match local {
        Local::File(_) => save_archive::file_md5(downloaded)?,
        _ => save_archive::zip_content_hash(downloaded)?,
    };
    if expected_hash.is_some_and(|h| h != actual) {
        warn!("downloaded content hash {} differs from RomM's {:?}", actual, expected_hash);
    }
    let had_local = local.mtime().is_some();
    match local {
        Local::File(dest) => {
            if dest.is_file() {
                std::fs::create_dir_all(backup)?;
                std::fs::copy(dest, backup.join(dest.file_name().unwrap_or_default()))?;
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Copy then rename so the emulator never sees a half-written save.
            let staged = dest.with_file_name(format!(
                ".{}.rommd-tmp",
                dest.file_name().unwrap_or_default().to_string_lossy()
            ));
            std::fs::copy(downloaded, &staged)?;
            std::fs::rename(&staged, dest)?;
        }
        Local::Dir(save) => {
            let mut primary = vec![save.dir.clone()];
            primary.extend(save.mirrors.iter().cloned());
            let mut roots = vec![(save.archive_root.clone(), primary)];
            roots.extend(save.extra_roots.iter().map(|(root, dir)| (root.clone(), vec![dir.clone()])));
            save_archive::restore_roots(downloaded, &roots, backup)?;
        }
        Local::Bundle(set) => {
            let accept = |p: &Path| {
                let name = p.file_name().unwrap_or_default().to_string_lossy();
                set.matcher.matches_name(&name) && set.matcher.matches_file(p)
            };
            save_archive::restore_bundle(downloaded, &set.dir, &set.files(), accept, backup)?;
        }
    }
    if let Some(parent) = backup.parent() {
        prune_backups(parent);
    }
    Ok(had_local)
}

/// Both sides changed. The newer one wins; the other is preserved first.
async fn resolve_conflict(ctx: &SyncCtx<'_>, unit: &Unit, key: &str, hash: String) -> Result<()> {
    let remote = unit.remote.as_ref().unwrap();
    let local_time = unit.local.mtime();
    let remote_time = parse_romm_time(&remote.updated_at);
    let keep_local = !unit.local_is_fresh
        && match (local_time, remote_time) {
            (Some(l), Some(r)) => l > r,
            (Some(_), None) => true,
            _ => false,
        };

    let loser_backup = if keep_local {
        let dest = backup_dir(ctx).join("romm").join(&remote.file_name);
        unit.kind
            .download(ctx, remote.id, &dest)
            .await
            .map_err(|e| e.context("could not preserve the RomM version; leaving both sides untouched"))?;
        upload(ctx, unit, key, hash).await?;
        Some(dest)
    } else {
        download(ctx, unit, key, remote).await?
    };

    warn!(
        "save conflict for {}: kept {} version (local modified {:?}{}, RomM id={} updated {}); other version kept at {:?}",
        unit.local.path().display(),
        if keep_local { "local" } else { "RomM" },
        local_time.map(rfc3339),
        if unit.local_is_fresh { ", created this session" } else { "" },
        remote.id,
        remote.updated_at,
        loser_backup
    );
    ctx.store
        .record_conflict(ConflictRecord {
            resolved_at: rfc3339(SystemTime::now()),
            rom_id: ctx.rom_id,
            emulator: ctx.emulator.id().to_owned(),
            local_path: unit.local.path().display().to_string(),
            kept: if keep_local { "local" } else { "remote" }.to_owned(),
            local_modified: local_time.map(rfc3339),
            remote_id: remote.id,
            remote_emulator: remote.emulator.clone(),
            remote_updated_at: remote.updated_at.clone(),
            loser_backup: loser_backup.map(|p| p.display().to_string()),
        })
        .await;
    Ok(())
}

// ── RomM entries ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Save,
    State,
}

/// The fields sync needs from a RomM save or state.
#[derive(Debug, Clone)]
struct Remote {
    id: i64,
    file_name: String,
    emulator: Option<String>,
    slot: Option<String>,
    updated_at: String,
    content_hash: Option<String>,
}

impl Remote {
    fn base_for(&self, local_hash: String) -> SyncBase {
        SyncBase {
            local_hash,
            remote_id: self.id,
            remote_hash: self.content_hash.clone(),
            remote_updated_at: self.updated_at.clone(),
        }
    }
}

impl From<SaveSchema> for Remote {
    fn from(s: SaveSchema) -> Self {
        Self {
            id: s.id,
            file_name: s.file_name,
            emulator: s.emulator,
            slot: s.slot,
            updated_at: s.updated_at,
            content_hash: s.content_hash,
        }
    }
}

impl From<StateSchema> for Remote {
    fn from(s: StateSchema) -> Self {
        Self {
            id: s.id,
            file_name: s.file_name,
            emulator: s.emulator,
            slot: None,
            updated_at: s.updated_at,
            content_hash: None,
        }
    }
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Save => "save",
            Kind::State => "state",
        }
    }

    async fn list(self, ctx: &SyncCtx<'_>) -> Result<Vec<Remote>> {
        Ok(match self {
            Kind::Save => ctx.romm.get_saves(ctx.rom_id).await?.into_iter().map(Into::into).collect(),
            Kind::State => ctx.romm.get_states(ctx.rom_id).await?.into_iter().map(Into::into).collect(),
        })
    }

    async fn download(self, ctx: &SyncCtx<'_>, id: i64, dest: &Path) -> Result<()> {
        match self {
            Kind::Save => ctx.romm.download_save(id, dest).await,
            Kind::State => ctx.romm.download_state(id, dest).await,
        }
    }

    async fn create(self, ctx: &SyncCtx<'_>, slot: Option<&str>, keep: Option<u32>, file: &Path) -> Result<Remote> {
        let emulator = ctx.emulator.id();
        Ok(match self {
            Kind::Save => ctx.romm.upload_save(ctx.rom_id, emulator, slot, keep, file).await?.into(),
            Kind::State => ctx.romm.upload_state(ctx.rom_id, emulator, file).await?.into(),
        })
    }

    async fn update(self, ctx: &SyncCtx<'_>, id: i64, file: &Path) -> Result<Remote> {
        Ok(match self {
            Kind::Save => ctx.romm.update_save(id, file).await?.into(),
            Kind::State => ctx.romm.update_state(id, file).await?.into(),
        })
    }
}

fn is_archive_save(r: &Remote) -> bool {
    r.file_name.to_lowercase().ends_with(".zip")
        && r.slot.as_deref().is_none_or(|slot| slot == AUTOSAVE_SLOT)
}

fn newest<'a>(remotes: impl Iterator<Item = &'a Remote>) -> Option<&'a Remote> {
    remotes.max_by_key(|r| parse_romm_time(&r.updated_at))
}

/// The name a RomM entry had locally: RomM appends ` [<upload timestamp>]` to
/// the stem when storing (`anguna [2026-05-05 13-07-23-697].srm`). Returns None
/// for names that are not a plain file name.
fn local_file_name(romm_name: &str) -> Option<String> {
    let (stem, ext) = match romm_name.rsplit_once('.') {
        Some((s, e)) => (s, Some(e)),
        None => (romm_name, None),
    };
    let stem = match stem.rfind(" [") {
        Some(pos) if stem.ends_with(']') && is_romm_timestamp(&stem[pos + 2..stem.len() - 1]) => &stem[..pos],
        _ => stem,
    };
    let name = match ext {
        Some(e) => format!("{}.{}", stem, e),
        None => stem.to_owned(),
    };
    let plain = !name.is_empty()
        && !name.contains(['/', '\\'])
        && name != "."
        && name != ".."
        && !name.starts_with('.');
    plain.then_some(name)
}

/// `YYYY-MM-DD[ _]HH-MM-SS` with an optional `-<millis>` suffix.
fn is_romm_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 19 {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    let base = digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && (b[10] == b' ' || b[10] == b'_')
        && digits(11..13)
        && b[13] == b'-'
        && digits(14..16)
        && b[16] == b'-'
        && digits(17..19);
    base && (b.len() == 19 || (b[19] == b'-' && b.len() > 20 && digits(20..b.len())))
}

pub fn parse_romm_time(s: &str) -> Option<SystemTime> {
    use chrono::{DateTime, NaiveDateTime};
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(SystemTime::from(dt));
    }
    // Python datetime without timezone — assume UTC.
    for fmt in &["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(SystemTime::from(ndt.and_utc()));
        }
    }
    None
}

fn rfc3339(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339()
}

// ── Local files ───────────────────────────────────────────────────────────────

/// `save-backups/<emulator>/<rom_id>/<time>/`; one dir per sync event.
fn backup_dir(ctx: &SyncCtx<'_>) -> PathBuf {
    ctx.store
        .backups_dir()
        .join(ctx.emulator.id())
        .join(ctx.rom_id.to_string())
        .join(chrono::Local::now().format("%Y-%m-%d_%H-%M-%S%.3f").to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_romm_timestamp_tags() {
        assert_eq!(local_file_name("anguna [2026-05-05 13-07-23-697].srm").as_deref(), Some("anguna.srm"));
        assert_eq!(
            local_file_name("Super Mario Odyssey[0100000000010000][V0](axekin.fr) [2026-09-27_09-33-42].zip").as_deref(),
            Some("Super Mario Odyssey[0100000000010000][V0](axekin.fr).zip")
        );
        assert_eq!(
            local_file_name("Mario vs. Donkey Kong (USA, Australia).sav").as_deref(),
            Some("Mario vs. Donkey Kong (USA, Australia).sav")
        );
        // A bracket tag that is not a RomM timestamp is part of the real name.
        assert_eq!(local_file_name("Game [v1].sav").as_deref(), Some("Game [v1].sav"));
    }

    #[test]
    fn single_file_history_only_when_alone_in_the_slot() {
        let slot = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<BTreeSet<_>>();
        assert!(single_file_history_ok(&slot(&[]), 1, "Game.sav"));
        assert!(single_file_history_ok(&slot(&["Game.sav"]), 1, "Game.sav"));
        assert!(single_file_history_ok(&slot(&["Game.sav"]), 0, "Game.sav"));
        assert!(!single_file_history_ok(&slot(&["Game.zip"]), 1, "Game.sav"));
        assert!(!single_file_history_ok(&slot(&[]), 2, "Game_1.mcd"));
    }

    #[test]
    fn rejects_unsafe_names() {
        assert_eq!(local_file_name("../evil.sav"), None);
        assert_eq!(local_file_name("dir/evil.sav"), None);
        assert_eq!(local_file_name(".hidden"), None);
    }
}
