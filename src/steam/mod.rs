//! Steam shortcuts: one non-Steam game per RomM game, running `rommd play
//! --attach id:N` (in Game Mode Steam only shows and controls the processes
//! it starts), with RomM's cover as its portrait art and its platform as its
//! Steam collection.
//!
//! Which games: every game rommd has downloaded, every favorite in RomM, and
//! every game in the RomM collections `[steam] collections` names; the last
//! two whether downloaded or not, since the launch downloads them. rommd
//! only ever changes shortcuts it made (its own program, `play --attach
//! id:`). Steam rewrites `shortcuts.vdf` from memory when it exits, so this
//! runs only while Steam is closed, and Steam shows the result once started.

mod collections;
pub mod sgdb;
pub mod vdf;

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, stream};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use crate::config::Config;
use crate::games::{Emulators, choose_emulator};
use crate::romm::RommClient;
use crate::romm::types::RomDetail;
use sgdb::Kind;
use vdf::{Map, Value};

/// How many ROMs' details are fetched from RomM at once.
const ROMM_AT_ONCE: usize = 8;
/// How many games' artwork is fetched at once.
const ART_AT_ONCE: usize = 6;

/// What a shortcut of rommd's runs, before the ROM's ID.
const LAUNCH_PREFIX: &str = "play --attach id:";

/// A game that gets a shortcut.
#[derive(Debug, Clone, PartialEq)]
pub struct Game {
    pub rom_id: i64,
    pub name: String,
    pub platform: String,
    /// RomM's path of its cover.
    pub cover: Option<String>,
    /// RomM's path of its first screenshot.
    pub screenshot: Option<String>,
    /// Its SteamGridDB game, as RomM matched it.
    pub sgdb_id: Option<i64>,
}

/// The games `[steam]` asks for, by ROM ID, on platforms an emulator runs.
pub async fn games(config: &Config, romm: &RommClient, emulators: &Emulators) -> Result<Vec<Game>> {
    let steam = &config.steam;
    let runnable = |slug: &str| choose_emulator(slug, None, config, emulators).is_ok();
    let platforms: Vec<_> = romm
        .platforms()
        .await?
        .into_iter()
        .filter(|p| runnable(&p.slug))
        .collect();
    if platforms.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = platforms.iter().map(|p| p.id).collect();
    // The listing has all a shortcut needs but a multi-file ROM's files.
    let library: BTreeMap<i64, RomDetail> = romm
        .library(&ids)
        .await?
        .into_iter()
        .map(|rom| (rom.id, rom))
        .collect();
    let mut details: BTreeMap<i64, RomDetail> = BTreeMap::new();

    if steam.downloaded {
        let mut several_files = Vec::new();
        for rom in library.values() {
            let dir = config.platform_dir(&rom.platform_slug, &rom.platform_fs_slug);
            if rom.has_multiple_files && !rom.fs_name.ends_with(".zip") {
                // Only those with something where the download would be.
                let name = crate::romm::local_file_name(&rom.fs_name);
                let stem = Path::new(&name).file_stem().unwrap_or_default();
                if dir.join(stem).exists() {
                    several_files.push(rom.id);
                }
            } else if crate::romm::is_downloaded(rom, &dir) {
                details.insert(rom.id, rom.clone());
            }
        }
        for detail in fetch_roms(romm, several_files).await? {
            let dir = config.platform_dir(&detail.platform_slug, &detail.platform_fs_slug);
            if crate::romm::is_downloaded(&detail, &dir) {
                details.insert(detail.id, detail);
            }
        }
    }

    if steam.favorites || !steam.collections.is_empty() {
        let collections = romm.collections().await?;
        for wanted in &steam.collections {
            if !collections
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(wanted))
            {
                warn!("[steam] collections names {wanted:?}, which RomM has no collection called");
            }
        }
        let chosen = collections.iter().filter(|c| {
            (steam.favorites && c.is_favorite)
                || steam
                    .collections
                    .iter()
                    .any(|w| w.eq_ignore_ascii_case(&c.name))
        });
        for id in chosen.flat_map(|c| c.rom_ids.iter()) {
            // Not in the listing: on a platform no emulator runs.
            if let Some(rom) = library.get(id) {
                details.entry(*id).or_insert_with(|| rom.clone());
            } else {
                debug!("ROM {id} is on a platform no emulator runs");
            }
        }
    }

    let mut games: Vec<Game> = details
        .into_values()
        .map(|rom| Game {
            rom_id: rom.id,
            name: rom.name.clone().unwrap_or_else(|| {
                Path::new(&rom.fs_name)
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }),
            platform: rom
                .platform_display_name
                .clone()
                .unwrap_or_else(|| rom.platform_slug.clone()),
            cover: rom.path_cover_large.clone(),
            screenshot: rom.merged_screenshots.first().cloned(),
            sgdb_id: rom.sgdb_id,
        })
        .collect();
    games.sort_by(|a, b| a.name.cmp(&b.name).then(a.platform.cmp(&b.platform)));
    Ok(games)
}

