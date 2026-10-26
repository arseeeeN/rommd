//! `rommd search` and `rommd play`: a terminal client of the running daemon,
//! which does the searching (`GET /games`) and launching (`POST /launch`), so
//! a game started here is tracked and synced like any other.

use anyhow::{Context, Result};
use std::io::{BufRead, IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{Config, ServerConfig};
use crate::emulator::{Emulator, LaunchCommand};
use crate::launches::{LaunchInfo, LaunchState};
use crate::server::types::{GameMatch, GamesResponse};

const PLAY_CHOICES: u32 = 10;

pub async fn search(words: &[String], limit: u32) -> Result<()> {
    let daemon = Daemon::new()?;
    let found = daemon.games(&query_string(words), limit).await?;
    if found.games.is_empty() {
        println!("No games match.");
        return Ok(());
    }
    print_games(&mut std::io::stdout(), &found.games, false)?;
    if found.total > found.games.len() as i64 {
        println!(
            "Showing {} of {}; refine the query or raise --limit.",
            found.games.len(),
            found.total
        );
    }
    Ok(())
}

pub async fn play(words: &[String], first: bool, attach: bool) -> Result<()> {
    let daemon = Daemon::new()?;
    let query = query_string(words);
    let found = daemon.games(&query, PLAY_CHOICES).await?;
    let game = match pick(&found, first)? {
        Pick::Game(game) => game,
        Pick::Ask => match ask(&found)? {
            Some(game) => game,
            None => return Ok(()),
        },
    };
    let emulator = game
        .emulator
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("can't play {}: {}", game.name, game.emulator_reason))?;
    println!(
        "Starting {} ({}) with {}: {}.",
        game.name, game.platform_name, emulator, game.emulator_reason
    );
    if attach {
        return play_attached(&daemon, game, emulator).await;
    }
    let pid = launch_showing_progress(&daemon, game, emulator).await?;
    println!("Running (pid {pid}). Saves sync when the game exits.");
    Ok(())
}

/// `rommd play --attach`: the daemon downloads the ROM and syncs the saves,
/// then the emulator is started here, as this process's child, and this
/// process lives until the game has ended. Whatever started `rommd play`
/// (Steam in Game Mode) then owns the game: gamescope shows it, Steam Input
/// maps the controller, and Steam shows it as playing until it ends. The
/// daemon tracks the game as usual and syncs after it exits, even when this
/// process is killed first.
async fn play_attached(daemon: &Daemon, game: &GameMatch, emulator: &str) -> Result<()> {
    let config = Config::read(&Config::path()).context("reading the config")?;
    let id = match daemon.launch(game.rom_id, emulator, true).await? {
        Launched::Downloading(id) => id,
        Launched::Started(_) => anyhow::bail!("rommd started the game itself; is it up to date?"),
    };
    let command = match follow(daemon, id, true).await? {
        Followed::Ready(command) => command,
        Followed::Started(_) => anyhow::bail!("rommd started the game itself; is it up to date?"),
    };
    let started = trusted(&config, emulator, &command).and_then(|()| {
        command
            .spawn()
            .with_context(|| format!("starting {}", command.program.display()))
    });
    let child = match started {
        Ok(child) => child,
        Err(e) => {
            let _ = daemon.attached(id, Err(format!("{e:#}"))).await;
            return Err(e);
        }
    };
    let tree = crate::process::ProcessTree::new(&child)
        .ok_or_else(|| anyhow::anyhow!("{emulator} exited immediately"))?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("{emulator} exited immediately"))?;
    let tracked = match daemon.attached(id, Ok(pid)).await {
        Ok(()) => follow(daemon, id, true).await.map(|_| ()),
        Err(e) => Err(e),
    };
    match tracked {
        Ok(()) => println!("Running (pid {pid}). Saves sync when the game exits."),
        Err(e) => eprintln!("rommd is not tracking the game, so its saves won't sync: {e:#}"),
    }
    wait_for_game(child, tree).await;
    Ok(())
}

