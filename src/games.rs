//! Finding games in the library and picking the emulator that runs them: the
//! query language behind `GET /games`, `rommd search` and `rommd play`.
//!
//! A query is words to search for in game names plus `key:value` filters
//! (`platform:switch`, `emulator:eden`, `library:romm`, `id:199`), quoted where
//! a value has spaces. A word with an unknown key (`Zelda:`) is just a word.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::Config;
use crate::emulator::Emulator;
use crate::romm::RommClient;
use crate::romm::types::{PlatformSchema, RomSummary};
use crate::server::types::{GameMatch, GamesResponse};

pub type Emulators = HashMap<String, Arc<dyn Emulator>>;

const KEYS: &[&str] = &["platform", "emulator", "library", "id"];

#[derive(Debug, Default, PartialEq)]
pub struct Query {
    pub text: String,
    pub platform: Option<String>,
    pub emulator: Option<String>,
    pub library: Option<String>,
    pub id: Option<i64>,
}

pub fn parse_query(q: &str) -> Result<Query> {
    let mut query = Query::default();
    let mut words = Vec::new();
    for token in tokens(q)? {
        let Some((key, value)) = token
            .split_once(':')
            .filter(|(k, _)| KEYS.contains(&k.to_lowercase().as_str()))
        else {
            words.push(token);
            continue;
        };
        let key = key.to_lowercase();
        anyhow::ensure!(!value.is_empty(), "`{key}:` needs a value");
        let duplicate = match key.as_str() {
            "platform" => query.platform.replace(value.to_owned()).is_some(),
            "emulator" => query.emulator.replace(value.to_lowercase()).is_some(),
            "library" => query.library.replace(value.to_lowercase()).is_some(),
            _ => {
                let id = value
                    .parse()
                    .with_context(|| format!("`id:{value}` is not a ROM ID"))?;
                query.id.replace(id).is_some()
            }
        };
        anyhow::ensure!(!duplicate, "`{key}:` is given twice");
    }
    query.text = words.join(" ");
    Ok(query)
}

