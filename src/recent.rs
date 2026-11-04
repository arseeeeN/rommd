//! The games played last, newest first, for clients to offer again (the
//! tray's menu). Kept in `recent.json` beside the sync state.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

/// How many games are kept.
const KEPT: usize = 10;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RecentGame {
    pub rom_id: i64,
    pub name: String,
    /// The platform's name as RomM shows it.
    pub platform: String,
    pub emulator: String,
    /// When it was last started, Unix seconds.
    pub played_at: u64,
}

pub struct RecentGames {
    path: PathBuf,
    games: Mutex<Vec<RecentGame>>,
}

impl RecentGames {
    /// The list saved at `path`; empty when there is none or it doesn't read.
    pub fn load(path: PathBuf) -> Self {
        let games = std::fs::read(&path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        Self {
            path,
            games: Mutex::new(games),
        }
    }

    pub fn default_path() -> PathBuf {
        crate::sync::data_dir().join("recent.json")
    }

    pub fn list(&self) -> Vec<RecentGame> {
        self.games.lock().unwrap().clone()
    }

    /// Put `game` first, replacing its older entry, and save.
    pub fn record(&self, game: RecentGame) {
        let games = {
            let mut games = self.games.lock().unwrap();
            games.retain(|g| g.rom_id != game.rom_id);
            games.insert(0, game);
            games.truncate(KEPT);
            games.clone()
        };
        let result = (|| -> anyhow::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let staged = self.path.with_extension("json.tmp");
            std::fs::write(&staged, serde_json::to_vec_pretty(&games)?)?;
            std::fs::rename(&staged, &self.path)?;
            Ok(())
        })();
        if let Err(e) = result {
            tracing::warn!("saving the recent games to {}: {e:#}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn game(rom_id: i64, played_at: u64) -> RecentGame {
        RecentGame {
            rom_id,
            name: format!("Game {rom_id}"),
            platform: "Nintendo 64".into(),
            emulator: "rmg".into(),
            played_at,
        }
    }

    #[test]
    fn newest_first_once_each_and_saved() {
        let dir = TempDir::new("recent");
        let path = dir.path().join("recent.json");
        let recent = RecentGames::load(path.clone());
        for i in 0..12 {
            recent.record(game(i, i as u64));
        }
        recent.record(game(5, 100));
        let list = recent.list();
        assert_eq!(list.len(), KEPT);
        assert_eq!(list[0], game(5, 100));
        assert_eq!(list.iter().filter(|g| g.rom_id == 5).count(), 1);
        assert_eq!(
            RecentGames::load(path).list(),
            list,
            "it survives a restart"
        );
    }
}