/// The daemon's command runs only the emulator binary the user configured, and
/// sets no variable outside `LaunchCommand::ENV`: anything could be listening
/// on rommd's port while rommd isn't (another user's program), and must not
/// get to run code as this user.
fn trusted(config: &Config, emulator: &str, command: &LaunchCommand) -> Result<()> {
    let configured = config
        .emulators
        .get(emulator)
        .ok_or_else(|| anyhow::anyhow!("{emulator} is not in your config"))?;
    anyhow::ensure!(
        command.program == configured.binary,
        "refusing to start {}: your config's {emulator} binary is {}",
        command.program.display(),
        configured.binary.display()
    );
    if let Some(name) = command
        .env
        .keys()
        .find(|name| !LaunchCommand::ENV.contains(&name.as_str()))
    {
        anyhow::bail!("refusing to start {emulator} with {name} set");
    }
    Ok(())
}

/// Wait until every process of the game has ended. Asked to end (Steam's
/// "Exit game", Ctrl+C, the terminal closing), close the game the way rommd
/// stops one: politely first, killed a few seconds later.
async fn wait_for_game(mut child: tokio::process::Child, tree: crate::process::ProcessTree) {
    let tree = Arc::new(tree);
    let mut signals = termination_signals();
    let mut stopping = false;
    loop {
        tokio::select! {
            _ = child.wait() => break,
            Some(()) = signals.recv(), if !stopping => {
                stopping = true;
                eprintln!("Closing the game.");
                tree.clone().stop().await;
            }
        }
    }
    loop {
        tokio::select! {
            _ = tree.wait_until_gone() => break,
            Some(()) = signals.recv(), if !stopping => {
                stopping = true;
                tree.clone().stop().await;
            }
        }
    }
}

/// Every request to end this process, as a stream.
fn termination_signals() -> tokio::sync::mpsc::UnboundedReceiver<()> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        for kind in [
            SignalKind::terminate(),
            SignalKind::interrupt(),
            SignalKind::hangup(),
        ] {
            let Ok(mut received) = signal(kind) else {
                continue;
            };
            let tx = tx.clone();
            tokio::spawn(
                async move { while received.recv().await.is_some() && tx.send(()).is_ok() {} },
            );
        }
    }
    #[cfg(windows)]
    tokio::spawn(
        async move { while tokio::signal::ctrl_c().await.is_ok() && tx.send(()).is_ok() {} },
    );
    rx
}

/// Start the game and wait until it runs. When the ROM has to be downloaded
/// first, the daemon answers at once and the download is followed here;
/// Ctrl+C then cancels it. Otherwise the game is already starting, and Ctrl+C
/// only stops waiting.
async fn launch_showing_progress(daemon: &Daemon, game: &GameMatch, emulator: &str) -> Result<u64> {
    tokio::select! {
        launched = daemon.launch(game.rom_id, emulator, false) => match launched? {
            Launched::Started(pid) => Ok(pid),
            Launched::Downloading(id) => match follow(daemon, id, false).await? {
                Followed::Started(pid) => Ok(pid),
                Followed::Ready(_) => anyhow::bail!("rommd waits for the game to be started"),
            },
        },
        _ = tokio::signal::ctrl_c() => {
            eprintln!("Stopped waiting; rommd still starts the game.");
            std::process::exit(130);
        }
    }
}

enum Followed {
    Started(u64),
    /// An attached launch, ready to be started here.
    Ready(LaunchCommand),
}

