use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tracing::info;

use super::state::AppState;
use super::types::*;
use crate::config::ControllerConfig;
use crate::error::AppError;
use crate::process::{GameProcess, monitor};
use crate::sync::{self, ConflictRecord, SyncCtx};
use crate::update::UpdateStatus;

#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "Daemon is running", body = HealthResponse)
    )
)]
pub async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        emulators: emulator_list(&state),
    })
}

#[utoipa::path(
    get,
    path = "/emulators",
    responses(
        (status = 200, description = "List of known emulators", body = Vec<EmulatorInfo>)
    )
)]
pub async fn list_emulators(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(emulator_list(&state))
}

fn emulator_list(state: &AppState) -> Vec<EmulatorInfo> {
    state
        .emulators
        .iter()
        .map(|(id, emu)| EmulatorInfo {
            id: id.clone(),
            display_name: emu.display_name().to_owned(),
            configured: state.config.emulators.contains_key(id),
        })
        .collect()
}

#[utoipa::path(
    post,
    path = "/launch",
    request_body = LaunchRequest,
    responses(
        (status = 201, description = "Game launched", body = LaunchResponse),
        (status = 400, description = "Emulator not configured or RomM not available"),
        (status = 500, description = "Launch failed")
    )
)]
pub async fn launch(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LaunchRequest>,
) -> Result<impl IntoResponse, AppError> {
    let emulator = state
        .emulators
        .get(&req.emulator)
        .ok_or_else(|| anyhow::anyhow!("emulator '{}' is not configured", req.emulator))?;

    let emu_config = state
        .config
        .emulators
        .get(&req.emulator)
        .ok_or_else(|| anyhow::anyhow!("no config entry for emulator '{}'", req.emulator))?;

    let rom_path: PathBuf = if let Some(local) = &req.local_path {
        PathBuf::from(local)
    } else {
        let romm = state
            .romm
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("RomM is not configured; provide local_path"))?;

        let rom = romm.get_rom(req.rom_id).await?;
        let roms_dir = state.config.roms_dir();
        romm.download_rom_to_dir(&rom, &roms_dir).await?
    };

    let game_id = emulator.game_id_for_rom(emu_config, &rom_path);

    let save_layout = emulator
        .save_layout(emu_config, &rom_path, game_id.as_deref())
        .await;

    let present_at_launch = match &state.romm {
        Some(romm) => {
            let ctx = SyncCtx {
                romm,
                store: &state.sync_store,
                rom_id: req.rom_id,
                emulator: emulator.as_ref(),
            };
            sync::sync_layout(&ctx, &save_layout, &rom_path, None).await
        }
        None => Default::default(),
    };

    let (child, launch_ctx) = emulator
        .launch(
            emu_config,
            &rom_path,
            game_id.as_deref(),
            req.profile.as_ref(),
        )
        .await?;

    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("process exited immediately"))?;
    let started_at = SystemTime::now();
    info!(
        "launched {} pid={} rom_id={}",
        req.emulator, pid, req.rom_id
    );

    state
        .processes
        .add(
            pid,
            GameProcess {
                rom_id: req.rom_id,
                emulator: req.emulator.clone(),
                game_id,
                rom_path,
                started_at,
                launch_ctx,
                save_layout,
                present_at_launch,
            },
        )
        .await;

    monitor(
        pid,
        child,
        state.processes.clone(),
        state.romm.clone(),
        emulator.clone(),
        emu_config.clone(),
        state.sync_store.clone(),
    );

    Ok((StatusCode::CREATED, Json(LaunchResponse { pid })))
}

#[utoipa::path(
    get,
    path = "/status",
    responses(
        (status = 200, description = "Currently running games", body = StatusResponse)
    )
)]
pub async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let running = state
        .processes
        .list()
        .await
        .into_iter()
        .map(|g| RunningGameInfo {
            pid: g.pid,
            rom_id: g.rom_id,
            emulator: g.emulator,
            game_id: g.game_id,
            rom_path: g.rom_path,
            started_at: g.started_at,
        })
        .collect();

    Json(StatusResponse { running })
}

