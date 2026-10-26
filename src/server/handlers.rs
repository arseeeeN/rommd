use anyhow::Context;
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tracing::{info, warn};

use super::state::AppState;
use super::types::*;
use crate::config::ControllerConfig;
use crate::error::AppError;
use crate::launches::{Attach, Attached, Cancel, LaunchInfo, LaunchState};
use crate::process::{GameProcess, ProcessTree, monitor};
use crate::romm::Transfer;
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
        (status = 201, description = "Game started", body = LaunchResponse),
        (status = 202, description = "The ROM is downloading; the game starts when it is done. \
            Follow it with `GET /launches/{id}`, cancel it with `DELETE /launches/{id}`. Also \
            returned for a ROM that another launch is still fetching, and at once for an \
            attached launch.", body = LaunchResponse),
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
    let romm = state.romm();

    let rom = match &req.local_path {
        Some(_) => None,
        None => {
            let client = romm
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("RomM is not configured; provide local_path"))?;
            Some(client.get_rom(req.rom_id).await?)
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
    if !state.emulators.contains_key(&emulator_id) || !config.emulators.contains_key(&emulator_id) {
        return Err(anyhow::anyhow!("emulator '{}' is not configured", emulator_id).into());
    }
    let name = match (&rom, &req.local_path) {
        (Some(rom), _) => rom.name.clone().unwrap_or_else(|| rom.fs_name.clone()),
        (None, local) => local.clone().unwrap_or_default(),
    };

    let transfer = Transfer::default();
    let id = match state
        .launches
        .begin(req.rom_id, &name, &emulator_id, &transfer)
    {
        Ok(id) => id,
        Err(existing) => {
            let emulator = state
                .launches
                .info(existing)
                .map_or(emulator_id, |l| l.emulator);
            let response = LaunchResponse {
                launch_id: existing,
                pid: None,
                emulator,
            };
            return Ok((StatusCode::ACCEPTED, Json(response)));
        }
    };

    // The launch runs in its own task, so a client that disconnects or stops
    // waiting never interrupts it halfway: a game is always tracked and its
    // saves synced. Only `DELETE /launches/{id}` cancels, and only while the
    // ROM is being fetched.
    let launch = Launch {
        state: state.clone(),
        id,
        req,
        rom,
        emulator_id: emulator_id.clone(),
        config,
        romm,
        transfer: transfer.clone(),
    };
    let attach = launch.req.attach;
    let mut task = tokio::spawn(launch.run());
    if attach {
        let response = LaunchResponse {
            launch_id: id,
            pid: None,
            emulator: emulator_id,
        };
        return Ok((StatusCode::ACCEPTED, Json(response)));
    }
    // A ROM that is already here starts within seconds, so answer with the
    // PID. A download can take far longer than a browser waits for a response
    // (Firefox: 5 minutes), so then answer at once.
    tokio::select! {
        result = &mut task => {
            let pid = result.map_err(|e| anyhow::anyhow!("launch task failed: {e}"))??;
            let response = LaunchResponse { launch_id: id, pid: Some(pid), emulator: emulator_id };
            Ok((StatusCode::CREATED, Json(response)))
        }
        _ = transfer.wait_started() => {
            let response = LaunchResponse { launch_id: id, pid: None, emulator: emulator_id };
            Ok((StatusCode::ACCEPTED, Json(response)))
        }
    }
}

struct Launch {
    state: Arc<AppState>,
    id: u64,
    req: LaunchRequest,
    rom: Option<crate::romm::types::RomDetail>,
    emulator_id: String,
    config: Arc<crate::config::Config>,
    romm: Option<Arc<crate::romm::RommClient>>,
    transfer: Transfer,
}

impl Launch {
    /// Fetch the ROM, sync, start the emulator, and record how it ended.
    async fn run(self) -> anyhow::Result<u32> {
        let (state, id, rom_id) = (self.state.clone(), self.id, self.req.rom_id);
        let result = self.start().await;
        let outcome = match &result {
            Ok(pid) => LaunchState::Started { pid: *pid },
            Err(_) if state.launches.was_cancelled(id) => {
                info!("launch of rom {} cancelled while fetching the ROM", rom_id);
                LaunchState::Cancelled
            }
            Err(e) => {
                warn!("launching rom {}: {:#}", rom_id, e);
                LaunchState::Failed {
                    error: format!("{e:#}"),
                }
            }
        };
        state.launches.finish(id, outcome);
        result
    }

