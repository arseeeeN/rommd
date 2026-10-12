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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use config::Config;
use emulator::{
    Emulator, azahar::Azahar, dolphin::Dolphin, duckstation::DuckStation, eden::Eden, mgba::MGba,
    ryujinx::Ryujinx,
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
    /// Log to a file instead of the terminal and, on Windows, close the
    /// console window. Used by the Windows logon task.
    #[arg(long, hide = true)]
    background: bool,
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
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Service {
            action: ServiceAction::Install,
        }) => service::install(),
        Some(Command::Service {
            action: ServiceAction::Uninstall,
        }) => service::uninstall(),
        None => {
            let result = tokio::runtime::Runtime::new()?.block_on(run_daemon(cli.background));
            // In the background stderr goes nowhere; the log is all there is.
            if cli.background
                && let Err(e) = &result
            {
                tracing::error!("{:#}", e);
            }
            result
        }
    }
}

/// Where `--background` writes the log.
pub fn log_path() -> PathBuf {
    sync::data_dir().join("rommd.log")
}

const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

fn init_logging(background: bool) -> Result<()> {
    let fmt = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::from_default_env()
            .add_directive("rommd=debug".parse().unwrap()),
    );
    if !background {
        fmt.init();
        return Ok(());
    }
    let path = log_path();
    let file = open_log(&path).with_context(|| format!("opening {}", path.display()))?;
    fmt.with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .init();
    // A panic message would go to the console, which is gone.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        tracing::error!("{}", panic);
        default_hook(panic);
    }));
    #[cfg(windows)]
    service::console::detach();
    Ok(())
}

/// Append to the log, moving it to `rommd.log.old` once it grows large.
fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

async fn run_daemon(background: bool) -> Result<()> {
    init_logging(background)?;

    let config = Config::load_or_create().context("loading config")?;
    info!(
        "rommd {} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("ROMMD_TARGET")
    );
    info!("config loaded from {}", Config::path().display());
    info!("roms_dir: {}", config.roms_dir().display());

    let romm = config.romm.as_ref().map(|c| Arc::new(RommClient::new(c)));
    match (&romm, &config.romm) {
        (Some(client), Some(romm_config)) => {
            let client = client.clone();
            let url = romm_config.base_url.clone();
            tokio::spawn(async move {
                match client.check().await {
                    Ok(version) => info!("connected to RomM {} at {}", version, url),
                    Err(e) => warn!("RomM: {:#}", e),
                }
            });
        }
        _ => warn!("no [romm] section in the config: games can only launch with local_path"),
    }

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

    for (id, emu_config) in &config.emulators {
        match emulators.get(id) {
            Some(emu) => {
                if let Some(problem) = emulator::config_dir_problem(emu.as_ref(), emu_config) {
                    warn!("[emulators.{}] {}", id, problem);
                }
            }
            None => {
                let mut known: Vec<_> = emulators.keys().map(String::as_str).collect();
                known.sort_unstable();
                warn!(
                    "[emulators.{}] is not an emulator rommd knows; use one of: {}",
                    id,
                    known.join(", ")
                );
            }
        }
    }

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
    // The previous version has released the port, so it has exited.
    update::remove_replaced_binary();
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
