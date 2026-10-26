//! `rommd setup`: the questions a new user would otherwise answer by editing
//! the config file. The install scripts run it, so a fresh install ends with
//! rommd connected to RomM and knowing the emulators here.
//!
//! Every answer is checked before anything is written (the RomM address must
//! be a RomM server, the token one it accepts), and the file is written once
//! at the end, through `config_edit`, so an existing config keeps its
//! comments and nothing is written when setup is left halfway. The token is
//! read without echo and never printed.

use anyhow::{Context, Result};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::config::{Config, Os, RommConfig};
use crate::config_edit::{self, Settings};
use crate::detect::Found;
use crate::romm::RommClient;
use crate::update::UpdateMode;

/// Answers given in the environment instead, for unattended installs.
pub const URL_ENV: &str = "ROMMD_URL";
pub const TOKEN_ENV: &str = "ROMMD_TOKEN";

pub struct Options {
    /// Ask nothing: take the RomM address and token from the environment
    /// (or keep the config's), add the emulators found in the install
    /// folders, and keep defaults for everything else.
    pub yes: bool,
    /// Do nothing when the config already has RomM settings (the install
    /// scripts on an upgrade).
    pub if_needed: bool,
}

pub async fn run(options: Options) -> Result<()> {
    let path = Config::path();
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => {
            let config = Config::parse(&text, &path).with_context(|| {
                format!(
                    "{} has an error; fix it (or delete it to start over) and run \
                     `rommd setup` again",
                    path.display()
                )
            })?;
            Some((text, config))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if options.if_needed && existing.as_ref().is_some_and(|(_, c)| c.romm.is_some()) {
        return Ok(());
    }

    let found = if crate::detect::supported() {
        // Run by the user, so it may look further than the daemon's first
        // start does; what is outside the install folders is only listed.
        crate::detect::detect(&crate::emulator::all(), true).found
    } else {
        Vec::new()
    };
    let env = |name| {
        std::env::var(name)
            .ok()
            .filter(|v: &String| !v.trim().is_empty())
    };
    let given = Given {
        url: env(URL_ENV),
        token: env(TOKEN_ENV),
    };
    let stdin = std::io::stdin();
    let mut console = Console {
        input: Box::new(stdin.lock()),
        out: Box::new(std::io::stdout()),
        ask: !options.yes,
        hidden: true,
    };
    let config = existing.as_ref().map(|(_, c)| c);
    let answers = ask(&mut console, &path, config, &given, &found).await?;

    let text = match &existing {
        Some((text, _)) => text.clone(),
        None => {
            let installed: Vec<Found> = answers.emulators.clone();
            crate::config::default_config(Os::current(), &installed)
        }
    };
    let mut text = config_edit::with_settings(&text, &path, &answers.settings)?;
    if existing.is_some() {
        for f in &answers.emulators {
            text = config_edit::with_emulator(&text, &path, f.emulator, &f.binary, &f.config_dir)?;
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    config_edit::write_replacing(&path, &text)?;
    console.say(&format!("\nSaved to {}.", path.display()))?;
    if existing.is_some() {
        console.say("rommd picks up the change by itself if it is running.")?;
    }
    Ok(())
}

struct Given {
    url: Option<String>,
    token: Option<String>,
}

#[derive(Debug, Default)]
struct Answers {
    settings: Settings,
    /// Emulators to add to the config.
    emulators: Vec<Found>,
}

/// Prompts on a terminal, or (`ask` false) the defaults without asking.
struct Console<'a> {
    input: Box<dyn BufRead + 'a>,
    out: Box<dyn Write + 'a>,
    ask: bool,
    /// Read secrets without echo, from the terminal itself.
    hidden: bool,
}

impl Console<'_> {
    fn say(&mut self, text: &str) -> Result<()> {
        writeln!(self.out, "{text}")?;
        Ok(())
    }

    fn line(&mut self, prompt: &str) -> Result<String> {
        write!(self.out, "{prompt}")?;
        self.out.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            anyhow::bail!("setup cancelled; nothing was saved");
        }
        Ok(line.trim().to_owned())
    }

    fn secret(&mut self, prompt: &str) -> Result<String> {
        if !self.hidden {
            return self.line(prompt);
        }
        write!(self.out, "{prompt}")?;
        self.out.flush()?;
        let secret = rpassword::read_password().context("reading the token")?;
        Ok(secret.trim().to_owned())
    }

    /// `default` when the answer is empty, or when not asking.
    fn yes_no(&mut self, prompt: &str, default: bool) -> Result<bool> {
        if !self.ask {
            return Ok(default);
        }
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        loop {
            match self
                .line(&format!("{prompt} {hint} "))?
                .to_lowercase()
                .as_str()
            {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => self.say("Answer y or n.")?,
            }
        }
    }
}