    async fn start(self) -> anyhow::Result<u32> {
        let Launch {
            state,
            id,
            req,
            rom,
            emulator_id,
            config,
            romm,
            transfer,
        } = self;
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
                let client = romm.as_ref().expect("checked by the handler");
                let _awake = keep_awake_while_downloading(&transfer, rom);
                client
                    .download_rom_to_dir(rom, &config.roms_dir(), &transfer)
                    .await?
            }
            (None, local) => PathBuf::from(local.as_deref().unwrap_or_default()),
        };
        anyhow::ensure!(
            state.launches.to_starting(id),
            "cancelled while fetching the ROM"
        );

        if let Some(problem) = crate::emulator::config_dir_problem(emulator.as_ref(), emu_config) {
            tracing::warn!("[emulators.{}] {}", emulator_id, problem);
        }
        let game_id = emulator.game_id_for_rom(emu_config, &rom_path);

        let save_layout = emulator
            .save_layout(emu_config, &rom_path, game_id.as_deref())
            .await;

        let present_at_launch = match &romm {
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

        let (command, launch_ctx) = emulator
            .command(
                emu_config,
                &rom_path,
                game_id.as_deref(),
                req.profile.as_ref(),
            )
            .await?;
        let started = if req.attach {
            attached(&state, id, command).await
        } else {
            spawned(&command, emulator.display_name())
        };
        let (pid, child, tree) = match started {
            Ok(started) => started,
            Err(e) => {
                launch_ctx.clean_up().await;
                return Err(e);
            }
        };
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
            romm.clone(),
            emulator.clone(),
            emu_config.clone(),
            state.sync_store.clone(),
        );
        Ok(pid)
    }
}

/// How long an attached launch waits for its client to start the game.
const ATTACH_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

fn spawned(
    command: &crate::emulator::LaunchCommand,
    name: &str,
) -> anyhow::Result<(u32, Option<tokio::process::Child>, ProcessTree)> {
    let child = command
        .spawn()
        .with_context(|| format!("starting {name} from {}", command.program.display()))?;
    let exited = || anyhow::anyhow!("{name} exited immediately");
    let tree = ProcessTree::new(&child).ok_or_else(exited)?;
    let pid = child.id().ok_or_else(exited)?;
    Ok((pid, Some(child), tree))
}

/// Hand `command` to the client of an attached launch and wait for it to
/// report the game's PID.
async fn attached(
    state: &AppState,
    id: u64,
    command: crate::emulator::LaunchCommand,
) -> anyhow::Result<(u32, Option<tokio::process::Child>, ProcessTree)> {
    let reported = state.launches.to_ready(id, command);
    let pid = match tokio::time::timeout(ATTACH_WAIT, reported).await {
        Ok(Ok(Attached::Started { pid })) => pid,
        Ok(Ok(Attached::Failed { error })) => anyhow::bail!("{error}"),
        Ok(Err(_)) => anyhow::bail!("the launch is gone"),
        Err(_) => anyhow::bail!(
            "nothing started the game within {} s of it being ready",
            ATTACH_WAIT.as_secs()
        ),
    };
    let tree = ProcessTree::attach(pid)?;
    Ok((pid, None, tree))
}

/// Keep the computer from idle sleep once bytes flow, until the returned
/// guard is dropped (the download ended). A ROM already on disk never starts
/// a transfer, so nothing is held for it.
fn keep_awake_while_downloading(
    transfer: &Transfer,
    rom: &crate::romm::types::RomDetail,
) -> tokio_util::sync::DropGuard {
    let done = tokio_util::sync::CancellationToken::new();
    let (transfer, finished) = (transfer.clone(), done.clone());
    let why = format!(
        "downloading {}",
        rom.name.as_deref().unwrap_or(&rom.fs_name)
    );
    tokio::spawn(async move {
        tokio::select! {
            _ = transfer.wait_started() => {
                let awake = tokio::task::spawn_blocking(move || crate::awake::KeepAwake::start(&why))
                    .await
                    .ok()
                    .flatten();
                finished.cancelled().await;
                drop(awake);
            }
            _ = finished.cancelled() => {}
        }
    });
    done.drop_guard()
}

