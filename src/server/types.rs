use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

use crate::profile::ProfileBlob;

// ── /launch ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LaunchRequest {
    pub rom_id: i64,
    pub emulator: String,
    pub profile: Option<ProfileBlob>,
    /// Skip RomM download and use this local path directly.
    pub local_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LaunchResponse {
    pub pid: u32,
}

// ── /status ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub running: Vec<RunningGameInfo>,
}

#[derive(Debug, Serialize)]
pub struct RunningGameInfo {
    pub pid: u32,
    pub rom_id: i64,
    pub emulator: String,
    pub game_id: Option<String>,
    pub rom_path: PathBuf,
    pub started_at: SystemTime,
}

// ── /stop ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct StopRequest {
    /// PID to stop. Omit to stop all running games.
    pub pid: Option<u32>,
}

// ── /snapshot ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SnapshotRequest {
    pub emulator: String,
}

#[derive(Debug, Serialize)]
pub struct SnapshotResponse {
    pub game_id: String,
    pub profile: ProfileBlob,
}

// ── /health ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    pub emulators: Vec<EmulatorInfo>,
}

// ── /emulators ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct EmulatorInfo {
    pub id: String,
    pub display_name: String,
    /// Whether binary + config_dir have been set in the daemon config.
    /// False means the emulator is known but cannot be launched yet.
    pub configured: bool,
}