#[utoipa::path(
    post,
    path = "/stop",
    request_body = StopRequest,
    responses(
        (status = 204, description = "Game(s) stopped"),
        (status = 500, description = "Error stopping process")
    )
)]
pub async fn stop(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StopRequest>,
) -> Result<impl IntoResponse, AppError> {
    match req.pid {
        Some(pid) => {
            state.processes.kill(pid).await?;
        }
        None => {
            for game in state.processes.list().await {
                if let Err(e) = state.processes.kill(game.pid).await {
                    tracing::warn!("could not kill pid={}: {}", game.pid, e);
                }
            }
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/controllers",
    responses(
        (status = 200, description = "Locally configured controllers", body = Vec<ControllerInfo>)
    )
)]
pub async fn list_controllers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut controllers: Vec<ControllerInfo> = state
        .config
        .controllers
        .iter()
        .map(|(id, cfg)| {
            let controller_type = match cfg {
                ControllerConfig::Bluetooth { .. } => ControllerType::Bluetooth,
                ControllerConfig::Usb { .. } => ControllerType::Usb,
                ControllerConfig::DolphinWiimote { .. } => ControllerType::DolphinWiimote,
                ControllerConfig::Dsu { .. } => ControllerType::Dsu,
            };
            ControllerInfo {
                id: id.clone(),
                name: cfg.name().to_owned(),
                controller_type,
                connected: None, // connection detection deferred
            }
        })
        .collect();
    controllers.sort_by(|a, b| a.id.cmp(&b.id));
    Json(controllers)
}

#[utoipa::path(
    post,
    path = "/snapshot",
    request_body = SnapshotRequest,
    responses(
        (status = 200, description = "Emulator profile snapshot", body = SnapshotResponse),
        (status = 400, description = "No game running for this emulator"),
        (status = 500, description = "Snapshot failed")
    )
)]
pub async fn snapshot(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SnapshotRequest>,
) -> Result<impl IntoResponse, AppError> {
    let emulator = state
        .emulators
        .get(&req.emulator)
        .ok_or_else(|| anyhow::anyhow!("emulator '{}' is not configured", req.emulator))?;

    let emu_config = state
        .config
        .emulators
        .get(&req.emulator)
        .ok_or_else(|| anyhow::anyhow!("no config entry for emulator '{}'", req.emulator))?;

    let game_id = state
        .processes
        .game_id_for_emulator(&req.emulator)
        .await
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no '{}' game is currently running — launch one first to take a snapshot",
                req.emulator
            )
        })?;

    let profile = emulator.snapshot(emu_config, &game_id).await?;
    Ok(Json(SnapshotResponse { game_id, profile }))
}

#[utoipa::path(
    get,
    path = "/saves/conflicts",
    responses(
        (status = 200, description = "Recent save conflicts rommd resolved automatically, newest first. \
            The newer version was kept; the other is at `loser_backup`.", body = Vec<ConflictRecord>)
    )
)]
pub async fn save_conflicts(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.sync_store.conflicts().await)
}

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct UpdateQuery {
    /// Check GitHub now instead of returning the last result.
    #[serde(default)]
    pub check: bool,
}

#[utoipa::path(
    get,
    path = "/update",
    params(UpdateQuery),
    responses(
        (status = 200, description = "Installed version, the newest release, and whether this build can update itself", body = UpdateStatus)
    )
)]
pub async fn update_status(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<UpdateQuery>,
) -> impl IntoResponse {
    let status = state.updater.status().await;
    if query.check || status.last_checked.is_none() {
        return Json(state.updater.check().await);
    }
    Json(status)
}

#[utoipa::path(
    post,
    path = "/update",
    responses(
        (status = 200, description = "Update installed; rommd restarts into it", body = UpdateInstalled),
        (status = 400, description = "No newer release, or this build cannot install updates"),
        (status = 409, description = "A game is running; stop it first"),
        (status = 500, description = "Download, verification or install failed")
    )
)]
pub async fn update_install(State(state): State<Arc<AppState>>) -> Response {
    if !state.processes.list().await.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "a game is running; stop it before updating" })),
        )
            .into_response();
    }
    let status = state.updater.check().await;
    if let Some(blocker) = status.install_blocker.filter(|_| status.update_available) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": blocker })),
        )
            .into_response();
    }
    if !status.update_available {
        let error = status
            .last_error
            .unwrap_or_else(|| format!("rommd {} is the latest release", status.current_version));
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        )
            .into_response();
    }
    match state.updater.install().await {
        Ok(version) => {
            // Let the response reach the client before the process goes away.
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                crate::update::restart();
            });
            Json(UpdateInstalled {
                installed_version: version.to_string(),
                restarting: true,
            })
            .into_response()
        }
        Err(e) => AppError::from(e).into_response(),
    }
}