/// Follow a launch until the game started or, attached, is ready to start.
async fn follow(daemon: &Daemon, id: u64, attach: bool) -> Result<Followed> {
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    let mut progress = ProgressLine::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                progress.finish();
                match daemon.cancel(id).await {
                    Ok(true) => eprintln!("Download cancelled."),
                    Ok(false) if attach => {
                        let _ = daemon.attached(id, Err("stopped before the game started".into())).await;
                        eprintln!("Stopped before starting the game.");
                    }
                    Ok(false) => eprintln!("The download had just finished; rommd is starting the game."),
                    Err(e) => eprintln!("Could not cancel the download: {e:#}"),
                }
                std::process::exit(130);
            }
            _ = ticker.tick() => {
                let launch = daemon.launch_info(id).await?;
                match launch.state {
                    LaunchState::Preparing => {}
                    LaunchState::Downloading { downloaded_bytes, total_bytes } => {
                        progress.show(&download_line(&launch.name, downloaded_bytes, total_bytes));
                    }
                    LaunchState::Starting => progress.finish(),
                    LaunchState::Started { pid } => {
                        progress.finish();
                        return Ok(Followed::Started(pid.into()));
                    }
                    LaunchState::Ready { command } => {
                        progress.finish();
                        return Ok(Followed::Ready(command));
                    }
                    LaunchState::Failed { error } => {
                        progress.finish();
                        anyhow::bail!("{error}");
                    }
                    LaunchState::Cancelled => {
                        progress.finish();
                        anyhow::bail!("the download was cancelled");
                    }
                }
            }
        }
    }
}

/// A download's progress on stderr: one line rewritten in place on a
/// terminal, a single notice otherwise.
struct ProgressLine {
    terminal: bool,
    shown: bool,
}

impl ProgressLine {
    fn new() -> Self {
        Self {
            terminal: std::io::stderr().is_terminal(),
            shown: false,
        }
    }

    fn show(&mut self, line: &str) {
        if self.terminal {
            eprint!("\r{line:<70}");
        } else if !self.shown {
            eprintln!("{line}");
        }
        self.shown = true;
    }

    fn finish(&mut self) {
        if self.terminal && self.shown {
            eprintln!();
        }
        self.shown = false;
    }
}

fn download_line(name: &str, done: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => format!(
            "Downloading {name}: {} of {} ({}%)",
            size(done),
            size(total),
            done * 100 / total
        ),
        _ => format!("Downloading {name}: {}", size(done)),
    }
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1_000_000_000 => format!("{:.1} GB", b as f64 / 1e9),
        b if b >= 1_000_000 => format!("{:.0} MB", b as f64 / 1e6),
        b => format!("{} kB", b / 1000),
    }
}

/// `rommd detect`: the emulators installed here, next to what the config
/// says about them.
pub fn detect(
    emulators: &[Arc<dyn Emulator>],
    path: Option<&std::path::Path>,
    save: bool,
) -> Result<()> {
    if !crate::detect::supported() {
        println!("Emulator detection only covers macOS and Linux so far.");
        return Ok(());
    }
    let config = Config::read(&Config::path()).ok();
    let detection = match path {
        Some(path) => {
            anyhow::ensure!(path.exists(), "{} does not exist", path.display());
            crate::detect::detect_at(path, emulators)
        }
        None => crate::detect::detect(emulators, true),
    };
    if detection.found.is_empty() {
        match path {
            Some(path) => println!("No supported emulator found at {}.", path.display()),
            None => println!(
                "No supported emulator found. If one is somewhere unusual, name it: \
                 rommd detect <path>"
            ),
        }
    }
    for found in &detection.found {
        let configured = config
            .as_ref()
            .and_then(|c| c.emulators.get(found.emulator));
        let state = match configured {
            Some(c) if c.binary == found.binary && c.config_dir == found.config_dir => {
                "in your config"
            }
            Some(_) => "in your config with other paths",
            None => "not in your config",
        };
        println!("{} ({}, {state})", found.emulator, found.kind.label());
        println!("  binary      {}", home_relative(&found.binary));
        let unused = if found.config_dir_in_use {
            ""
        } else {
            "  (not created yet: run the emulator once)"
        };
        println!("  config_dir  {}{unused}", home_relative(&found.config_dir));
        if let Some(c) = configured.filter(|c| c.binary != found.binary) {
            println!("  your config: binary = {}", home_relative(&c.binary));
        }
        if let Some(c) = configured.filter(|c| c.config_dir != found.config_dir) {
            println!(
                "  your config: config_dir = {}",
                home_relative(&c.config_dir)
            );
        }
        if !found.installed {
            println!(
                "  note: move {} to {} first, or the config breaks when it is moved or \
                 deleted",
                home_relative(&found.install),
                install_folder()
            );
        }
        if let Some(sandbox) = &found.sandbox {
            let roms = config
                .as_ref()
                .map(Config::roms_dir)
                .unwrap_or_else(crate::config::default_roms_dir);
            if !sandbox.can_read(&roms) {
                println!(
                    "  note: the Flatpak may not see your ROMs in {}; let it read them with\n    \
                     flatpak override --user --filesystem={}:ro {}",
                    home_relative(&roms),
                    home_relative(&roms),
                    sandbox.app_id
                );
            }
        }
    }
    for other in &detection.also {
        println!(
            "Also installed, not used: {} ({}) at {}",
            other.emulator,
            other.kind.label(),
            home_relative(&other.install)
        );
    }
    for (platform, ids) in crate::detect::shared_platforms(&detection.found) {
        println!(
            "{} all run {platform}; [platforms] in the config picks one, e.g. {platform} = \"{}\"",
            ids.join(", "),
            ids[0]
        );
    }
    let undetectable: Vec<&str> = emulators
        .iter()
        .filter(|e| !crate::detect::detectable(e.as_ref()))
        .map(|e| e.id())
        .collect();
    if !undetectable.is_empty() && path.is_none() {
        println!("Not detected here yet: {}", undetectable.join(", "));
    }
    if save {
        save_detected(&detection, config.as_ref(), path.is_some())?;
    } else if detection.found.iter().any(|f| {
        f.installed
            && config
                .as_ref()
                .is_none_or(|c| !c.emulators.contains_key(f.emulator))
    }) {
        println!("Add them to the config with: rommd detect --save");
    }
    Ok(())
}