#[utoipa::path(
    get,
    path = "/launches/{id}",
    params(("id" = u64, Path, description = "From the `POST /launch` response")),
    responses(
        (status = 200, description = "How the launch is going", body = LaunchInfo),
        (status = 404, description = "No launch with this ID in the last few minutes")
    )
)]
pub async fn launch_info(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> Response {
    match state.launches.info(id) {
        Some(info) => Json(info).into_response(),
        None => not_found(id),
    }
}

#[utoipa::path(
    delete,
    path = "/launches/{id}",
    params(("id" = u64, Path, description = "From the `POST /launch` response")),
    responses(
        (status = 204, description = "The ROM download is cancelled and its partial file removed"),
        (status = 404, description = "No launch with this ID in the last few minutes"),
        (status = 409, description = "Too late: the ROM is there and the game is starting or has started")
    )
)]
pub async fn cancel_launch(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> Response {
    match state.launches.cancel(id) {
        Cancel::Cancelled => StatusCode::NO_CONTENT.into_response(),
        Cancel::NotFound => not_found(id),
        Cancel::TooLate => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "the ROM is already there; the game is starting or has started"
            })),
        )
            .into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/launches/{id}/attached",
    params(("id" = u64, Path, description = "From the `POST /launch` response")),
    request_body = AttachedRequest,
    responses(
        (status = 204, description = "Reported; the launch goes on to `started` (or `failed` \
            when the PID can't be tracked)"),
        (status = 403, description = "Sent from a web page; only local programs attach"),
        (status = 404, description = "No launch with this ID in the last few minutes"),
        (status = 409, description = "The launch is not waiting for a report: not attached, \
            not `ready` yet, or already reported")
    )
)]
/// The client of an attached launch started the game (or couldn't). Only for
/// programs, not web pages: the daemon would track, and could stop, any
/// process a page named.
pub async fn attached_launch(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<u64>,
    headers: axum::http::HeaderMap,
    Json(req): Json<AttachedRequest>,
) -> Response {
    if headers.contains_key(axum::http::header::ORIGIN) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "only local programs can attach to a launch" })),
        )
            .into_response();
    }
    let report = match req {
        AttachedRequest { pid: Some(pid), .. } => Attached::Started { pid },
        AttachedRequest { error, .. } => Attached::Failed {
            error: error.unwrap_or_else(|| "the game could not be started".into()),
        },
    };
    match state.launches.attach(id, report) {
        Attach::Reported => StatusCode::NO_CONTENT.into_response(),
        Attach::NotFound => not_found(id),
        Attach::NotWaiting => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "this launch is not waiting for a game to start" })),
        )
            .into_response(),
    }
}

