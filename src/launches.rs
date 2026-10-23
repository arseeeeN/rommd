//! Launches as they progress, so `POST /launch` can return while the ROM
//! downloads instead of holding the request open (browsers give up on a
//! response after a few minutes). Clients follow a launch through
//! `GET /launches/{id}` and cancel its download with `DELETE /launches/{id}`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::romm::Transfer;

/// Finished launches stay listed this long, so a client polling every few
/// seconds sees how its launch ended.
const KEEP_FINISHED: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LaunchState {
    /// Looking up the ROM or waiting for RomM to start sending it.
    Preparing,
    Downloading {
        downloaded_bytes: u64,
        /// `None` when RomM sends no size (multi-file ROMs bundled on the fly).
        total_bytes: Option<u64>,
    },
    /// The ROM is there; saves are synced and the emulator started.
    Starting,
    Started {
        pid: u32,
    },
    Failed {
        error: String,
    },
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LaunchInfo {
    pub id: u64,
    pub rom_id: i64,
    pub name: String,
    pub emulator: String,
    #[serde(flatten)]
    pub state: LaunchState,
}

/// Where a launch is; `Fetching` covers both `Preparing` and `Downloading`,
/// told apart by the transfer's progress.
enum Phase {
    Fetching { cancel_requested: bool },
    Starting,
    Done(LaunchState, Instant),
}

struct Entry {
    rom_id: i64,
    name: String,
    emulator: String,
    transfer: Transfer,
    phase: Phase,
}

impl Entry {
    fn info(&self, id: u64) -> LaunchInfo {
        let state = match &self.phase {
            Phase::Fetching { .. } => {
                let p = self.transfer.progress();
                if p.started {
                    LaunchState::Downloading {
                        downloaded_bytes: p.done,
                        total_bytes: p.total,
                    }
                } else {
                    LaunchState::Preparing
                }
            }
            Phase::Starting => LaunchState::Starting,
            Phase::Done(state, _) => state.clone(),
        };
        LaunchInfo {
            id,
            rom_id: self.rom_id,
            name: self.name.clone(),
            emulator: self.emulator.clone(),
            state,
        }
    }

    fn in_progress(&self) -> bool {
        !matches!(self.phase, Phase::Done(..))
    }
}

pub enum Cancel {
    Cancelled,
    /// The ROM is already there and the game is being started.
    TooLate,
    NotFound,
}

#[derive(Default)]
pub struct Launches {
    next_id: AtomicU64,
    entries: Mutex<BTreeMap<u64, Entry>>,
}

impl Launches {
    /// Track a new launch. While another launch of the same ROM is still in
    /// progress, returns that one's ID as the error instead: both would
    /// download to the same file, and a second click should just follow the
    /// first.
    pub fn begin(
        &self,
        rom_id: i64,
        name: &str,
        emulator: &str,
        transfer: &Transfer,
    ) -> Result<u64, u64> {
        let mut entries = self.entries.lock().unwrap();
        prune(&mut entries);
        if let Some((id, _)) = entries
            .iter()
            .find(|(_, e)| e.rom_id == rom_id && e.in_progress())
        {
            return Err(*id);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        entries.insert(
            id,
            Entry {
                rom_id,
                name: name.to_owned(),
                emulator: emulator.to_owned(),
                transfer: transfer.clone(),
                phase: Phase::Fetching {
                    cancel_requested: false,
                },
            },
        );
        Ok(id)
    }

    /// The ROM is there. False when the launch was cancelled meanwhile, in
    /// which case it must not go on.
    pub fn to_starting(&self, id: u64) -> bool {
        let mut entries = self.entries.lock().unwrap();
        match entries.get_mut(&id) {
            Some(e)
                if matches!(
                    e.phase,
                    Phase::Fetching {
                        cancel_requested: false
                    }
                ) =>
            {
                e.phase = Phase::Starting;
                true
            }
            _ => false,
        }
    }

    pub fn finish(&self, id: u64, state: LaunchState) {
        if let Some(e) = self.entries.lock().unwrap().get_mut(&id) {
            e.phase = Phase::Done(state, Instant::now());
        }
    }

    /// Cancel a launch whose ROM is still being fetched.
    pub fn cancel(&self, id: u64) -> Cancel {
        let mut entries = self.entries.lock().unwrap();
        let Some(e) = entries.get_mut(&id) else {
            return Cancel::NotFound;
        };
        match &mut e.phase {
            Phase::Fetching { cancel_requested } => {
                *cancel_requested = true;
                e.transfer.cancel.cancel();
                Cancel::Cancelled
            }
            Phase::Done(LaunchState::Cancelled, _) => Cancel::Cancelled,
            Phase::Starting | Phase::Done(..) => Cancel::TooLate,
        }
    }

    pub fn was_cancelled(&self, id: u64) -> bool {
        matches!(
            self.entries.lock().unwrap().get(&id).map(|e| &e.phase),
            Some(Phase::Fetching {
                cancel_requested: true
            })
        )
    }

    pub fn info(&self, id: u64) -> Option<LaunchInfo> {
        self.entries.lock().unwrap().get(&id).map(|e| e.info(id))
    }

    /// Launches in progress and those finished in the last few minutes.
    pub fn list(&self) -> Vec<LaunchInfo> {
        let mut entries = self.entries.lock().unwrap();
        prune(&mut entries);
        entries.iter().map(|(id, e)| e.info(*id)).collect()
    }

    /// Whether a launch is downloading or starting, which a restart (an
    /// update) would break.
    pub fn busy(&self) -> bool {
        self.entries
            .lock()
            .unwrap()
            .values()
            .any(Entry::in_progress)
    }
}

fn prune(entries: &mut BTreeMap<u64, Entry>) {
    entries.retain(|_, e| match e.phase {
        Phase::Done(_, at) => at.elapsed() < KEEP_FINISHED,
        _ => true,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_launch_of_the_same_rom_follows_the_first() {
        let launches = Launches::default();
        let id = launches
            .begin(199, "Xenoblade", "ryujinx", &Transfer::default())
            .unwrap();
        assert_eq!(
            launches.begin(199, "Xenoblade", "ryujinx", &Transfer::default()),
            Err(id)
        );
        assert!(
            launches
                .begin(5, "Odyssey", "ryujinx", &Transfer::default())
                .is_ok()
        );
        assert!(launches.busy());

        assert!(launches.to_starting(id));
        launches.finish(id, LaunchState::Started { pid: 42 });
        let again = launches.begin(199, "Xenoblade", "ryujinx", &Transfer::default());
        assert!(
            again.is_ok_and(|new| new != id),
            "a finished launch doesn't block"
        );
        assert_eq!(
            launches.info(id).unwrap().state,
            LaunchState::Started { pid: 42 }
        );
    }

    #[test]
    fn only_a_launch_still_fetching_its_rom_can_be_cancelled() {
        let launches = Launches::default();
        let transfer = Transfer::default();
        let id = launches
            .begin(199, "Xenoblade", "ryujinx", &transfer)
            .unwrap();
        assert_eq!(launches.info(id).unwrap().state, LaunchState::Preparing);
        assert!(matches!(launches.cancel(id), Cancel::Cancelled));
        assert!(transfer.cancel.is_cancelled());
        assert!(launches.was_cancelled(id));
        assert!(
            !launches.to_starting(id),
            "a download that finishes as it is cancelled must not launch"
        );

        let other = launches
            .begin(5, "Odyssey", "ryujinx", &Transfer::default())
            .unwrap();
        assert!(launches.to_starting(other));
        assert!(matches!(launches.cancel(other), Cancel::TooLate));
        assert!(matches!(launches.cancel(999), Cancel::NotFound));
    }
}