/// Add what was found to the config. Entries already there are kept as they
/// are, and an app outside the install folders is only added when the user named its
/// path, i.e. chose it knowingly.
fn save_detected(
    detection: &crate::detect::Detection,
    config: Option<&Config>,
    named: bool,
) -> Result<()> {
    let path = Config::path();
    if !path.exists() {
        Config::load_or_create()?;
    }
    println!();
    let mut added = 0;
    for found in &detection.found {
        let id = found.emulator;
        if config.is_some_and(|c| c.emulators.contains_key(id)) {
            println!("Kept your entry for {id}.");
        } else if !found.installed && !named {
            println!(
                "Skipped {id}: move it to {}, then run this again (or name it: \
                 rommd detect <path> --save).",
                install_folder()
            );
        } else {
            crate::config_edit::add_emulator(&path, id, &found.binary, &found.config_dir)
                .with_context(|| format!("adding {id} to {}", path.display()))?;
            println!("Added [emulators.{id}] to {}.", home_relative(&path));
            added += 1;
        }
    }
    if added > 0 {
        println!("rommd picks up the change by itself if it is running.");
    }
    Ok(())
}

/// Where `detect` expects apps to be installed.
fn install_folder() -> &'static str {
    if cfg!(target_os = "macos") {
        "Applications"
    } else if cfg!(windows) {
        r"a folder it can stay in, e.g. %LOCALAPPDATA%\Programs,"
    } else {
        "~/Applications (or add a desktop entry for it)"
    }
}

/// `~/…` for paths in the home folder; whole on Windows, where the config
/// holds them that way.
fn home_relative(path: &std::path::Path) -> String {
    if cfg!(windows) {
        return path.display().to_string();
    }
    match dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(|p| p.to_owned())) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

enum Launched {
    Started(u64),
    Downloading(u64),
}

enum Pick<'a> {
    Game(&'a GameMatch),
    Ask,
}

/// One match is played, `--first` plays the best playable one, several are
/// asked about on a terminal and refused otherwise.
fn pick(found: &GamesResponse, first: bool) -> Result<Pick<'_>> {
    match found.games.as_slice() {
        [] => anyhow::bail!("no game matches"),
        [only] => Ok(Pick::Game(only)),
        games if first => games
            .iter()
            .find(|g| g.emulator.is_some())
            .map(Pick::Game)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no emulator is configured for any match ({})",
                    games[0].emulator_reason
                )
            }),
        _ if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => Ok(Pick::Ask),
        games => {
            print_games(&mut std::io::stderr(), games, false)?;
            anyhow::bail!(
                "{} games match; refine the query (e.g. platform:…, id:…) or use --first",
                found.total
            )
        }
    }
}

