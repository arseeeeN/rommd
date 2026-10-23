use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

use crate::profile::ProfileBlob;

// ── /launch ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct LaunchRequest {
    /// RomM ROM ID to download and launch.
    pub rom_id: i64,
    /// Emulator ID (e.g. `"duckstation"`, `"ryujinx"`). Without one, rommd
    /// picks the emulator for the ROM's platform (see `GET /games`).
    pub emulator: Option<String>,
    /// Optional settings overlay to apply before launch.
    pub profile: Option<ProfileBlob>,
    /// Skip RomM download and use this local path directly.
    pub local_path: Option<String>,
    /// Physical controller bindings for each slot. Omit to launch without
    /// applying any controller configuration.
    #[allow(dead_code)]
    // accepted ahead of wiring to emulator configs (dev/controller-design.md)
    pub controller_slots: Option<Vec<ControllerSlot>>,
}

/// Binds a locally configured physical controller to an emulator slot.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[allow(dead_code)] // see LaunchRequest::controller_slots
pub struct ControllerSlot {
    /// Emulator slot number (1-indexed).
    pub slot: u8,
    /// ID of a controller registered in the local rommd config.
    pub controller_id: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LaunchResponse {
    /// Follow it with `GET /launches/{id}`, cancel it with
    /// `DELETE /launches/{id}`.
    pub launch_id: u64,
    /// The emulator's PID once it started (201). Absent while the ROM
    /// downloads (202); the game starts when the download completes.
    pub pid: Option<u32>,
    pub emulator: String,
}

// ── /games ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct GamesQuery {
    /// Words to search for in game names, plus `key:value` filters:
    /// `platform:` (RomM platform slug, folder name or name), `emulator:`,
    /// `library:romm`, `id:` (a ROM ID). Quote values with spaces.
    pub q: String,
    /// Most games to return (default 20).
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GamesResponse {
    /// Best matches first: exact name, then names starting with the search.
    pub games: Vec<GameMatch>,
    /// How many games match in all, beyond `limit`.
    pub total: i64,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GameMatch {
    pub rom_id: i64,
    pub name: String,
    /// RomM platform slug.
    pub platform: String,
    pub platform_name: String,
    /// The emulator a launch would use; `None` when none is configured for
    /// the platform.
    pub emulator: Option<String>,
    /// Why that emulator, or why none.
    pub emulator_reason: String,
}

// ── /status ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct StatusResponse {
    pub running: Vec<RunningGameInfo>,
    /// Launches in progress (with download progress) and those that
    /// finished in the last few minutes.
    pub launches: Vec<crate::launches::LaunchInfo>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RunningGameInfo {
    pub pid: u32,
    pub rom_id: i64,
    pub emulator: String,
    pub game_id: Option<String>,
    #[schema(value_type = String)]
    pub rom_path: PathBuf,
    /// Unix timestamp as `{"secs_since_epoch": u64, "nanos_since_epoch": u32}`.
    #[schema(value_type = Object)]
    pub started_at: SystemTime,
}

// ── /stop ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct StopRequest {
    /// PID to stop. Omit to stop all running games.
    pub pid: Option<u32>,
}

// ── /snapshot ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SnapshotRequest {
    pub emulator: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SnapshotResponse {
    pub game_id: String,
    pub profile: ProfileBlob,
}

// ── /health ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    pub emulators: Vec<EmulatorInfo>,
    /// Set when the config file on disk has an error: rommd keeps running with
    /// the last config that loaded until the file is fixed.
    pub config_error: Option<String>,
}

// ── /emulators ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct EmulatorInfo {
    pub id: String,
    pub display_name: String,
    /// Whether binary + config_dir have been set in the daemon config.
    /// False means the emulator is known but cannot be launched yet.
    pub configured: bool,
}

// ── /controllers ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ControllerInfo {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    #[schema(rename = "type")]
    pub controller_type: ControllerType,
    /// Whether the controller is currently connected. `null` when the
    /// connection state cannot be determined (Dolphin Wiimotes, DSU devices).
    pub connected: Option<bool>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ControllerType {
    Bluetooth,
    Usb,
    DolphinWiimote,
    Dsu,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UpdateInstalled {
    pub installed_version: String,
    /// rommd restarts into the new version right after this response.
    pub restarting: bool,
}