/// `rommd steam sync` (or with `remove`, `rommd steam remove`).
pub async fn run(remove: bool, options: Options) -> Result<()> {
    let dry_run = options.dry_run;
    let (changes, account) = sync_from_config(remove, options).await?;

    let list = |names: &[String]| names.join(", ");
    let verb = if dry_run { "Would add" } else { "Added" };
    if !changes.added.is_empty() {
        println!("{verb} {}: {}", changes.added.len(), list(&changes.added));
    }
    if !changes.removed.is_empty() {
        let verb = if dry_run { "Would remove" } else { "Removed" };
        println!(
            "{verb} {}: {}",
            changes.removed.len(),
            list(&changes.removed)
        );
    }
    println!(
        "{} shortcut{} in {}",
        changes.added.len() + changes.kept.len(),
        if changes.added.len() + changes.kept.len() == 1 {
            ""
        } else {
            "s"
        },
        account.join("shortcuts.vdf").display()
    );
    if !dry_run && (!changes.added.is_empty() || !changes.removed.is_empty()) {
        println!("Start Steam to see them.");
    }
    Ok(())
}

/// A sync (or with `remove`, taking them all out) as the config says, for
/// this account. Returns what changed and the account's config folder.
pub async fn sync_from_config(remove: bool, options: Options) -> Result<(Changes, PathBuf)> {
    let config = Config::read(&Config::path()).context("reading the config")?;
    let romm_config = config
        .romm
        .as_ref()
        .context("the config has no [romm] section; run rommd setup")?;
    let romm = RommClient::new(romm_config);
    let emulators: Emulators = crate::emulator::all()
        .into_iter()
        .map(|e| (e.id().to_owned(), e))
        .collect();
    let account = account_config_dir()?;
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("finding rommd's own program")?;
    let games = if remove {
        Vec::new()
    } else {
        games(&config, &romm, &emulators).await?
    };
    let sgdb = config
        .steam
        .steamgriddb_api_key
        .as_deref()
        .filter(|k| !k.is_empty())
        .map(sgdb::Client::new);
    let changes = sync(&romm, sgdb.as_ref(), &account, &games, &exe, options).await?;
    Ok((changes, account))
}

/// The details of `ids`, a few at a time.
async fn fetch_roms(romm: &RommClient, ids: Vec<i64>) -> Result<Vec<RomDetail>> {
    stream::iter(ids)
        .map(|id| romm.get_rom(id))
        .buffer_unordered(ROMM_AT_ONCE)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect()
}

// ── Finding Steam ─────────────────────────────────────────────────────────────

