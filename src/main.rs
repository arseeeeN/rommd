mod config;
mod emulator;
mod error;
mod ini;
mod process;
mod profile;
mod romm;
mod server;
mod switch;

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::info;

use config::Config;
use emulator::{dolphin::Dolphin, duckstation::DuckStation, mgba::MGba, ryujinx::Ryujinx, Emulator};
use process::ProcessManager;
use romm::RommClient;
use server::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("rommd=debug".parse().unwrap()),
        )
        .init();

    let config = Config::load_or_create().context("loading config")?;
    info!("config loaded from {}", Config::path().display());
    info!("roms_dir: {}", config.roms_dir().display());

    let romm = config.romm.as_ref().map(|c| Arc::new(RommClient::new(c)));

    let mut emulators: HashMap<String, Arc<dyn Emulator>> = HashMap::new();
    // Use id() as the registry key so the trait and the map stay in sync.
    let ds = DuckStation;
    emulators.insert(ds.id().to_owned(), Arc::new(ds));
    let mgba = MGba;
    emulators.insert(mgba.id().to_owned(), Arc::new(mgba));
    let dolphin = Dolphin;
    emulators.insert(dolphin.id().to_owned(), Arc::new(dolphin));
    let ryujinx = Ryujinx;
    emulators.insert(ryujinx.id().to_owned(), Arc::new(ryujinx));

    let state = Arc::new(AppState {
        config,
        processes: ProcessManager::new(),
        romm,
        emulators,
    });

    let port = state.config.server.port;
    let addr = format!("127.0.0.1:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding to {}", addr))?;

    info!("rommd listening on http://{}", addr);

    axum::serve(listener, server::router(state))
        .await
        .context("server error")
}
