//! Save and state sync with RomM.
//!
//! Every save is a *unit*: one local file (flat-file emulators) or one
//! directory (Switch), paired with at most one RomM entry. Each unit is synced
//! by a three-way comparison of its local content hash, the RomM entry, and
//! the state both were in at the last sync (`SyncStore`). The same logic runs
//! before launch and after exit.
//!
//! Hashes follow RomM's `content_hash` scheme, so identical content is
//! recognised as in sync even without history (e.g. a save uploaded by
//! argosy-launcher). Real conflicts are resolved newest-wins by timestamp, and
//! the losing version is always kept in the backup dir.

pub mod decide;
pub mod state;

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::{debug, info, warn};

use crate::emulator::{Emulator, GameSaveDir, SaveFilter};
use crate::romm::RommClient;
use crate::romm::types::{SaveSchema, StateSchema};
use crate::save_archive;
use decide::{Action, RemoteView, decide};
pub use state::{ConflictRecord, SyncBase, SyncStore};

/// RomM slot for directory saves; matches argosy-launcher's latest-save channel.
const AUTOSAVE_SLOT: &str = "autosave";
const BACKUPS_KEPT: usize = 5;

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("rommd")
}

fn backups_root() -> PathBuf {
    data_dir().join("save-backups")
}

pub struct SyncCtx<'a> {
    pub romm: &'a RommClient,
    pub store: &'a SyncStore,
    pub rom_id: i64,
    pub emulator: &'a dyn Emulator,
}

/// Sync a directory save (Switch) as one argosy-compatible zip.
pub async fn sync_dir_save(ctx: &SyncCtx<'_>, save: &GameSaveDir, rom_path: &Path) {
    let remotes = match Kind::Save.list(ctx).await {
        Ok(r) => r,
        Err(e) => {
            warn!("could not fetch RomM saves for rom {}: {:#}", ctx.rom_id, e);
            return;
        }
    };
    let zips: Vec<&Remote> = remotes
        .iter()
        .filter(|r| is_dir_save(r) && ctx.emulator.accepts_saves_from(r.emulator.as_deref()))
        .collect();
    let own = newest(zips.iter().copied().filter(|r| r.emulator.as_deref() == Some(ctx.emulator.id())));
    sync_unit(
        ctx,
        Unit {
            kind: Kind::Save,
            local: Local::Dir(save.clone()),
            remote: newest(zips.iter().copied()).cloned(),
            own_remote: own.map(|r| r.id),
            slot: Some(AUTOSAVE_SLOT.to_owned()),
            upload_name: dir_save_upload_name(rom_path),
        },
    )
    .await;
}

/// Sync flat save and state files, scoped by the filename prefixes in `filter`.
/// A kind with an empty prefix is skipped: without a prefix every file in the
/// directory would be attributed to this game.
pub async fn sync_flat(
    ctx: &SyncCtx<'_>,
    saves_dir: &Path,
    states_dir: Option<&Path>,
    filter: &SaveFilter,
) {
    if filter.saves.is_empty() {
        warn!("skipping save sync: no file prefix for rom_id={}", ctx.rom_id);
    } else {
        sync_flat_kind(ctx, Kind::Save, saves_dir, &filter.saves).await;
    }
    if let Some(dir) = states_dir {
        if filter.states.is_empty() {
            warn!("skipping state sync: no file prefix for rom_id={}", ctx.rom_id);
        } else {
            sync_flat_kind(ctx, Kind::State, dir, &filter.states).await;
        }
    }
}

