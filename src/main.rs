mod awake;
mod cli;
mod config;
mod config_edit;
mod detect;
mod emulator;
mod error;
mod games;
mod ini;
mod launches;
mod live;
mod n3ds;
mod process;
mod profile;
mod romm;
mod save_archive;
mod server;
mod service;
mod setup;
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
use emulator::Emulator;
use process::ProcessManager;
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
    /// Find games in your RomM library. The query is words from the name plus
    /// filters: platform:switch, emulator:eden, id:199 (quote values with
    /// spaces). Needs rommd running.
    Search {
        /// Most games to list.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(required = true, num_args = 1.., value_name = "QUERY")]
        query: Vec<String>,
    },
    /// Connect rommd to RomM and add the emulators found here, by answering
    /// a few questions. Changes only what you answer; run it again any time.
    Setup {
        /// Ask nothing: RomM's address and API token come from ROMMD_URL and
        /// ROMMD_TOKEN (or stay as configured), emulators found in the
        /// install folders are added, everything else keeps its default.
        #[arg(long)]
        yes: bool,
        /// Do nothing if the config already has RomM settings.
        #[arg(long)]
        if_needed: bool,
    },
    /// List the emulators installed on this computer and where their data
    /// is, next to what the config says. Doesn't need rommd running.
    Detect {
        /// Look here instead: an app, or a folder holding one (e.g. an
        /// emulator kept on the Desktop).
        path: Option<std::path::PathBuf>,
        /// Add the emulators found to the config. Entries already there are
        /// never changed.
        #[arg(long)]
        save: bool,
    },
    /// Start a game found by a query, as for `search`. With several matches
    /// it asks which one, or takes the best match with --first.
    Play {
        /// Play the best match without asking.
        #[arg(long)]
        first: bool,
        /// Start the emulator from this process and wait until the game
        /// ends, instead of the daemon starting it. For Steam shortcuts: in
        /// Game Mode, Steam only shows and controls games it started.
        #[arg(long)]
        attach: bool,
        #[arg(required = true, num_args = 1.., value_name = "QUERY")]
        query: Vec<String>,
    },
    /// Run rommd in the background from login, and start, stop or restart it.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Register this binary to start at login, and start it now.
    Install,
    /// Start it now if it is not running. Checks the config first.
    Start,
    /// Stop it until the next login. Refuses while a game is running.
    Stop,
    /// Restart it, e.g. after editing the config. Checks the config first and
    /// refuses while a game is running.
    Restart,
    /// Stop it and remove the registration.
    Uninstall,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let control = |action| tokio::runtime::Runtime::new()?.block_on(service::control(action));
    match cli.command {
        Some(Command::Search { limit, query }) => {
            tokio::runtime::Runtime::new()?.block_on(cli::search(&query, limit))
        }
        Some(Command::Setup { yes, if_needed }) => {
            tokio::runtime::Runtime::new()?.block_on(setup::run(setup::Options { yes, if_needed }))
        }
        Some(Command::Detect { path, save }) => {
            cli::detect(&emulator::all(), path.as_deref(), save)
        }
        Some(Command::Play {
            first,
            attach,
            query,
        }) => tokio::runtime::Runtime::new()?.block_on(cli::play(&query, first, attach)),
        Some(Command::Service { action }) => match action {
            ServiceAction::Install => service::install(),
            ServiceAction::Start => control(service::Control::Start),
            ServiceAction::Stop => control(service::Control::Stop),
            ServiceAction::Restart => control(service::Control::Restart),
            ServiceAction::Uninstall => service::uninstall(),
        },
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

    let emulators: HashMap<String, Arc<dyn Emulator>> = emulator::all()
        .into_iter()
        .map(|e| (e.id().to_owned(), e))
        .collect();

    check_emulators(&emulators, &config, None);
    let live = Arc::new(live::Live::new(Config::path(), config));
    check_romm(&live.get());

    let updater = Arc::new(update::Updater::new(&live.config().updates));
    live::watch(live.clone(), {
        let (updater, emulators) = (updater.clone(), emulators.clone());
        move |snapshot, changes| apply_changes(snapshot, changes, &updater, &emulators)
    });
    let state = Arc::new(AppState {
        updater: updater.clone(),
        live,
        processes: ProcessManager::new(),
        emulators,
        sync_store: Arc::new(sync::SyncStore::load(sync::SyncStore::default_path())),
        launches: Default::default(),
    });

    let port = state.config().server.port;
    let addr = format!("127.0.0.1:{}", port);
    let listener = bind(&addr).await?;
    info!("rommd listening on http://{}", addr);
    // The previous version has released the port, so it has exited.
    update::remove_replaced_binary();
    update::spawn_background(updater, state.processes.clone(), state.launches.clone());

    axum::serve(listener, server::router(state))
        .await
        .context("server error")
}

/// React to a reloaded config beyond what the next request picks up anyway.
fn apply_changes(
    snapshot: &live::Snapshot,
    changes: &[live::Change],
    updater: &Arc<update::Updater>,
    emulators: &HashMap<String, Arc<dyn Emulator>>,
) {
    for change in changes {
        match change {
            live::Change::Romm => check_romm(snapshot),
            live::Change::Updates => {
                let (updater, config) = (updater.clone(), snapshot.config.updates.clone());
                tokio::spawn(async move { updater.reconfigure(&config).await });
            }
            live::Change::Emulators(ids) => check_emulators(emulators, &snapshot.config, Some(ids)),
            live::Change::Port => warn!(
                "the new port {} takes effect after a restart (rommd service restart)",
                snapshot.config.server.port
            ),
            _ => {}
        }
    }
}

/// Log whether RomM is reachable with the configured credentials.
fn check_romm(snapshot: &live::Snapshot) {
    match (&snapshot.romm, &snapshot.config.romm) {
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
}

/// Warn about emulator entries that won't work; `only` limits the check to
/// the entries a reload touched.
fn check_emulators(
    emulators: &HashMap<String, Arc<dyn Emulator>>,
    config: &Config,
    only: Option<&[String]>,
) {
    for (id, emu_config) in &config.emulators {
        if only.is_some_and(|ids| !ids.contains(id)) {
            continue;
        }
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