async fn ask(
    console: &mut Console<'_>,
    path: &Path,
    config: Option<&Config>,
    given: &Given,
    found: &[Found],
) -> Result<Answers> {
    let mut answers = Answers::default();
    console.say(&format!(
        "rommd setup. Settings are saved to {}.",
        path.display()
    ))?;
    console.say("Nothing is saved until the end; Ctrl+C leaves without changes.\n")?;

    answers.settings.romm = romm(console, config.and_then(|c| c.romm.as_ref()), given).await?;
    answers.emulators = emulators(console, config, found)?;

    if console.ask && console.yes_no("\nChange more settings (ROM folder, updates)?", false)? {
        answers.settings.roms_dir = roms_dir(console, config)?;
        answers.settings.update_mode = update_mode(console, config)?;
    }
    Ok(answers)
}

/// The RomM address and token, both checked against the server. `None`
/// keeps the config's.
async fn romm(
    console: &mut Console<'_>,
    current: Option<&RommConfig>,
    given: &Given,
) -> Result<Option<(String, String)>> {
    if let Some(current) = current
        && given.url.is_none()
    {
        console.say(&format!("RomM: {}", current.base_url))?;
        match RommClient::new(current).check().await {
            Ok(version) => {
                console.say(&format!("  connected (RomM {version})."))?;
                if !console.yes_no("Change the RomM server or token?", false)? {
                    return Ok(None);
                }
            }
            Err(e) => {
                console.say(&format!("  not working: {e:#}"))?;
                anyhow::ensure!(
                    console.ask,
                    "the config's RomM settings don't work; run `rommd setup` to change them"
                );
            }
        }
    }

    let (base_url, version) = loop {
        let typed = match &given.url {
            Some(url) => url.clone(),
            None if console.ask => console
                .line("RomM address (as in your browser, e.g. https://romm.example.com): ")?,
            None => anyhow::bail!("no RomM address; set {URL_ENV} or run `rommd setup`"),
        };
        if typed.is_empty() {
            continue;
        }
        let url = normalize_url(&typed);
        console.say(&format!("Checking {url} ..."))?;
        match client(&url, "").server_version().await {
            Ok(version) => break (url, version),
            Err(e) if console.ask && given.url.is_none() => {
                console.say(&format!("  {e:#}"))?;
            }
            Err(e) => return Err(e),
        }
    };
    console.say(&format!("  RomM {version}."))?;

    let token = loop {
        let token = match &given.token {
            Some(token) => token.clone(),
            None if console.ask => {
                console.say(
                    "API token: create one in RomM under Settings > API Tokens, then paste it \
                     here.",
                )?;
                console.secret("Token (hidden while you type or paste): ")?
            }
            None => anyhow::bail!("no RomM API token; set {TOKEN_ENV} or run `rommd setup`"),
        };
        if token.is_empty() {
            continue;
        }
        match client(&base_url, &token).check_login(&version).await {
            Ok(()) => break token,
            Err(_) if console.ask && given.token.is_none() => {
                console.say("  RomM didn't accept that token; check it and try again.")?;
            }
            Err(e) => return Err(e),
        }
    };
    console.say("  Token accepted.")?;
    Ok(Some((base_url, token)))
}

fn client(base_url: &str, token: &str) -> RommClient {
    RommClient::new(&RommConfig {
        base_url: base_url.to_owned(),
        api_token: Some(token.to_owned()),
        username: None,
        password: None,
    })
}