/// The newest-logged-in account's config folder, `userdata/<id>/config`,
/// in the Steam install on this computer.
pub fn account_config_dir() -> Result<PathBuf> {
    let roots = steam_roots();
    let Some(root) = roots.first() else {
        bail!("no Steam install found");
    };
    if roots.len() > 1 {
        debug!("several Steam installs; using {}", root.display());
    }
    let userdata = root.join("userdata");
    let accounts: Vec<String> = std::fs::read_dir(&userdata)
        .with_context(|| format!("reading {}", userdata.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "0" && n.bytes().all(|b| b.is_ascii_digit()))
        .collect();
    let account = match accounts.as_slice() {
        [] => bail!(
            "Steam at {} has no account yet; log in once",
            root.display()
        ),
        [only] => only.clone(),
        _ => std::fs::read_to_string(root.join("config/loginusers.vdf"))
            .ok()
            .and_then(|text| newest_account(&text))
            .filter(|a| accounts.contains(a))
            .context("Steam has several accounts and doesn't say which logged in last")?,
    };
    Ok(userdata.join(account).join("config"))
}

/// Steam's own folders, those that hold accounts first. A Flatpak Steam is
/// left out: it can't run a program outside its sandbox.
fn steam_roots() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        if cfg!(target_os = "macos") {
            candidates.push(home.join("Library/Application Support/Steam"));
        } else if cfg!(unix) {
            candidates.push(home.join(".steam/steam"));
            candidates.push(home.join(".local/share/Steam"));
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::HKEY_CURRENT_USER;
        if let Some(path) =
            crate::detect::registry_string(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath")
        {
            candidates.push(PathBuf::from(path));
        }
        candidates.push(PathBuf::from(r"C:\Program Files (x86)\Steam"));
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    for c in candidates {
        let Ok(real) = c.canonicalize() else { continue };
        if real.join("userdata").is_dir() && !roots.contains(&real) {
            roots.push(real);
        }
    }
    roots
}

/// Steam's 32-bit account ID of the login `loginusers.vdf` (text VDF) marks
/// `MostRecent`, else the one with the newest `timestamp`.
fn newest_account(text: &str) -> Option<String> {
    const STEAM_ID_BASE: u64 = 76_561_197_960_265_728;
    let tokens = text_vdf_tokens(text);
    let mut users: Vec<(u64, bool, u64)> = Vec::new();
    let mut depth = 0;
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i].as_str() {
            "{" => depth += 1,
            "}" => depth -= 1,
            id if depth == 1 && tokens.get(i + 1).map(String::as_str) == Some("{") => {
                if let Ok(id) = id.parse::<u64>() {
                    users.push((id, false, 0));
                }
            }
            key if depth == 2 => {
                if let (Some(user), Some(value)) = (users.last_mut(), tokens.get(i + 1)) {
                    match key.to_ascii_lowercase().as_str() {
                        "mostrecent" => user.1 = value == "1",
                        "timestamp" => user.2 = value.parse().unwrap_or(0),
                        _ => {}
                    }
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    let (id, _, _) = users
        .into_iter()
        .max_by_key(|&(_, recent, time)| (recent, time))?;
    Some(id.checked_sub(STEAM_ID_BASE)?.to_string())
}

/// Quoted strings and braces.
fn text_vdf_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' | '}' => tokens.push(c.to_string()),
            '"' => {
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => s.extend(chars.next()),
                        c => s.push(c),
                    }
                }
                tokens.push(s);
            }
            _ => {}
        }
    }
    tokens
}

// ── Shortcuts ─────────────────────────────────────────────────────────────────

/// What a sync changes, by shortcut name.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    pub added: Vec<String>,
    pub kept: Vec<String>,
    pub removed: Vec<String>,
}

/// One shortcut to write: the game, its name in Steam and its app ID.
struct Planned<'a> {
    game: &'a Game,
    name: String,
    app_id: u32,
}

/// The app ID Steam ROM Manager gives a shortcut, which names its artwork.
fn app_id(exe: &str, name: &str) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(exe.as_bytes());
    crc.update(name.as_bytes());
    crc.sum() | 0x8000_0000
}

