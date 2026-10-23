use std::collections::HashMap;
use std::sync::Arc;

use crate::config::Config;
use crate::emulator::Emulator;
use crate::launches::Launches;
use crate::live::Live;
use crate::process::ProcessManager;
use crate::romm::RommClient;
use crate::sync::SyncStore;
use crate::update::Updater;

pub struct AppState {
    /// The config and RomM client, swapped when the config file changes.
    pub live: Arc<Live>,
    pub processes: ProcessManager,
    pub emulators: HashMap<String, Arc<dyn Emulator>>,
    pub sync_store: Arc<SyncStore>,
    pub updater: Arc<Updater>,
    pub launches: Arc<Launches>,
}

impl AppState {
    /// The config as of now. Hold on to the returned value for the length of
    /// one operation, so it sees a single consistent config.
    pub fn config(&self) -> Arc<Config> {
        self.live.config()
    }

    pub fn romm(&self) -> Option<Arc<RommClient>> {
        self.live.romm()
    }
}
