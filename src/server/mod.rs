pub mod handlers;
pub mod state;
pub mod types;

use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Arc;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

use state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(handlers::health))
        .route("/emulators", get(handlers::list_emulators))
        .route("/launch", post(handlers::launch))
        .route("/status", get(handlers::status))
        .route("/stop", post(handlers::stop))
        .route("/snapshot", post(handlers::snapshot))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