fn quoted(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

/// The ROM ID of a shortcut rommd made: its program is a `rommd` and it runs
/// `play --attach id:N`.
fn rommd_rom_id(shortcut: &Map) -> Option<i64> {
    let exe = shortcut.str("Exe")?.trim_matches('"');
    let is_rommd = Path::new(exe)
        .file_stem()
        .is_some_and(|s| s.eq_ignore_ascii_case("rommd"));
    let id = shortcut.str("LaunchOptions")?.strip_prefix(LAUNCH_PREFIX)?;
    is_rommd.then(|| id.trim().parse().ok()).flatten()
}

/// Names as Steam shows them: a name two games share gets its platform.
fn plan<'a>(games: &'a [Game], exe: &str) -> Vec<Planned<'a>> {
    let mut count: BTreeMap<&str, usize> = BTreeMap::new();
    for g in games {
        *count.entry(&g.name).or_default() += 1;
    }
    games
        .iter()
        .map(|game| {
            let name = if count[game.name.as_str()] > 1 {
                format!("{} ({})", game.name, game.platform)
            } else {
                game.name.clone()
            };
            Planned {
                game,
                app_id: app_id(exe, &name),
                name,
            }
        })
        .collect()
}

fn shortcut_entry(old: Option<&Map>, p: &Planned, exe: &Path) -> Map {
    // Steam's own keys of a shortcut it already had (play time, hidden, …) stay.
    let mut m = old.cloned().unwrap_or_else(|| {
        let mut m = Map::default();
        for (key, value) in [
            ("icon", Value::Str(String::new())),
            ("ShortcutPath", Value::Str(String::new())),
            ("IsHidden", Value::Int(0)),
            ("AllowDesktopConfig", Value::Int(1)),
            ("AllowOverlay", Value::Int(1)),
            ("OpenVR", Value::Int(0)),
            ("Devkit", Value::Int(0)),
            ("DevkitGameID", Value::Str(String::new())),
            ("DevkitOverrideAppID", Value::Int(0)),
            ("LastPlayTime", Value::Int(0)),
            ("FlatpakAppID", Value::Str(String::new())),
        ] {
            m.set(key, value);
        }
        m
    });
    m.set("appid", Value::Int(p.app_id as i32));
    m.set("AppName", Value::Str(p.name.clone()));
    m.set("Exe", Value::Str(quoted(exe)));
    m.set(
        "StartDir",
        Value::Str(quoted(exe.parent().unwrap_or(Path::new("")))),
    );
    m.set(
        "LaunchOptions",
        Value::Str(format!("{LAUNCH_PREFIX}{}", p.game.rom_id)),
    );
    m.set(
        "tags",
        Value::Map(Map(vec![("0".into(), Value::Str(p.game.platform.clone()))])),
    );
    m
}

/// `shortcuts.vdf` with rommd's shortcuts made `games`: the user's own
/// shortcuts first, as they were, then rommd's. Also the app IDs of rommd's
/// shortcuts it dropped, whose artwork goes.
fn rewrite(file: Option<&[u8]>, games: &[Game], exe: &Path) -> Result<(Map, Changes, Vec<u32>)> {
    let root = match file {
        Some(data) if !data.is_empty() => vdf::parse(data)?,
        _ => Map(vec![("shortcuts".into(), Value::Map(Map::default()))]),
    };
    let shortcuts = root
        .map("shortcuts")
        .context("shortcuts.vdf has no shortcuts")?;
    let mut theirs = Vec::new();
    let mut ours: BTreeMap<i64, Map> = BTreeMap::new();
    for (_, value) in &shortcuts.0 {
        let Value::Map(s) = value else { continue };
        match rommd_rom_id(s) {
            Some(id) => {
                ours.insert(id, s.clone());
            }
            None => theirs.push(s.clone()),
        }
    }
    let exe_str = quoted(exe);
    let planned = plan(games, &exe_str);
    let mut changes = Changes::default();
    let mut entries = theirs;
    let mut dropped = Vec::new();
    for p in &planned {
        let old = ours.remove(&p.game.rom_id);
        match &old {
            Some(old) => {
                changes.kept.push(p.name.clone());
                // A new name or program path gives a new app ID; the old
                // ID's art goes.
                if let Some(id) = old.int("appid").filter(|&id| id as u32 != p.app_id) {
                    dropped.push(id as u32);
                }
            }
            None => changes.added.push(p.name.clone()),
        }
        entries.push(shortcut_entry(old.as_ref(), p, exe));
    }
    for old in ours.values() {
        changes
            .removed
            .push(old.str("AppName").unwrap_or_default().to_owned());
        if let Some(id) = old.int("appid") {
            dropped.push(id as u32);
        }
    }
    // Steam numbers its shortcuts from 0.
    let numbered = entries
        .into_iter()
        .enumerate()
        .map(|(i, m)| (i.to_string(), Value::Map(m)))
        .collect();
    let mut root = root;
    root.set("shortcuts", Value::Map(Map(numbered)));
    Ok((root, changes, dropped))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Only report what would change.
    pub dry_run: bool,
    /// Fetch every shortcut's artwork again, not only what is missing.
    pub refresh_art: bool,
}

