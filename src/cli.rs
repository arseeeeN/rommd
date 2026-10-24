//! `rommd search` and `rommd play`: a terminal client of the running daemon,
//! which does the searching (`GET /games`) and launching (`POST /launch`), so
//! a game started here is tracked and synced like any other.

use anyhow::{Context, Result};
use std::io::{BufRead, IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{Config, ServerConfig};
use crate::emulator::Emulator;
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

pub async fn play(words: &[String], first: bool) -> Result<()> {
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
    let pid = launch_showing_progress(&daemon, game, emulator).await?;
    println!("Running (pid {pid}). Saves sync when the game exits.");
    Ok(())
}

/// Start the game and wait until it runs. When the ROM has to be downloaded
/// first, the daemon answers at once and the download is followed here;
/// Ctrl+C then cancels it. Otherwise the game is already starting, and Ctrl+C
/// only stops waiting.
async fn launch_showing_progress(daemon: &Daemon, game: &GameMatch, emulator: &str) -> Result<u64> {
    tokio::select! {
        launched = daemon.launch(game.rom_id, emulator) => match launched? {
            Launched::Started(pid) => Ok(pid),
            Launched::Downloading(id) => follow(daemon, id).await,
        },
        _ = tokio::signal::ctrl_c() => {
            eprintln!("Stopped waiting; rommd still starts the game.");
            std::process::exit(130);
        }
    }
}

async fn follow(daemon: &Daemon, id: u64) -> Result<u64> {
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    let mut progress = ProgressLine::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                progress.finish();
                match daemon.cancel(id).await {
                    Ok(true) => eprintln!("Download cancelled."),
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
                        return Ok(pid.into());
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
    if !cfg!(target_os = "macos") {
        println!("Emulator detection only covers macOS so far.");
        return Ok(());
    }
    let config = Config::read(&Config::path()).ok();
    let detection = match path {
        Some(path) => {
            anyhow::ensure!(path.exists(), "{} does not exist", path.display());
            crate::detect::detect_at(path, emulators)
        }
        None => crate::detect::detect(emulators),
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
        println!("{} ({state})", found.emulator);
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
        if !found.in_install_folder {
            println!(
                "  note: {} is outside Applications; move it there first, or the config \
                 breaks when it is moved or deleted",
                home_relative(&found.install)
            );
        }
    }
    for other in &detection.also {
        println!(
            "Also installed, not used: {} at {}",
            other.emulator,
            home_relative(&other.install)
        );
    }
    let undetectable: Vec<&str> = emulators
        .iter()
        .filter(|e| e.mac_app().is_none())
        .map(|e| e.id())
        .collect();
    if !undetectable.is_empty() && path.is_none() {
        println!("Not detected on macOS yet: {}", undetectable.join(", "));
    }
    if save {
        save_detected(&detection, config.as_ref(), path.is_some())?;
    } else if detection.found.iter().any(|f| {
        f.in_install_folder
            && config
                .as_ref()
                .is_none_or(|c| !c.emulators.contains_key(f.emulator))
    }) {
        println!("Add them to the config with: rommd detect --save");
    }
    Ok(())
}

/// Add what was found to the config. Entries already there are kept as they
/// are, and an app outside Applications is only added when the user named its
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
        } else if !found.in_install_folder && !named {
            println!(
                "Skipped {id}: move it into Applications, then run this again (or name it: \
                 rommd detect <path> --save)."
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

/// `~/…` for paths in the home folder.
fn home_relative(path: &std::path::Path) -> String {
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
    async fn launch(&self, rom_id: i64, emulator: &str) -> Result<Launched> {
        let request = self
            .client
            .post(format!("{}/launch", self.base))
            .json(&serde_json::json!({ "rom_id": rom_id, "emulator": emulator }));
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
}
