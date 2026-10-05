mod config;
mod emulator;
mod error;
mod ini;
mod n3ds;
mod process;
mod profile;
mod romm;
mod save_archive;
mod server;
mod service;
mod switch;
mod sync;
#[cfg(test)]
mod testing;
mod update;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use config::Config;
use emulator::{
    azahar::Azahar, dolphin::Dolphin, duckstation::DuckStation, eden::Eden, mgba::MGba, ryujinx::Ryujinx,
    Emulator,
};
use process::ProcessManager;
use romm::RommClient;
use server::state::AppState;

/// Local daemon bridging RomM with emulators. Without a command it runs the
/// daemon.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start rommd automatically when you log in.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Register this binary to start at login, and start it now.
    Install,
    /// Stop it and remove the registration.
    Uninstall,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Some(Command::Service { action: ServiceAction::Install }) => service::install(),
        Some(Command::Service { action: ServiceAction::Uninstall }) => service::uninstall(),
        None => tokio::runtime::Runtime::new()?.block_on(run_daemon()),
    }
}

async fn run_daemon() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("rommd=debug".parse().unwrap()),
        )
        .init();

    let config = Config::load_or_create().context("loading config")?;
    info!("rommd {} ({})", env!("CARGO_PKG_VERSION"), env!("ROMMD_TARGET"));
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
    let eden = Eden;
    emulators.insert(eden.id().to_owned(), Arc::new(eden));
    let azahar = Azahar;
    emulators.insert(azahar.id().to_owned(), Arc::new(azahar));

    let updater = Arc::new(update::Updater::new(&config.updates));
    let state = Arc::new(AppState {
        updater: updater.clone(),
        config,
        processes: ProcessManager::new(),
        romm,
        emulators,
        sync_store: Arc::new(sync::SyncStore::load(sync::SyncStore::default_path())),
    });

    let port = state.config.server.port;
    let addr = format!("127.0.0.1:{}", port);
    let listener = bind(&addr).await?;
    info!("rommd listening on http://{}", addr);
    update::spawn_background(updater, state.processes.clone());

    axum::serve(listener, server::router(state))
        .await
        .context("server error")
}

/// Bind the API port, retrying briefly: after an update the new version starts
/// while the old one is still shutting down.
async fn bind(addr: &str) -> Result<tokio::net::TcpListener> {
    for attempt in 1..=20 {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && attempt < 20 => {
                if attempt == 1 {
                    warn!("{} is in use; waiting for it to be released", addr);
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => return Err(e).with_context(|| format!("binding to {}", addr)),
        }
    }
    unreachable!()
}
