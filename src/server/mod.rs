pub mod handlers;
pub mod security;
pub mod state;
pub mod types;

use axum::{Json, Router, routing::get, routing::post};
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;

use crate::profile::ProfileBlob;
use state::AppState;
use types::*;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "rommd",
        description = "Local emulator daemon bridging RomM with emulators",
        version = env!("CARGO_PKG_VERSION"),
        license(name = "MIT"),
    ),
    paths(
        handlers::health,
        handlers::list_emulators,
        handlers::launch,
        handlers::games,
        handlers::launch_info,
        handlers::cancel_launch,
        handlers::status,
        handlers::stop,
        handlers::snapshot,
        handlers::list_controllers,
        handlers::save_conflicts,
        handlers::update_status,
        handlers::update_install,
    ),
    components(schemas(
        HealthResponse,
        EmulatorInfo,
        LaunchRequest,
        LaunchResponse,
        GamesResponse,
        GameMatch,
        ControllerSlot,
        StatusResponse,
        RunningGameInfo,
        crate::launches::LaunchInfo,
        crate::launches::LaunchState,
        StopRequest,
        SnapshotRequest,
        SnapshotResponse,
        ProfileBlob,
        ControllerInfo,
        ControllerType,
        crate::sync::ConflictRecord,
        crate::update::UpdateStatus,
        crate::update::UpdateMode,
        UpdateInstalled,
    ))
)]
struct ApiDoc;

pub fn router(state: Arc<AppState>) -> Router {
    let cors = security::cors_layer(state.live.clone());
    let guard = state.live.clone();
    Router::new()
        .route("/health", get(handlers::health))
        .route("/emulators", get(handlers::list_emulators))
        .route("/launch", post(handlers::launch))
        .route("/games", get(handlers::games))
        .route(
            "/launches/{id}",
            get(handlers::launch_info).delete(handlers::cancel_launch),
        )
        .route("/status", get(handlers::status))
        .route("/stop", post(handlers::stop))
        .route("/snapshot", post(handlers::snapshot))
        .route("/controllers", get(handlers::list_controllers))
        .route("/saves/conflicts", get(handlers::save_conflicts))
        .route(
            "/update",
            get(handlers::update_status).post(handlers::update_install),
        )
        .route("/openapi.json", get(|| async { Json(ApiDoc::openapi()) }))
        .layer(axum::middleware::from_fn_with_state(
            guard,
            security::check_request,
        ))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