/// What people type for an address: no scheme means https, and a page's
/// address (copied from the browser) means its server.
fn normalize_url(typed: &str) -> String {
    let typed = typed.trim();
    let with_scheme = if typed.contains("://") {
        typed.to_owned()
    } else {
        format!("https://{typed}")
    };
    match reqwest::Url::parse(&with_scheme) {
        Ok(url) if url.path().contains("/rom/") || url.path().contains("/platform/") => {
            url.origin().ascii_serialization()
        }
        _ => with_scheme.trim_end_matches('/').to_owned(),
    }
}

/// The emulators found here that the config doesn't have yet.
fn emulators(
    console: &mut Console<'_>,
    config: Option<&Config>,
    found: &[Found],
) -> Result<Vec<Found>> {
    console.say("")?;
    if !crate::detect::supported() {
        console.say(
            "rommd can't find emulators by itself on this system yet; add them to the \
             config file (it has examples).",
        )?;
        return Ok(Vec::new());
    }
    let configured = |f: &&Found| config.is_some_and(|c| c.emulators.contains_key(f.emulator));
    let (installed, elsewhere): (Vec<&Found>, Vec<&Found>) = found
        .iter()
        .filter(|f| !configured(f))
        .partition(|f| f.installed);
    if let Some(config) = config.filter(|c| !c.emulators.is_empty()) {
        let ids: Vec<&str> = config.emulators.keys().map(String::as_str).collect();
        console.say(&format!("Emulators in your config: {}", ids.join(", ")))?;
    }
    for f in &elsewhere {
        console.say(&format!(
            "Found {} at {}, outside the usual install folders; move it there and run \
             `rommd setup` again to add it (or add it with `rommd detect <path> --save`).",
            f.name,
            f.install.display()
        ))?;
    }
    if installed.is_empty() {
        if config.is_none_or(|c| c.emulators.is_empty()) {
            console
                .say("No supported emulator found. Install one, then run `rommd detect --save`.")?;
        }
        return Ok(Vec::new());
    }
    console.say("Emulators found on this computer:")?;
    for f in &installed {
        console.say(&format!("  {:<12} {}", f.emulator, f.binary.display()))?;
    }
    for (platform, ids) in
        crate::detect::shared_platforms(&installed.iter().map(|f| (*f).clone()).collect::<Vec<_>>())
    {
        console.say(&format!(
            "  ({} all run {platform}; {} is used. [platforms] in the config can pick another.)",
            ids.join(", "),
            ids[0]
        ))?;
    }
    if console.yes_no("Add them?", true)? {
        Ok(installed.into_iter().cloned().collect())
    } else {
        Ok(Vec::new())
    }
}

fn roms_dir(console: &mut Console<'_>, config: Option<&Config>) -> Result<Option<PathBuf>> {
    let current = config.map_or_else(crate::config::default_roms_dir, Config::roms_dir);
    console.say("Where downloaded games go (one folder per platform inside it).")?;
    let typed = console.line(&format!("ROM folder [{}]: ", current.display()))?;
    if typed.is_empty() {
        return Ok(None);
    }
    let dir = crate::config::expand_path(PathBuf::from(typed));
    if dir == current {
        return Ok(None);
    }
    anyhow::ensure!(
        dir.is_absolute(),
        "{} is not a full path (like {})",
        dir.display(),
        current.display()
    );
    Ok(Some(dir))
}