fn not_found(id: u64) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": format!("no launch {id}") })),
    )
        .into_response()
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

    let launches = state.launches.list();
    Json(StatusResponse { running, launches })
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
    if state.launches.busy() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "a game is downloading or starting; try again when it runs" })),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::ryujinx::Ryujinx;
    use crate::testing::TempDir;
    use crate::testing::fake_romm::FakeRomm;
    use serde_json::{Value, json};
    use std::time::Duration;

    /// rommd on a local port, with RomM faked and a Ryujinx that can't start.
    async fn rommd(dir: &TempDir, fake: &FakeRomm) -> String {
        let root = dir.path().display();
        let toml = format!(
            "roms_dir = '{root}/roms'\n\
             [romm]\nbase_url = '{}'\napi_token = 'test'\n\
             [emulators.ryujinx]\nbinary = '{root}/no-ryujinx'\nconfig_dir = '{root}/ryujinx'\n",
            fake.base_url()
        );
        let path = dir.path().join("config.toml");
        let config = crate::config::Config::parse(&toml, &path).unwrap();
        let emulators: std::collections::HashMap<String, Arc<dyn crate::emulator::Emulator>> =
            [("ryujinx".to_owned(), Arc::new(Ryujinx) as _)].into();
        let state = Arc::new(AppState {
            updater: Arc::new(crate::update::Updater::new(&config.updates)),
            live: Arc::new(crate::live::Live::new(path, config)),
            processes: crate::process::ProcessManager::new(),
            emulators,
            sync_store: Arc::new(crate::sync::SyncStore::load(dir.path().join("sync.json"))),
            launches: Default::default(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::server::router(state);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    async fn get(http: &reqwest::Client, url: String) -> Value {
        http.get(url).send().await.unwrap().json().await.unwrap()
    }

    #[tokio::test]
    async fn a_launch_answers_while_its_rom_downloads_and_can_be_cancelled() {
        let fake = FakeRomm::start().await;
        fake.seed_platform(22, "switch", "switch", "Nintendo Switch");
        fake.seed_rom(199, "Xenoblade", 22);
        let dir = TempDir::new("launch-download");
        let base = rommd(&dir, &fake).await;
        let http = reqwest::Client::new();
        let launch = || {
            http.post(format!("{base}/launch"))
                .json(&json!({ "rom_id": 199 }))
                .send()
        };

        let first = tokio::time::timeout(Duration::from_secs(10), launch())
            .await
            .expect("the answer must not wait for the download")
            .unwrap();
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        let body: Value = first.json().await.unwrap();
        let id = body["launch_id"].as_u64().unwrap();
        assert_eq!(body["emulator"], "ryujinx", "picked for the platform");
        assert!(body["pid"].is_null());

        // 202 comes as soon as RomM starts answering; the first bytes follow.
        let part = dir.path().join("roms/switch/Xenoblade.bin.part");
        let mut info = Value::Null;
        for _ in 0..50 {
            info = get(&http, format!("{base}/launches/{id}")).await;
            if info["downloaded_bytes"].as_u64() > Some(0) && part.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(info["state"], "downloading", "{info}");
        assert!(info["downloaded_bytes"].as_u64() > Some(0), "{info}");
        assert!(part.exists());
        let again: Value = launch().await.unwrap().json().await.unwrap();
        assert_eq!(again["launch_id"], id, "a second launch follows the first");

        let cancel = http
            .delete(format!("{base}/launches/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(cancel.status(), StatusCode::NO_CONTENT);
        let mut state = Value::Null;
        for _ in 0..50 {
            state = get(&http, format!("{base}/launches/{id}")).await["state"].clone();
            if state == "cancelled" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(state, "cancelled");
        assert!(!part.exists(), "the partial download is removed");
        let status = get(&http, format!("{base}/status")).await;
        assert_eq!(status["launches"][0]["state"], "cancelled");
    }

    #[tokio::test]
    async fn a_launch_of_a_downloaded_rom_answers_when_it_has_started_or_failed() {
        let fake = FakeRomm::start().await;
        fake.seed_platform(22, "switch", "switch", "Nintendo Switch");
        fake.seed_rom(5, "Odyssey", 22);
        let dir = TempDir::new("launch-cached");
        crate::testing::write(&dir.path().join("roms/switch/Odyssey.bin"), b"rom");
        let base = rommd(&dir, &fake).await;
        let http = reqwest::Client::new();

        let response = http
            .post(format!("{base}/launch"))
            .json(&json!({ "rom_id": 5 }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let error = response.json::<Value>().await.unwrap()["error"].to_string();
        assert!(error.contains("Ryujinx"), "{error}");
        let status = get(&http, format!("{base}/status")).await;
        assert_eq!(status["launches"][0]["state"], "failed", "{status}");
        let cancel = http
            .delete(format!("{base}/launches/1"))
            .send()
            .await
            .unwrap();
        assert_eq!(cancel.status(), StatusCode::CONFLICT);
    }

    async fn wait_for_state(http: &reqwest::Client, base: &str, id: u64, state: &str) -> Value {
        let mut info = Value::Null;
        for _ in 0..100 {
            info = get(http, format!("{base}/launches/{id}")).await;
            if info["state"] == state {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        info
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn an_attached_launch_hands_over_the_command_and_tracks_the_game_it_reports() {
        let fake = FakeRomm::start().await;
        fake.seed_platform(22, "switch", "switch", "Nintendo Switch");
        fake.seed_rom(5, "Odyssey", 22);
        let dir = TempDir::new("launch-attached");
        let rom = dir.path().join("roms/switch/Odyssey.bin");
        crate::testing::write(&rom, b"rom");
        let base = rommd(&dir, &fake).await;
        let http = reqwest::Client::new();

        let response = http
            .post(format!("{base}/launch"))
            .json(&json!({ "rom_id": 5, "attach": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let id = response.json::<Value>().await.unwrap()["launch_id"]
            .as_u64()
            .unwrap();
        let info = wait_for_state(&http, &base, id, "ready").await;
        assert_eq!(info["state"], "ready", "{info}");
        let command: crate::emulator::LaunchCommand =
            serde_json::from_value(info["command"].clone()).unwrap();
        assert_eq!(command.program, dir.path().join("no-ryujinx"));
        assert_eq!(command.args, [rom.display().to_string()]);
        assert!(
            get(&http, format!("{base}/status")).await["running"]
                .as_array()
                .unwrap()
                .is_empty(),
            "the daemon starts nothing itself"
        );

        // The client runs the command; a stand-in game that ends by itself.
        let mut game = crate::emulator::LaunchCommand::new(std::path::Path::new("sleep"));
        game.arg("1");
        let mut child = game.spawn().unwrap();
        let pid = child.id().unwrap();
        let report = |origin: Option<&str>| {
            let mut request = http
                .post(format!("{base}/launches/{id}/attached"))
                .json(&json!({ "pid": pid }));
            if let Some(origin) = origin {
                request = request.header("Origin", origin);
            }
            request.send()
        };
        let from_page = report(Some(fake.base_url())).await.unwrap();
        assert_eq!(
            from_page.status(),
            StatusCode::FORBIDDEN,
            "web pages can't attach"
        );
        assert_eq!(report(None).await.unwrap().status(), StatusCode::NO_CONTENT);
        assert_eq!(
            report(None).await.unwrap().status(),
            StatusCode::CONFLICT,
            "one report per launch"
        );

        let info = wait_for_state(&http, &base, id, "started").await;
        assert_eq!(info["pid"], pid, "{info}");
        let running = get(&http, format!("{base}/status")).await["running"].clone();
        assert_eq!(running[0]["pid"], pid, "{running}");

        child.wait().await.unwrap();
        let mut running = Value::Null;
        for _ in 0..50 {
            running = get(&http, format!("{base}/status")).await["running"].clone();
            if running.as_array().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            running,
            json!([]),
            "the game's end is noticed without a child handle"
        );
    }

    #[tokio::test]
    async fn an_attached_launch_fails_when_its_client_reports_a_failure() {
        let fake = FakeRomm::start().await;
        fake.seed_platform(22, "switch", "switch", "Nintendo Switch");
        fake.seed_rom(5, "Odyssey", 22);
        let dir = TempDir::new("launch-attached-failed");
        crate::testing::write(&dir.path().join("roms/switch/Odyssey.bin"), b"rom");
        let base = rommd(&dir, &fake).await;
        let http = reqwest::Client::new();
        let response: Value = http
            .post(format!("{base}/launch"))
            .json(&json!({ "rom_id": 5, "attach": true }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = response["launch_id"].as_u64().unwrap();
        wait_for_state(&http, &base, id, "ready").await;
        let report = http
            .post(format!("{base}/launches/{id}/attached"))
            .json(&json!({ "error": "no such file" }))
            .send()
            .await
            .unwrap();
        assert_eq!(report.status(), StatusCode::NO_CONTENT);
        let info = wait_for_state(&http, &base, id, "failed").await;
        assert_eq!(info["error"], "no such file", "{info}");
    }
}
