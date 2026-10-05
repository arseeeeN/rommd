use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;
use tracing::warn;

const CONFLICTS_KEPT: usize = 50;

/// What a save looked like on both sides the last time it was in sync. The
/// three-way comparison against this is what tells "I changed" apart from
/// "RomM changed" apart from "both changed".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncBase {
    /// RomM-compatible content hash of the local save when last in sync.
    pub local_hash: String,
    /// The RomM save/state entry it was in sync with.
    pub remote_id: i64,
    /// That entry's `content_hash`, when RomM provides one.
    pub remote_hash: Option<String>,
    /// That entry's `updated_at`; the change signal when there is no hash.
    pub remote_updated_at: String,
}

/// A conflict rommd resolved automatically (newest wins). The losing copy is
/// always kept at `loser_backup`.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ConflictRecord {
    /// RFC 3339 time the conflict was resolved.
    pub resolved_at: String,
    pub rom_id: i64,
    pub emulator: String,
    /// Local save file or directory.
    pub local_path: String,
    /// `"local"` or `"remote"` — the side that was kept.
    pub kept: String,
    /// RFC 3339 mtime of the local save.
    pub local_modified: Option<String>,
    pub remote_id: i64,
    pub remote_emulator: Option<String>,
    pub remote_updated_at: String,
    /// Where the losing version was saved.
    pub loser_backup: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Data {
    #[serde(default)]
    bases: BTreeMap<String, SyncBase>,
    #[serde(default)]
    conflicts: Vec<ConflictRecord>,
}

/// Persistent sync state, one JSON file under rommd's data dir.
pub struct SyncStore {
    path: PathBuf,
    data: Mutex<Data>,
}

impl SyncStore {
    pub fn default_path() -> PathBuf {
        super::data_dir().join("sync-state.json")
    }

    /// Load the store, starting empty if the file is missing. A corrupt file is
    /// moved aside rather than overwritten, so the history can be inspected.
    pub fn load(path: PathBuf) -> Self {
        let data = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                let aside = path.with_extension("json.corrupt");
                warn!(
                    "sync state {} is unreadable ({}); moved to {}",
                    path.display(),
                    e,
                    aside.display()
                );
                let _ = std::fs::rename(&path, &aside);
                Data::default()
            }),
            Err(_) => Data::default(),
        };
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    /// Where local versions are backed up before being replaced:
    /// `save-backups/` next to the state file.
    pub fn backups_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map_or_else(|| PathBuf::from("save-backups"), |p| p.join("save-backups"))
    }

    pub async fn base(&self, key: &str) -> Option<SyncBase> {
        self.data.lock().await.bases.get(key).cloned()
    }

    pub async fn set_base(&self, key: &str, base: SyncBase) {
        let mut data = self.data.lock().await;
        if data.bases.get(key) == Some(&base) {
            return;
        }
        data.bases.insert(key.to_owned(), base);
        persist(&self.path, &data);
    }

    pub async fn record_conflict(&self, record: ConflictRecord) {
        let mut data = self.data.lock().await;
        data.conflicts.push(record);
        let excess = data.conflicts.len().saturating_sub(CONFLICTS_KEPT);
        data.conflicts.drain(..excess);
        persist(&self.path, &data);
    }

    /// Recent conflicts, newest first.
    pub async fn conflicts(&self) -> Vec<ConflictRecord> {
        self.data
            .lock()
            .await
            .conflicts
            .iter()
            .rev()
            .cloned()
            .collect()
    }
}

/// Write via a temp file and rename so a crash never leaves a torn file.
fn persist(path: &Path, data: &Data) {
    let result = (|| -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(data)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if let Err(e) = result {
        warn!(
            "could not persist sync state to {}: {:#}",
            path.display(),
            e
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn persists_and_recovers_from_corruption() {
        let dir = std::env::temp_dir().join(format!("rommd-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sync-state.json");
        let base = SyncBase {
            local_hash: "abc".into(),
            remote_id: 7,
            remote_hash: Some("abc".into()),
            remote_updated_at: "2026-01-01T00:00:00Z".into(),
        };
        SyncStore::load(path.clone())
            .set_base("k", base.clone())
            .await;
        assert_eq!(SyncStore::load(path.clone()).base("k").await, Some(base));

        std::fs::write(&path, b"{not json").unwrap();
        let store = SyncStore::load(path.clone());
        assert_eq!(store.base("k").await, None);
        assert!(path.with_extension("json.corrupt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