/// Every image of `kinds` for app `id` in `grid`.
fn remove_art(grid: &Path, id: u32, kinds: &[Kind]) {
    for kind in kinds {
        for ext in ["png", "jpg", "webp", "ico"] {
            let _ = std::fs::remove_file(grid.join(format!("{id}{}.{ext}", kind.suffix())));
        }
    }
}

fn art_file(grid: &Path, id: u32, kind: Kind) -> Option<PathBuf> {
    ["png", "jpg", "webp", "ico"]
        .iter()
        .map(|ext| grid.join(format!("{id}{}.{ext}", kind.suffix())))
        .find(|p| p.exists())
}

/// A shortcut's artwork, each kind it lacks (all of them with `refresh`):
/// SteamGridDB's when there is a key and it has one (for the game RomM
/// matched, else the title's first search hit), else RomM's cover for the
/// portrait and the first screenshot for the banner and hero.
async fn art(
    romm: &RommClient,
    sgdb: Option<&sgdb::Client>,
    grid: &Path,
    p: &Planned<'_>,
    refresh: bool,
) -> Result<()> {
    let missing: Vec<Kind> = Kind::ALL
        .into_iter()
        .filter(|&k| refresh || art_file(grid, p.app_id, k).is_none())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    // RomM's own match when its scan made one: it weighs all of the
    // game's names, where the title search takes its first hit.
    let game = match (sgdb, p.game.sgdb_id) {
        (Some(_), Some(id)) => Some(id),
        (Some(client), None) => client.game(&p.game.name).await?,
        (None, _) => None,
    };
    for kind in missing {
        let mut image = None;
        if let (Some(client), Some(id)) = (sgdb, game)
            && let Some(url) = client.artwork(id, kind).await?
        {
            image = Some(client.download(&url).await?);
        }
        let fallback = match kind {
            Kind::Tall => p.game.cover.as_deref(),
            Kind::Long | Kind::Hero => p.game.screenshot.as_deref(),
            Kind::Logo | Kind::Icon => None,
        };
        if image.is_none()
            && let Some(path) = fallback
        {
            image = Some(romm.asset(path).await?);
        }
        if let Some(image) = image {
            remove_art(grid, p.app_id, &[kind]);
            let ext = sgdb::extension(&image);
            std::fs::write(
                grid.join(format!("{}{}.{ext}", p.app_id, kind.suffix())),
                image,
            )?;
        }
    }
    Ok(())
}

/// Point each of rommd's shortcuts at its icon in `grid`, as Steam ROM
/// Manager does, else at its portrait: with no icon Steam shows the
/// program's, and rommd has none.
fn set_icons(root: &mut Map, grid: &Path) {
    let Some(Value::Map(shortcuts)) = root
        .0
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case("shortcuts"))
        .map(|(_, v)| v)
    else {
        return;
    };
    for (_, value) in shortcuts.0.iter_mut() {
        let Value::Map(s) = value else { continue };
        let (Some(_), Some(id)) = (rommd_rom_id(s), s.int("appid")) else {
            continue;
        };
        let icon =
            art_file(grid, id as u32, Kind::Icon).or_else(|| art_file(grid, id as u32, Kind::Tall));
        if let Some(icon) = icon {
            s.set("icon", Value::Str(icon.display().to_string()));
        }
    }
}