fn ask(found: &GamesResponse) -> Result<Option<&GameMatch>> {
    let mut out = std::io::stdout();
    print_games(&mut out, &found.games, true)?;
    if found.total > found.games.len() as i64 {
        writeln!(
            out,
            "     … {} more; refine the query to see them.",
            found.total - found.games.len() as i64
        )?;
    }
    loop {
        write!(
            out,
            "Play which? [1-{}, Enter to cancel] ",
            found.games.len()
        )?;
        out.flush()?;
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line)? == 0 || line.trim().is_empty() {
            return Ok(None);
        }
        match line.trim().parse::<usize>() {
            Ok(n) if (1..=found.games.len()).contains(&n) => return Ok(Some(&found.games[n - 1])),
            _ => writeln!(out, "Type a number from the list.")?,
        }
    }
}

fn print_games(out: &mut impl Write, games: &[GameMatch], numbered: bool) -> Result<()> {
    let width = games
        .iter()
        .map(|g| g.name.chars().count())
        .max()
        .unwrap_or(0);
    for (i, g) in games.iter().enumerate() {
        let number = if numbered {
            format!("{:>3}) ", i + 1)
        } else {
            String::new()
        };
        let emulator = match &g.emulator {
            Some(e) => format!("-> {e}"),
            None => "(no emulator)".into(),
        };
        writeln!(
            out,
            "{number}{:<width$}  {:<8}  id:{:<6} {emulator}",
            g.name, g.platform, g.rom_id
        )?;
    }
    Ok(())
}

