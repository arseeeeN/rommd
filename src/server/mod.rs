pub mod handlers;
pub mod state;
pub mod types;

use axum::{Json, Router, routing::get, routing::post};
use std::sync::Arc;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use utoipa::OpenApi;

use state::AppState;
use types::*;
use crate::profile::ProfileBlob;

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
        handlers::status,
        handlers::stop,
        handlers::snapshot,
    ),
    components(schemas(
        HealthResponse,
        EmulatorInfo,
        LaunchRequest,
        LaunchResponse,
        StatusResponse,
        RunningGameInfo,
        StopRequest,
        SnapshotRequest,
        SnapshotResponse,
        ProfileBlob,
    ))
)]
struct ApiDoc;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(handlers::health))
        .route("/emulators", get(handlers::list_emulators))
        .route("/launch", post(handlers::launch))
        .route("/status", get(handlers::status))
        .route("/stop", post(handlers::stop))
        .route("/snapshot", post(handlers::snapshot))
        .route("/openapi.json", get(|| async { Json(ApiDoc::openapi()) }))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