async fn sync_flat_kind(ctx: &SyncCtx<'_>, kind: Kind, dir: &Path, prefix: &str) {
    let remotes = match kind.list(ctx).await {
        Ok(r) => r,
        Err(e) => {
            warn!("could not fetch RomM {}s for rom {}: {:#}", kind.label(), ctx.rom_id, e);
            return;
        }
    };
    // RomM tags stored names with an upload timestamp; group entries by the
    // name the emulator actually uses. Older rommd versions created duplicates,
    // so a group can hold several entries — the newest one is used.
    let mut groups: BTreeMap<String, Vec<Remote>> = BTreeMap::new();
    for r in remotes {
        if !ctx.emulator.accepts_saves_from(r.emulator.as_deref()) {
            continue;
        }
        match local_file_name(&r.file_name) {
            Some(name) => groups.entry(name).or_default().push(r),
            None => warn!("ignoring RomM {} with unusable name {:?}", kind.label(), r.file_name),
        }
    }

    let mut units = Vec::new();
    for path in collect_files(dir, prefix).await {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let group = groups.remove(&name).unwrap_or_default();
        units.push(flat_unit(ctx, kind, path, name, &group));
    }
    for (name, group) in groups {
        if name.starts_with(prefix) {
            units.push(flat_unit(ctx, kind, dir.join(&name), name, &group));
        }
    }
    for unit in units {
        sync_unit(ctx, unit).await;
    }
}

fn flat_unit(ctx: &SyncCtx<'_>, kind: Kind, path: PathBuf, name: String, group: &[Remote]) -> Unit {
    let own = newest(group.iter().filter(|r| r.emulator.as_deref() == Some(ctx.emulator.id())));
    Unit {
        kind,
        local: Local::File(path),
        remote: newest(group.iter()).cloned(),
        own_remote: own.map(|r| r.id),
        slot: match kind {
            Kind::Save => extract_slot(&name),
            Kind::State => None,
        },
        upload_name: name,
    }
}

/// RomM file name for a directory save: `<rom file stem>.zip`, as argosy does.
pub fn dir_save_upload_name(rom_path: &Path) -> String {
    let stem = rom_path.file_stem().unwrap_or_default().to_string_lossy();
    format!("{}.zip", stem)
}

// ── Units ─────────────────────────────────────────────────────────────────────

struct Unit {
    kind: Kind,
    local: Local,
    /// The RomM entry this unit syncs against: the newest compatible one.
    remote: Option<Remote>,
    /// This emulator's own entry, updated in place on upload. Other emulators'
    /// entries are never overwritten.
    own_remote: Option<i64>,
    slot: Option<String>,
    upload_name: String,
}

#[derive(Debug, Clone)]
enum Local {
    File(PathBuf),
    Dir(GameSaveDir),
}

impl Local {
    fn path(&self) -> &Path {
        match self {
            Local::File(p) => p,
            Local::Dir(s) => &s.dir,
        }
    }

    fn hash(&self) -> Result<Option<String>> {
        match self {
            Local::File(p) if p.is_file() => Ok(Some(save_archive::file_md5(p)?)),
            Local::File(_) => Ok(None),
            Local::Dir(s) => save_archive::dir_content_hash(&s.dir, &s.archive_root),
        }
    }

    fn mtime(&self) -> Option<SystemTime> {
        match self {
            Local::File(p) => std::fs::metadata(p).and_then(|m| m.modified()).ok(),
            Local::Dir(s) => save_archive::newest_mtime(&s.dir),
        }
    }
}

async fn local_hash(local: &Local) -> Result<Option<String>> {
    let local = local.clone();
    tokio::task::spawn_blocking(move || local.hash()).await?
}

fn unit_key(ctx: &SyncCtx<'_>, unit: &Unit) -> String {
    format!(
        "{}:{}:{}:{}",
        ctx.emulator.id(),
        ctx.rom_id,
        unit.kind.label(),
        unit.local.path().display()
    )
}

