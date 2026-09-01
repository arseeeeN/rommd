use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tracing::info;

use super::state::AppState;
use super::types::*;
use crate::error::AppError;
use crate::process::{self, GameProcess, monitor};

pub async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        emulators: emulator_list(&state),
    })
}

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

    let save_filter = emulator.save_filter(emu_config, &rom_path, game_id.as_deref());

    let saves_dir = emulator.saves_dir(emu_config);
    let states_dir = emulator.states_dir(emu_config);

    // Resolve game-specific save slot dir for emulators that use one (Ryujinx).
    let slot_dir = match &game_id {
        Some(id) => emulator.game_saves_dir(emu_config, id).await,
        None => None,
    };

    // Upload any existing local saves to RomM before the session starts.
    // This bootstraps saves that predate rommd; the on-exit sync is still
    // mtime-filtered so it only uploads what changed during this session.
    if let Some(romm) = &state.romm {
        if let Some(ref slot) = slot_dir {
            process::sync_profile_saves_pre_launch(romm, req.rom_id, &req.emulator, slot).await;
        } else {
            process::sync_saves_pre_launch(romm, req.rom_id, &req.emulator, &saves_dir, &save_filter).await;
            process::sync_states_pre_launch(romm, req.rom_id, &req.emulator, &states_dir, &save_filter).await;
        }
    }

    let (child, launch_ctx) = emulator
        .launch(emu_config, &rom_path, game_id.as_deref(), req.profile.as_ref())
        .await?;

    let pid = child.id().ok_or_else(|| anyhow::anyhow!("process exited immediately"))?;
    let started_at = SystemTime::now();
    info!("launched {} pid={} rom_id={}", req.emulator, pid, req.rom_id);

    state
        .processes
        .add(
            pid,
            GameProcess {
                rom_id: req.rom_id,
                emulator: req.emulator.clone(),
                game_id,
                save_filter,
                rom_path,
                started_at,
                launch_ctx,
                slot_dir,
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
        saves_dir,
        states_dir,
    );

    Ok((StatusCode::CREATED, Json(LaunchResponse { pid })))
}

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