/// Split on whitespace outside double quotes, dropping the quotes, so
/// `platform:"Nintendo Switch"` and `"platform:Nintendo Switch"` are one token.
fn tokens(q: &str) -> Result<Vec<String>> {
    let (mut out, mut current, mut quoted, mut started) = (Vec::new(), String::new(), false, false);
    for c in q.chars() {
        match c {
            '"' => (quoted, started) = (!quoted, true),
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    anyhow::ensure!(!quoted, "the query has an unclosed quote");
    if started {
        out.push(current);
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmulatorChoice {
    pub id: String,
    pub reason: String,
}

/// The configured emulator that runs `platform` (a RomM platform slug): the
/// one requested, else the `[platforms]` default, else the first in the
/// config file. A requested or default emulator that can't run the platform
/// is an error, not skipped, so a typo doesn't silently start another one.
pub fn choose_emulator(
    platform: &str,
    requested: Option<&str>,
    config: &Config,
    emulators: &Emulators,
) -> Result<EmulatorChoice> {
    let runs = |id: &str| {
        config.emulators.contains_key(id)
            && emulators
                .get(id)
                .is_some_and(|e| e.platforms().contains(&platform))
    };
    let choice = |id: &str, reason: String| EmulatorChoice {
        id: id.to_owned(),
        reason,
    };
    if let Some(id) = requested {
        anyhow::ensure!(
            config.emulators.contains_key(id),
            "emulator `{id}` is not configured"
        );
        anyhow::ensure!(runs(id), "{id} doesn't run {platform} games");
        return Ok(choice(id, "requested".into()));
    }
    if let Some(id) = config.platforms.get(platform) {
        anyhow::ensure!(
            runs(id),
            "[platforms] {platform} = \"{id}\" is not a configured emulator for {platform}"
        );
        return Ok(choice(
            id,
            format!("the [platforms] default for {platform}"),
        ));
    }
    let candidates: Vec<&str> = config
        .emulators
        .keys()
        .map(String::as_str)
        .filter(|id| runs(id))
        .collect();
    match candidates.as_slice() {
        [] => anyhow::bail!("no configured emulator runs {platform} games"),
        [only] => Ok(choice(
            only,
            format!("the only emulator configured for {platform}"),
        )),
        [first, others @ ..] => Ok(choice(
            first,
            format!(
                "first configured for {platform} (also {}); set [platforms] {platform} to pick another",
                others.join(", ")
            ),
        )),
    }
}

/// Platforms `value` names, by slug, folder name or name (case-insensitive).
pub fn matching_platforms<'a>(
    platforms: &'a [PlatformSchema],
    value: &str,
) -> Vec<&'a PlatformSchema> {
    let value = value.to_lowercase();
    platforms
        .iter()
        .filter(|p| {
            [
                Some(&p.slug),
                Some(&p.fs_slug),
                Some(&p.name),
                p.custom_name.as_ref(),
                p.display_name.as_ref(),
            ]
            .into_iter()
            .flatten()
            .any(|n| n.to_lowercase() == value)
        })
        .collect()
}

fn display_name(rom: &RomSummary) -> &str {
    rom.name
        .as_deref()
        .filter(|n| !n.is_empty())
        .unwrap_or(&rom.fs_name)
}

/// Exact name matches first, then names starting with `text`; otherwise
/// RomM's order (by name) is kept.
fn rank(roms: &mut [RomSummary], text: &str) {
    let text = text.to_lowercase();
    roms.sort_by_key(|rom| {
        let name = display_name(rom).to_lowercase();
        if text.is_empty() {
            2
        } else if name == text {
            0
        } else if name.starts_with(&text) {
            1
        } else {
            2
        }
    });
}

pub async fn find_games(
    romm: &RommClient,
    config: &Config,
    emulators: &Emulators,
    q: &str,
    limit: u32,
) -> Result<GamesResponse> {
    let query = parse_query(q)?;
    if let Some(library) = &query.library {
        anyhow::ensure!(
            library == "romm",
            "unknown library `{library}`: only `library:romm` is supported"
        );
    }
    if let Some(id) = &query.emulator {
        anyhow::ensure!(
            config.emulators.contains_key(id),
            "emulator `{id}` is not configured"
        );
    }

    let (mut roms, total) = match query.id {
        Some(id) => {
            let rom = romm.get_rom(id).await?;
            let summary = RomSummary {
                id: rom.id,
                name: rom.name,
                fs_name: rom.fs_name,
                platform_slug: rom.platform_slug,
                platform_display_name: rom.platform_display_name,
            };
            (vec![summary], 1)
        }
        None => {
            let platform_ids = match (&query.platform, &query.emulator) {
                (None, None) => Vec::new(),
                (platform, emulator) => {
                    let all = romm.platforms().await?;
                    let mut wanted: Vec<&PlatformSchema> = match platform {
                        Some(value) => {
                            let found = matching_platforms(&all, value);
                            anyhow::ensure!(
                                !found.is_empty(),
                                "no platform is called `{value}`; RomM has {}",
                                all.iter()
                                    .map(|p| p.slug.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
                            found
                        }
                        None => all.iter().collect(),
                    };
                    if let Some(id) = emulator {
                        let runs = emulators.get(id).map_or(&[][..], |e| e.platforms());
                        wanted.retain(|p| runs.contains(&p.slug.as_str()));
                        anyhow::ensure!(
                            !wanted.is_empty(),
                            "{id} doesn't run games of the platform asked for"
                        );
                    }
                    wanted.iter().map(|p| p.id).collect()
                }
            };
            anyhow::ensure!(
                !query.text.is_empty() || !platform_ids.is_empty(),
                "the query is empty: give part of a game's name or a filter"
            );
            let page = romm.search_roms(&query.text, &platform_ids, limit).await?;
            (page.items, page.total)
        }
    };
    rank(&mut roms, &query.text);

    let games = roms
        .into_iter()
        .map(|rom| {
            let (emulator, emulator_reason) = match choose_emulator(
                &rom.platform_slug,
                query.emulator.as_deref(),
                config,
                emulators,
            ) {
                Ok(c) => (Some(c.id), c.reason),
                Err(e) => (None, e.to_string()),
            };
            GameMatch {
                rom_id: rom.id,
                name: display_name(&rom).to_owned(),
                platform_name: rom
                    .platform_display_name
                    .clone()
                    .unwrap_or_else(|| rom.platform_slug.clone()),
                platform: rom.platform_slug,
                emulator,
                emulator_reason,
            }
        })
        .collect();
    Ok(GamesResponse { games, total })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::{dolphin::Dolphin, eden::Eden, mgba::MGba, ryujinx::Ryujinx};
    use crate::testing::fake_romm::FakeRomm;
    use std::path::Path;

    fn emulators() -> Emulators {
        let all: [Arc<dyn Emulator>; 4] = [
            Arc::new(Eden),
            Arc::new(Ryujinx),
            Arc::new(Dolphin),
            Arc::new(MGba),
        ];
        all.into_iter().map(|e| (e.id().to_owned(), e)).collect()
    }

    /// A config with these emulators, in this order, plus `extra` TOML.
    fn config(ids: &[&str], extra: &str) -> Config {
        let mut toml: String = ids
            .iter()
            .map(|id| {
                format!("[emulators.{id}]\nbinary = \"/bin/{id}\"\nconfig_dir = \"/tmp/{id}\"\n")
            })
            .collect();
        toml.push_str(extra);
        Config::parse(&toml, Path::new("config.toml")).unwrap()
    }

    #[test]
    fn parses_words_filters_and_quotes() {
        let q = parse_query(r#"Zelda: "Breath of" platform:switch EMULATOR:Eden id:42"#).unwrap();
        assert_eq!(
            q,
            Query {
                text: "Zelda: Breath of".into(),
                platform: Some("switch".into()),
                emulator: Some("eden".into()),
                library: None,
                id: Some(42),
            }
        );
        assert_eq!(
            parse_query(r#"platform:"Nintendo Switch" mario"#)
                .unwrap()
                .platform
                .as_deref(),
            Some("Nintendo Switch")
        );
        for bad in ["platform:", "id:x", "\"mario", "platform:a platform:b"] {
            assert!(parse_query(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn picks_requested_then_default_then_first_configured() {
        let emus = emulators();
        let both = config(&["ryujinx", "eden", "dolphin"], "");
        let first = choose_emulator("switch", None, &both, &emus).unwrap();
        assert_eq!(first.id, "ryujinx", "first in the file, not alphabetical");
        assert!(first.reason.contains("also eden"), "{}", first.reason);

        let requested = choose_emulator("switch", Some("eden"), &both, &emus).unwrap();
        assert_eq!(requested.id, "eden");

        let default = config(&["ryujinx", "eden"], "[platforms]\nswitch = \"eden\"\n");
        assert_eq!(
            choose_emulator("switch", None, &default, &emus).unwrap().id,
            "eden"
        );
        assert_eq!(
            choose_emulator("switch", Some("ryujinx"), &default, &emus)
                .unwrap()
                .id,
            "ryujinx",
            "a request beats the default"
        );

        let only = choose_emulator("ngc", None, &both, &emus).unwrap();
        assert_eq!(only.id, "dolphin");
        assert!(only.reason.contains("only"), "{}", only.reason);
    }

    #[test]
    fn refuses_an_emulator_that_cant_run_the_platform() {
        let emus = emulators();
        let cfg = config(&["ryujinx", "dolphin"], "[platforms]\nngc = \"ryujinx\"\n");
        assert!(choose_emulator("switch", Some("dolphin"), &cfg, &emus).is_err());
        assert!(
            choose_emulator("switch", Some("eden"), &cfg, &emus).is_err(),
            "not configured"
        );
        assert!(
            choose_emulator("ngc", None, &cfg, &emus).is_err(),
            "a wrong default is reported, not skipped"
        );
        assert!(choose_emulator("psx", None, &cfg, &emus).is_err());
    }

    async fn library() -> FakeRomm {
        let fake = FakeRomm::start().await;
        fake.seed_platform(22, "switch", "switch", "Nintendo Switch");
        fake.seed_platform(11, "ngc", "gc", "Nintendo GameCube");
        fake.seed_rom(5, "Super Mario Odyssey", 22);
        fake.seed_rom(6, "Mario Kart 8 Deluxe", 22);
        fake.seed_rom(7, "Mario", 22);
        fake.seed_rom(66, "Mario Kart: Double Dash!!", 11);
        fake.seed_rom(199, "Xenoblade Chronicles", 22);
        fake
    }

    #[tokio::test]
    async fn finds_games_by_name_and_platform() {
        let fake = library().await;
        let (romm, emus) = (fake.client(), emulators());
        let cfg = config(&["ryujinx", "eden", "dolphin"], "");

        let found = find_games(&romm, &cfg, &emus, "mario", 20).await.unwrap();
        let ids: Vec<i64> = found.games.iter().map(|g| g.rom_id).collect();
        assert_eq!(
            ids,
            vec![7, 6, 66, 5],
            "exact name, then names starting with it"
        );
        assert_eq!(found.total, 4);

        // The folder was renamed to `gc`; slug, folder and name all find it.
        for platform in ["ngc", "gc", "\"Nintendo GameCube\""] {
            let found = find_games(
                &romm,
                &cfg,
                &emus,
                &format!("mario platform:{platform}"),
                20,
            )
            .await
            .unwrap();
            assert_eq!(found.games.len(), 1, "platform:{platform}");
            assert_eq!(found.games[0].emulator.as_deref(), Some("dolphin"));
        }

        let switch = find_games(&romm, &cfg, &emus, "kart emulator:eden", 20)
            .await
            .unwrap();
        assert_eq!(switch.games.len(), 1, "eden runs only switch games");
        assert_eq!(switch.games[0].emulator.as_deref(), Some("eden"));

        let by_id = find_games(&romm, &cfg, &emus, "id:199", 20).await.unwrap();
        assert_eq!(by_id.games[0].name, "Xenoblade Chronicles");
        assert_eq!(by_id.games[0].emulator.as_deref(), Some("ryujinx"));
    }

    #[tokio::test]
    async fn reports_games_nothing_can_run_and_bad_filters() {
        let fake = library().await;
        let (romm, emus) = (fake.client(), emulators());
        let cfg = config(&["dolphin"], "");

        let found = find_games(&romm, &cfg, &emus, "odyssey", 20).await.unwrap();
        assert_eq!(found.games[0].emulator, None);
        assert!(found.games[0].emulator_reason.contains("switch"));

        for bad in [
            "mario platform:snes",
            "mario library:gaseous",
            "mario emulator:eden",
            "",
        ] {
            assert!(
                find_games(&romm, &cfg, &emus, bad, 20).await.is_err(),
                "{bad:?}"
            );
        }
    }
}