fn update_mode(console: &mut Console<'_>, config: Option<&Config>) -> Result<Option<UpdateMode>> {
    let current = config.map(|c| c.updates.mode).unwrap_or_default();
    console.say(
        "Updates: auto (installed by itself while no game runs), notify (only reported), off.",
    )?;
    loop {
        let typed = console.line(&format!("Updates [{}]: ", config_edit::mode_name(current)))?;
        let mode = match typed.to_lowercase().as_str() {
            "" => return Ok(None),
            "auto" => UpdateMode::Auto,
            "notify" => UpdateMode::Notify,
            "off" => UpdateMode::Off,
            _ => {
                console.say("Type auto, notify or off.")?;
                continue;
            }
        };
        return Ok((mode != current).then_some(mode));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fake_romm::FakeRomm;

    fn console<'a>(input: &'a str, out: &'a mut Vec<u8>) -> Console<'a> {
        Console {
            input: Box::new(input.as_bytes()),
            out: Box::new(out),
            ask: true,
            hidden: false,
        }
    }

    const NO_ONE: Given = Given {
        url: None,
        token: None,
    };

    #[test]
    fn addresses_as_people_type_them() {
        assert_eq!(
            normalize_url("romm.example.com"),
            "https://romm.example.com"
        );
        assert_eq!(normalize_url(" http://nas:8080/ "), "http://nas:8080");
        assert_eq!(
            normalize_url("https://romm.example.com/rom/199"),
            "https://romm.example.com"
        );
        assert_eq!(
            normalize_url("https://example.com/romm"),
            "https://example.com/romm",
            "RomM under a path stays there"
        );
    }

    #[tokio::test]
    async fn asks_again_until_the_address_and_token_work() {
        let fake = FakeRomm::start().await;
        let dead = "http://127.0.0.1:9";
        let input = format!("{dead}\n{}\nwrong\ntest\n", fake.base_url());
        let mut out = Vec::new();
        let romm = romm(&mut console(&input, &mut out), None, &NO_ONE)
            .await
            .unwrap();
        assert_eq!(romm, Some((fake.base_url().to_owned(), "test".to_owned())));
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("cannot reach"), "{out}");
        assert!(out.contains("didn't accept that token"), "{out}");
        assert!(
            !out.contains("wrong\n") && !out.contains(": test"),
            "the token is never shown: {out}"
        );
    }

    #[tokio::test]
    async fn a_working_config_is_kept_unless_asked_to_change() {
        let fake = FakeRomm::start().await;
        let current = RommConfig {
            base_url: fake.base_url().to_owned(),
            api_token: Some("test".into()),
            username: None,
            password: None,
        };
        let mut out = Vec::new();
        let kept = romm(&mut console("\n", &mut out), Some(&current), &NO_ONE)
            .await
            .unwrap();
        assert_eq!(kept, None);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("connected (RomM 5.2.0)")
        );
    }

    #[tokio::test]
    async fn unattended_setup_takes_the_environment_and_fails_on_a_bad_token() {
        let fake = FakeRomm::start().await;
        let mut out = Vec::new();
        let mut quiet = console("", &mut out);
        quiet.ask = false;
        let given = Given {
            url: Some(fake.base_url().to_owned()),
            token: Some("test".into()),
        };
        let romm = super::romm(&mut quiet, None, &given).await.unwrap();
        assert_eq!(romm.unwrap().1, "test");

        let bad = Given {
            url: Some(fake.base_url().to_owned()),
            token: Some("nope".into()),
        };
        assert!(super::romm(&mut quiet, None, &bad).await.is_err());
        assert!(super::romm(&mut quiet, None, &NO_ONE).await.is_err());
    }

    #[test]
    fn leaving_halfway_is_a_cancel() {
        let mut out = Vec::new();
        let err = console("", &mut out).line("? ").unwrap_err();
        assert!(err.to_string().contains("nothing was saved"));
    }

    #[test]
    fn more_settings_are_only_changed_when_they_differ() {
        let games = if cfg!(windows) { r"D:\Games" } else { "/games" };
        let config = Config::parse(
            &format!("roms_dir = '{games}'\n[updates]\nmode = 'notify'\n"),
            Path::new("c.toml"),
        )
        .unwrap();
        let mut out = Vec::new();
        let input = format!("\n{games}\n");
        let mut c = console(&input, &mut out);
        assert_eq!(roms_dir(&mut c, Some(&config)).unwrap(), None);
        assert_eq!(roms_dir(&mut c, Some(&config)).unwrap(), None);
        drop(c);
        #[cfg(unix)]
        {
            let mut out = Vec::new();
            let mut c = console("/other\nrelative\n", &mut out);
            let other = roms_dir(&mut c, Some(&config)).unwrap();
            assert_eq!(other, Some(PathBuf::from("/other")));
            assert!(roms_dir(&mut c, Some(&config)).is_err());
        }

        let mut out = Vec::new();
        let mut c = console("\nnotify\nloud\noff\n", &mut out);
        assert_eq!(update_mode(&mut c, Some(&config)).unwrap(), None);
        assert_eq!(update_mode(&mut c, Some(&config)).unwrap(), None);
        assert_eq!(
            update_mode(&mut c, Some(&config)).unwrap(),
            Some(UpdateMode::Off),
            "asked again after a typo"
        );
    }
}