async fn sync_unit(ctx: &SyncCtx<'_>, unit: Unit) {
    let key = unit_key(ctx, &unit);
    let path = unit.local.path().display().to_string();
    let hash = match local_hash(&unit.local).await {
        Ok(h) => h,
        Err(e) => {
            warn!("could not read local {} {}: {:#}", unit.kind.label(), path, e);
            return;
        }
    };
    let base = ctx.store.base(&key).await;
    let view = unit.remote.as_ref().map(|r| RemoteView {
        id: r.id,
        hash: r.content_hash.as_deref(),
        updated_at: &r.updated_at,
    });

    let result = match decide(hash.as_deref(), view, base.as_ref()) {
        Action::Nothing => {
            debug!("{} unchanged", path);
            Ok(())
        }
        Action::InSync => {
            let (Some(h), Some(r)) = (hash, &unit.remote) else { unreachable!() };
            ctx.store.set_base(&key, r.base_for(h)).await;
            debug!("{} already in sync with RomM id={}", path, r.id);
            Ok(())
        }
        Action::Upload => upload(ctx, &unit, &key, hash.unwrap()).await,
        Action::Download => download(ctx, &unit, &key, unit.remote.as_ref().unwrap()).await.map(|_| ()),
        Action::Conflict => resolve_conflict(ctx, &unit, &key, hash.unwrap()).await,
    };
    if let Err(e) = result {
        warn!("{} sync failed for {}: {:#}", unit.kind.label(), path, e);
    }
}

async fn upload(ctx: &SyncCtx<'_>, unit: &Unit, key: &str, hash: String) -> Result<()> {
    let work = scratch_dir()?;
    let result = async {
        let file = match &unit.local {
            Local::File(p) => p.clone(),
            Local::Dir(save) => {
                let zip = work.join(&unit.upload_name);
                let (dir, root, dest) = (save.dir.clone(), save.archive_root.clone(), zip.clone());
                tokio::task::spawn_blocking(move || save_archive::zip_dir(&dir, &root, &dest)).await??;
                zip
            }
        };
        let remote = match unit.own_remote {
            Some(id) => unit.kind.update(ctx, id, &file).await?,
            None => unit.kind.create(ctx, unit.slot.as_deref(), &file).await?,
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

/// Blocking part of a download: verify, back up, and put the file in place.
fn apply_download(local: &Local, downloaded: &Path, backup: &Path, expected_hash: Option<&str>) -> Result<bool> {
    let actual = match local {
        Local::File(_) => save_archive::file_md5(downloaded)?,
        Local::Dir(_) => save_archive::zip_content_hash(downloaded)?,
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
            let mut targets = vec![save.dir.clone()];
            targets.extend(save.mirrors.iter().cloned());
            save_archive::restore_zip(downloaded, &save.archive_root, &targets, &backup.join(&save.archive_root))?;
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
    let keep_local = match (local_time, remote_time) {
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
        "save conflict for {}: kept {} version (local modified {:?}, RomM id={} updated {}); other version kept at {:?}",
        unit.local.path().display(),
        if keep_local { "local" } else { "RomM" },
        local_time.map(rfc3339),
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

#[derive(Debug, Clone, Copy)]
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

    async fn create(self, ctx: &SyncCtx<'_>, slot: Option<&str>, file: &Path) -> Result<Remote> {
        let emulator = ctx.emulator.id();
        Ok(match self {
            Kind::Save => ctx.romm.upload_save(ctx.rom_id, emulator, slot, file).await?.into(),
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

fn is_dir_save(r: &Remote) -> bool {
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

/// Try to extract a slot from a filename: "SLUS-00255_1.mcd" → Some("1").
fn extract_slot(name: &str) -> Option<String> {
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    stem.rsplit_once('_').map(|(_, slot)| slot.to_owned())
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

async fn collect_files(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return result;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_file() && !name.starts_with('.') && name.starts_with(prefix) {
            result.push(path);
        }
    }
    result.sort();
    result
}

/// `save-backups/<emulator>/<rom_id>/<time>/`; one dir per sync event.
fn backup_dir(ctx: &SyncCtx<'_>) -> PathBuf {
    backups_root()
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
    fn rejects_unsafe_names() {
        assert_eq!(local_file_name("../evil.sav"), None);
        assert_eq!(local_file_name("dir/evil.sav"), None);
        assert_eq!(local_file_name(".hidden"), None);
    }
}
