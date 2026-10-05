use std::collections::HashMap;
use std::sync::Arc;

use crate::config::Config;
use crate::emulator::Emulator;
use crate::process::ProcessManager;
use crate::romm::RommClient;
use crate::sync::SyncStore;
use crate::update::Updater;

pub struct AppState {
    pub config: Config,
    pub processes: ProcessManager,
    pub romm: Option<Arc<RommClient>>,
    pub emulators: HashMap<String, Arc<dyn Emulator>>,
    pub sync_store: Arc<SyncStore>,
    pub updater: Arc<Updater>,
}