/// Make the shortcuts of `account` (an `userdata/<id>/config` folder) match
/// `games`, with their artwork.
pub async fn sync(
    romm: &RommClient,
    sgdb: Option<&sgdb::Client>,
    account: &Path,
    games: &[Game],
    exe: &Path,
    options: Options,
) -> Result<Changes> {
    let Options {
        dry_run,
        refresh_art,
    } = options;
    let path = account.join("shortcuts.vdf");
    let current = match std::fs::read(&path) {
        Ok(d) => Some(d),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let (mut root, changes, dropped) = rewrite(current.as_deref(), games, exe)?;
    if dry_run {
        return Ok(changes);
    }
    if crate::process::program_running(Path::new(""), &["steam", "steam_osx"]) {
        bail!("Steam is running; close it first, since it rewrites its shortcuts when it exits");
    }

    let grid = account.join("grid");
    std::fs::create_dir_all(&grid)?;
    let dropped_ids = dropped.clone();
    for id in dropped {
        remove_art(&grid, id, &Kind::ALL);
    }
    let planned = plan(games, &quoted(exe));
    let grid_dir = grid.as_path();
    let results: Vec<_> = stream::iter(&planned)
        .map(|p| async move { (p, art(romm, sgdb, grid_dir, p, refresh_art).await) })
        .buffer_unordered(ART_AT_ONCE)
        .collect()
        .await;
    for (p, result) in results {
        if let Err(e) = result {
            warn!("artwork for {}: {e:#}", p.name);
        }
    }
    set_icons(&mut root, &grid);

    let mut by_platform: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for p in &planned {
        by_platform
            .entry(p.game.platform.clone())
            .or_default()
            .push(p.app_id);
    }
    let ours: BTreeSet<u32> = planned
        .iter()
        .map(|p| p.app_id)
        .chain(dropped_ids)
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    if let Err(e) = collections::update(account, &by_platform, &ours, now) {
        warn!("Steam collections: {e:#}");
    }

    if let Some(old) = &current {
        std::fs::write(account.join("shortcuts.vdf.rommd-backup"), old)?;
    }
    let staged = account.join(".shortcuts.vdf.rommd-tmp");
    std::fs::write(&staged, vdf::write(&root))?;
    std::fs::rename(&staged, &path)?;
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(id: i64, name: &str, platform: &str) -> Game {
        Game {
            rom_id: id,
            name: name.into(),
            platform: platform.into(),
            cover: None,
            screenshot: None,
            sgdb_id: None,
        }
    }

    fn shortcuts(root: &Map) -> Vec<&Map> {
        root.map("shortcuts")
            .unwrap()
            .0
            .iter()
            .map(|(_, v)| match v {
                Value::Map(m) => m,
                _ => panic!(),
            })
            .collect()
    }

    #[test]
    fn app_ids_follow_steam_rom_manager() {
        // crc32("\"/usr/bin/game\"Some Game") with the high bit set.
        let id = app_id("\"/usr/bin/game\"", "Some Game");
        assert!(id & 0x8000_0000 != 0);
        assert_eq!(id, app_id("\"/usr/bin/game\"", "Some Game"));
        assert_ne!(id, app_id("\"/usr/bin/game\"", "Other Game"));
    }

    #[test]
    fn adds_keeps_and_removes_only_rommds_shortcuts() {
        let exe = Path::new("/home/u/.local/bin/rommd");
        let mut theirs = Map::default();
        theirs.set("AppName", Value::Str("Their Game".into()));
        theirs.set("Exe", Value::Str("\"/usr/bin/their\"".into()));
        theirs.set("LaunchOptions", Value::Str(String::new()));
        let mut old_ours = Map::default();
        old_ours.set("appid", Value::Int(-5));
        old_ours.set("AppName", Value::Str("Gone".into()));
        old_ours.set("Exe", Value::Str("\"/old/path/rommd\"".into()));
        old_ours.set("LaunchOptions", Value::Str("play --attach id:9".into()));
        let mut played = Map::default();
        played.set("AppName", Value::Str("Mario Kart 64".into()));
        played.set("Exe", Value::Str("\"/old/path/rommd\"".into()));
        played.set("LaunchOptions", Value::Str("play --attach id:30".into()));
        played.set("LastPlayTime", Value::Int(1_700_000_000));
        let file = vdf::write(&Map(vec![(
            "shortcuts".into(),
            Value::Map(Map(vec![
                ("0".into(), Value::Map(theirs.clone())),
                ("1".into(), Value::Map(old_ours)),
                ("2".into(), Value::Map(played)),
            ])),
        )]));

        let games = [
            game(30, "Mario Kart 64", "Nintendo 64"),
            game(35, "Super Mario 64", "Nintendo 64"),
        ];
        let (root, changes, dropped) = rewrite(Some(&file), &games, exe).unwrap();
        assert_eq!(changes.added, ["Super Mario 64"]);
        assert_eq!(changes.kept, ["Mario Kart 64"]);
        assert_eq!(changes.removed, ["Gone"]);
        assert_eq!(
            dropped,
            [(-5i32) as u32],
            "Mario Kart had no app ID to drop"
        );

        let list = shortcuts(&root);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0], &theirs, "the user's own shortcut untouched, first");
        let kart = list[1];
        assert_eq!(kart.str("Exe"), Some("\"/home/u/.local/bin/rommd\""));
        assert_eq!(kart.str("LaunchOptions"), Some("play --attach id:30"));
        assert_eq!(
            kart.int("LastPlayTime"),
            Some(1_700_000_000),
            "Steam's keys kept"
        );
        assert_eq!(kart.map("tags").unwrap().str("0"), Some("Nintendo 64"));
        assert_eq!(
            kart.int("appid").map(|i| i as u32),
            Some(app_id("\"/home/u/.local/bin/rommd\"", "Mario Kart 64"))
        );
        assert_eq!(list[2].str("AppName"), Some("Super Mario 64"));
        let keys: Vec<_> = root
            .map("shortcuts")
            .unwrap()
            .0
            .iter()
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(keys, ["0", "1", "2"]);
    }

    #[test]
    fn a_name_two_games_share_gets_the_platform() {
        let games = [
            game(1, "Tetris", "Game Boy"),
            game(2, "Tetris", "NES"),
            game(3, "Doom", "N64"),
        ];
        let names: Vec<_> = plan(&games, "x").into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["Tetris (Game Boy)", "Tetris (NES)", "Doom"]);
    }

    #[test]
    fn an_empty_or_missing_file_starts_the_list() {
        let exe = Path::new("/bin/rommd");
        let empty = b"\x00shortcuts\x00\x08\x08";
        let (root, changes, _) = rewrite(Some(empty), &[game(1, "Doom", "N64")], exe).unwrap();
        assert_eq!(changes.added, ["Doom"]);
        assert_eq!(shortcuts(&root).len(), 1);
        let (root, _, _) = rewrite(None, &[], exe).unwrap();
        assert_eq!(vdf::write(&root), empty);
    }

    #[test]
    fn the_newest_login_is_the_account() {
        let text = r#""users"
{
	"76561198132825895"
	{
		"AccountName"		"a"
		"timestamp"		"1793781190"
	}
	"76561198255100756"
	{
		"AccountName"		"b"
		"timestamp"		"1793527746"
	}
}"#;
        assert_eq!(newest_account(text).as_deref(), Some("172560167"));
        let recent = text.replace(
            "\"AccountName\"\t\t\"b\"",
            "\"AccountName\"\t\t\"b\"\n\t\t\"MostRecent\"\t\t\"1\"",
        );
        assert_eq!(newest_account(&recent).as_deref(), Some("294835028"));
    }
}
