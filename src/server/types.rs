use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

use crate::profile::ProfileBlob;

// ── /launch ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct LaunchRequest {
    /// RomM ROM ID to download and launch.
    pub rom_id: i64,
    /// Emulator ID (e.g. `"duckstation"`, `"ryujinx"`).
    pub emulator: String,
    /// Optional settings overlay to apply before launch.
    pub profile: Option<ProfileBlob>,
    /// Skip RomM download and use this local path directly.
    pub local_path: Option<String>,
    /// Physical controller bindings for each slot. Omit to launch without
    /// applying any controller configuration.
    pub controller_slots: Option<Vec<ControllerSlot>>,
}

/// Binds a locally configured physical controller to an emulator slot.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ControllerSlot {
    /// Emulator slot number (1-indexed).
    pub slot: u8,
    /// ID of a controller registered in the local rommd config.
    pub controller_id: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LaunchResponse {
    /// PID of the spawned emulator process.
    pub pid: u32,
}

// ── /status ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct StatusResponse {
    pub running: Vec<RunningGameInfo>,
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
