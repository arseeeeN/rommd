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
use crate::process::{GameProcess, ProcessTree, monitor};
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
        config_error: state.live.error(),
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
    let config = state.config();
    state
        .emulators
        .iter()
        .map(|(id, emu)| EmulatorInfo {
            id: id.clone(),
            display_name: emu.display_name().to_owned(),
            configured: config.emulators.contains_key(id),
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
    // One config and RomM client for the whole launch, even if the config
    // file is reloaded meanwhile; the session keeps them until it ends.
    let config = state.config();
    let romm_client = state.romm();

    let rom = match &req.local_path {
        Some(_) => None,
        None => {
            let romm = romm_client
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("RomM is not configured; provide local_path"))?;
            Some(romm.get_rom(req.rom_id).await?)
        }
    };
    let emulator_id = match (&req.emulator, &rom) {
        (Some(id), _) => id.clone(),
        (None, Some(rom)) => {
            let choice =
                crate::games::choose_emulator(&rom.platform_slug, None, &config, &state.emulators)?;
            info!(
                "using {} for rom {}: {}",
                choice.id, req.rom_id, choice.reason
            );
            choice.id
        }
        (None, None) => {
            return Err(anyhow::anyhow!("a launch with local_path needs an emulator").into());
        }
    };
    let emulator = state
        .emulators
        .get(&emulator_id)
        .ok_or_else(|| anyhow::anyhow!("emulator '{}' is not configured", emulator_id))?;
    let emu_config = config
        .emulators
        .get(&emulator_id)
        .ok_or_else(|| anyhow::anyhow!("no config entry for emulator '{}'", emulator_id))?;

    let rom_path: PathBuf = match (&rom, &req.local_path) {
        (Some(rom), _) => {
            let romm = romm_client.as_ref().expect("checked above");
            romm.download_rom_to_dir(rom, &config.roms_dir()).await?
        }
        (None, local) => PathBuf::from(local.as_deref().unwrap_or_default()),
    };

    if let Some(problem) = crate::emulator::config_dir_problem(emulator.as_ref(), emu_config) {
        tracing::warn!("[emulators.{}] {}", emulator_id, problem);
    }
    let game_id = emulator.game_id_for_rom(emu_config, &rom_path);

    let save_layout = emulator
        .save_layout(emu_config, &rom_path, game_id.as_deref())
        .await;

    let present_at_launch = match &romm_client {
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

    let tree =
        ProcessTree::new(&child).ok_or_else(|| anyhow::anyhow!("process exited immediately"))?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("process exited immediately"))?;
    let started_at = SystemTime::now();
    info!("launched {} pid={} rom_id={}", emulator_id, pid, req.rom_id);

    state
        .processes
        .add(
            pid,
            GameProcess {
                rom_id: req.rom_id,
                emulator: emulator_id.clone(),
                game_id,
                rom_path,
                started_at,
                launch_ctx,
                save_layout,
                present_at_launch,
                tree: Arc::new(tree),
            },
        )
        .await;

    monitor(
        pid,
        child,
        state.processes.clone(),
        romm_client.clone(),
        emulator.clone(),
        emu_config.clone(),
        state.sync_store.clone(),
    );

    Ok((
        StatusCode::CREATED,
        Json(LaunchResponse {
            pid,
            emulator: emulator_id,
        }),
    ))
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
        (status = 204, description = "Asked the game(s) to close; they are killed if still running 5 s later"),
        (status = 404, description = "No game launched by rommd has this PID")
    )
)]
pub async fn stop(State(state): State<Arc<AppState>>, Json(req): Json<StopRequest>) -> Response {
    match req.pid {
        Some(pid) => {
            if !state.processes.stop(pid).await {
                return (
                    StatusCode::NOT_FOUND,
                    Json(
                        serde_json::json!({ "error": format!("no running game has pid {}", pid) }),
                    ),
                )
                    .into_response();
            }
        }
        None => {
            for game in state.processes.list().await {
                state.processes.stop(game.pid).await;
            }
        }
    }
    StatusCode::NO_CONTENT.into_response()
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
        .config()
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

    let config = state.config();
    let emu_config = config
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
    path = "/games",
    params(GamesQuery),
    responses(
        (status = 200, description = "Games in the RomM library matching the query, \
            with the emulator a launch without one would use", body = GamesResponse),
        (status = 500, description = "Invalid query, or RomM is unavailable")
    )
)]
pub async fn games(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<GamesQuery>,
) -> Result<Json<GamesResponse>, AppError> {
    let config = state.config();
    let romm = state
        .romm()
        .ok_or_else(|| anyhow::anyhow!("RomM is not configured"))?;
    let found = crate::games::find_games(
        &romm,
        &config,
        &state.emulators,
        &query.q,
        query.limit.unwrap_or(20).clamp(1, 100),
    )
    .await?;
    Ok(Json(found))
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