/// The shell has already split the query and removed its quotes; quote the
/// words that had spaces again so `platform:"Nintendo Switch"` stays one.
fn query_string(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            if w.chars().any(char::is_whitespace) && !w.contains('"') {
                format!("\"{w}\"")
            } else {
                w.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

struct Daemon {
    client: reqwest::Client,
    base: String,
}

impl Daemon {
    fn new() -> Result<Self> {
        let port = Config::load_or_create().map_or(ServerConfig::default().port, |c| c.server.port);
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .build()?,
            base: format!("http://127.0.0.1:{port}"),
        })
    }

    async fn games(&self, query: &str, limit: u32) -> Result<GamesResponse> {
        let request = self
            .client
            .get(format!("{}/games", self.base))
            .query(&[("q", query), ("limit", &limit.to_string())])
            .timeout(Duration::from_secs(30));
        let response = self.send(request).await?;
        response.json().await.context("reading the search results")
    }

    /// Returns the emulator's PID. Not time-limited: the daemon downloads the
    /// ROM first, which can take minutes.
    /// Not time-limited: for a ROM that is already here the daemon answers
    /// once the saves are synced and the emulator started.
    async fn launch(&self, rom_id: i64, emulator: &str, attach: bool) -> Result<Launched> {
        let request = self
            .client
            .post(format!("{}/launch", self.base))
            .json(&serde_json::json!({ "rom_id": rom_id, "emulator": emulator, "attach": attach }));
        let response = self.send(request).await?;
        let accepted = response.status() == reqwest::StatusCode::ACCEPTED;
        let body: serde_json::Value = response.json().await?;
        let field = |name: &str| {
            body[name]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("unexpected launch response: {body}"))
        };
        Ok(if accepted {
            Launched::Downloading(field("launch_id")?)
        } else {
            Launched::Started(field("pid")?)
        })
    }

    async fn launch_info(&self, id: u64) -> Result<LaunchInfo> {
        let request = self
            .client
            .get(format!("{}/launches/{id}", self.base))
            .timeout(Duration::from_secs(5));
        Ok(self.send(request).await?.json().await?)
    }

    /// Report on an attached launch: the game's PID, or why it didn't start.
    async fn attached(&self, id: u64, started: std::result::Result<u32, String>) -> Result<()> {
        let body = match started {
            Ok(pid) => serde_json::json!({ "pid": pid }),
            Err(error) => serde_json::json!({ "error": error }),
        };
        let request = self
            .client
            .post(format!("{}/launches/{id}/attached", self.base))
            .json(&body)
            .timeout(Duration::from_secs(5));
        self.send(request).await.map(|_| ())
    }

    /// False when it was too late: the download had finished.
    async fn cancel(&self, id: u64) -> Result<bool> {
        let response = self
            .client
            .delete(format!("{}/launches/{id}", self.base))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .context("talking to rommd")?;
        match response.status() {
            reqwest::StatusCode::NO_CONTENT => Ok(true),
            reqwest::StatusCode::CONFLICT => Ok(false),
            status => anyhow::bail!("rommd answered {status}"),
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let response = request.send().await.map_err(|e| {
            if e.is_connect() {
                anyhow::anyhow!(
                    "rommd is not running; start it with `rommd service start` (or run `rommd`)"
                )
            } else {
                anyhow::Error::new(e).context("talking to rommd")
            }
        })?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        match body["error"].as_str() {
            Some(error) => anyhow::bail!("{error}"),
            None => anyhow::bail!("rommd answered {status}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_with_spaces_are_quoted_again() {
        let words = ["Mario Odyssey", "platform:Nintendo Switch", "id:3"].map(String::from);
        assert_eq!(
            query_string(&words),
            r#""Mario Odyssey" "platform:Nintendo Switch" id:3"#
        );
        let parsed = crate::games::parse_query(&query_string(&words)).unwrap();
        assert_eq!(parsed.text, "Mario Odyssey");
        assert_eq!(parsed.platform.as_deref(), Some("Nintendo Switch"));
    }

    #[test]
    fn download_lines() {
        assert_eq!(
            download_line("Xenoblade", 5_200_000_000, Some(14_000_000_000)),
            "Downloading Xenoblade: 5.2 GB of 14.0 GB (37%)"
        );
        assert_eq!(
            download_line("Xenoblade", 42_000_000, None),
            "Downloading Xenoblade: 42 MB"
        );
    }

    fn game(id: i64, emulator: Option<&str>) -> GameMatch {
        GameMatch {
            rom_id: id,
            name: format!("Game {id}"),
            platform: "switch".into(),
            platform_name: "Nintendo Switch".into(),
            emulator: emulator.map(Into::into),
            emulator_reason: "no configured emulator runs switch games".into(),
        }
    }

    #[test]
    fn first_skips_games_without_an_emulator() {
        let found = GamesResponse {
            games: vec![game(1, None), game(2, Some("eden"))],
            total: 2,
        };
        match pick(&found, true).unwrap() {
            Pick::Game(g) => assert_eq!(g.rom_id, 2),
            Pick::Ask => panic!("--first must not ask"),
        }
        let single = GamesResponse {
            games: vec![game(5, Some("eden"))],
            total: 1,
        };
        assert!(matches!(pick(&single, false).unwrap(), Pick::Game(g) if g.rom_id == 5));
        let none = GamesResponse {
            games: vec![],
            total: 0,
        };
        assert!(pick(&none, true).is_err());
    }

    #[test]
    fn an_attached_launch_runs_only_the_configured_binary() {
        let config = Config::parse(
            "[emulators.ryujinx]\nbinary = '/apps/Ryujinx'\nconfig_dir = '/data'\n",
            std::path::Path::new("config.toml"),
        )
        .unwrap();
        let mut command = LaunchCommand::new(std::path::Path::new("/apps/Ryujinx"));
        command.arg("/roms/game.nsp");
        assert!(trusted(&config, "ryujinx", &command).is_ok());
        assert!(trusted(&config, "eden", &command).is_err());

        let other = LaunchCommand::new(std::path::Path::new("/tmp/evil"));
        assert!(trusted(&config, "ryujinx", &other).is_err());

        let mut preload = command.clone();
        preload
            .env
            .insert("LD_PRELOAD".into(), "/tmp/evil.so".into());
        assert!(trusted(&config, "ryujinx", &preload).is_err());
        command.env("XDG_CONFIG_HOME", "/tmp/rommd-mgba");
        assert!(trusted(&config, "ryujinx", &command).is_ok());
    }
}
