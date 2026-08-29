use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Portable settings overlay for one game on one emulator.
/// Identity (emulator, game_id) is carried by the API request/response,
/// not by the blob itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileBlob {
    /// Flat "Section.Key" → "Value" overlay in the emulator's native INI format.
    pub overlay: IndexMap<String, String>,
}

impl ProfileBlob {
    pub fn new(overlay: IndexMap<String, String>) -> Self {
        Self { overlay }
    }
}

/// Turns a raw per-game config snapshot into a portable overlay by stripping
/// machine-specific keys before the blob is sent to RomM.
pub struct SnapshotCleaner {
    blacklist: &'static [&'static str],
}

impl SnapshotCleaner {
    pub fn for_emulator(emulator: &str) -> Self {
        Self {
            blacklist: match emulator {
                "duckstation" => DUCKSTATION_BLACKLIST,
                _ => &[],
            },
        }
    }

    pub fn clean(&self, mut overlay: IndexMap<String, String>) -> IndexMap<String, String> {
        overlay.retain(|key, value| {
            if self.blacklist.contains(&key.as_str()) {
                return false;
            }
            if looks_like_path(value) {
                return false;
            }
            true
        });
        overlay
    }
}

/// Heuristic: if the value looks like a filesystem path, it's machine-specific.
fn looks_like_path(value: &str) -> bool {
    if value.contains('/') || value.contains('~') {
        return true;
    }
    // Windows absolute path (e.g. "C:\")
    if value.len() >= 3 {
        let mut chars = value.chars();
        if let (Some(drive), Some(colon), Some(slash)) =
            (chars.next(), chars.next(), chars.next())
        {
            if drive.is_ascii_alphabetic() && colon == ':' && (slash == '\\' || slash == '/') {
                return true;
            }
        }
    }
    false
}

/// Keys that are machine-specific but don't look like paths.
const DUCKSTATION_BLACKLIST: &[&str] = &[
    "Display.WindowX",
    "Display.WindowY",
    "Display.WindowWidth",
    "Display.WindowHeight",
    "Display.WindowState",
    "MemoryCards.Card1Path",
    "MemoryCards.Card2Path",
    "MemoryCards.Card1SharedPath",
    "MemoryCards.Card2SharedPath",
    "Main.LastBootedPath",
    "Main.LastGamePath",
    "Logging.LogFilter",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_paths() {
        let mut overlay = IndexMap::new();
        overlay.insert("GPU.ResolutionScale".into(), "4".into());
        overlay.insert("MemoryCards.Card1Path".into(), "/home/user/card.mcd".into());
        overlay.insert("Display.WindowX".into(), "100".into());
        overlay.insert("Main.LastBootedPath".into(), "/roms/game.iso".into());

        let cleaner = SnapshotCleaner::for_emulator("duckstation");
        let clean = cleaner.clean(overlay);

        assert!(clean.contains_key("GPU.ResolutionScale"));
        assert!(!clean.contains_key("MemoryCards.Card1Path"));
        assert!(!clean.contains_key("Display.WindowX"));
        assert!(!clean.contains_key("Main.LastBootedPath"));
    }
}
