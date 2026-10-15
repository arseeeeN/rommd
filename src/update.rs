//! Self-update from GitHub Releases.
//!
//! A release carries one raw binary per target (`rommd-<target>[.exe]`) and a
//! minisign signature for each (`<asset>.minisig`). The updater only installs a
//! binary whose signature verifies against the public key compiled into this
//! build (`minisign.pub`, see build.rs) and whose signed trusted comment reads
//! `rommd <version> <asset>`: that binds the signature to one version and one
//! target, so an old signed release can't be served as a new one and one
//! platform's binary can't stand in for another's. Builds without the key can
//! check for updates but never install them.
//!
//! Installing swaps the binary on disk, then the daemon restarts: under a
//! service manager (`ROMMD_SUPERVISED` set by `rommd service install`) it just
//! exits and is started again; otherwise it starts a new copy of itself first.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::process::ProcessManager;

pub const DEFAULT_REPOSITORY: &str = "arseeeeN/rommd";
const GITHUB_API: &str = "https://api.github.com";
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);
const GAME_POLL: Duration = Duration::from_secs(300);
/// Set by the service definitions `rommd service install` writes.
pub const SUPERVISED_ENV: &str = "ROMMD_SUPERVISED";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum UpdateMode {
    /// Check daily and install new releases while no game is running.
    #[default]
    Auto,
    /// Check daily and report new releases; installing is up to the user.
    Notify,
    /// No background checks.
    Off,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct UpdateConfig {
    #[serde(default)]
    pub mode: UpdateMode,
    /// GitHub `owner/name` to take releases from.
    #[serde(default)]
    pub repository: Option<String>,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct UpdateStatus {
    pub current_version: String,
    /// Newest release found by the last check.
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub release_url: Option<String>,
    pub mode: UpdateMode,
    /// RFC 3339 time of the last successful check.
    pub last_checked: Option<String>,
    pub last_error: Option<String>,
    /// Why this build cannot install updates, when it cannot.
    pub install_blocker: Option<String>,
    pub installing: bool,
}

#[derive(Debug, Clone)]
struct Release {
    version: semver::Version,
    url: String,
    asset: Option<(String, String)>,
    signature_url: Option<String>,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

pub struct Updater {
    /// Mode and repository follow the config file (`reconfigure`).
    mode: std::sync::Mutex<UpdateMode>,
    api_base: String,
    repository: std::sync::Mutex<String>,
    /// Wakes the background loop after `reconfigure`.
    wake: tokio::sync::Notify,
    current: semver::Version,
    target: String,
    public_key: Option<String>,
    client: reqwest::Client,
    status: Mutex<UpdateStatus>,
    latest: Mutex<Option<Release>>,
}

impl Updater {
    pub fn new(config: &UpdateConfig) -> Self {
        Self::with(
            config.mode,
            GITHUB_API.into(),
            config
                .repository
                .clone()
                .unwrap_or_else(|| DEFAULT_REPOSITORY.into()),
            env!("CARGO_PKG_VERSION")
                .parse()
                .expect("crate version is semver"),
            env!("ROMMD_TARGET").into(),
            option_env!("ROMMD_UPDATE_PUBKEY").map(Into::into),
        )
    }

    fn with(
        mode: UpdateMode,
        api_base: String,
        repository: String,
        current: semver::Version,
        target: String,
        public_key: Option<String>,
    ) -> Self {
        let install_blocker = public_key
            .is_none()
            .then(|| "this build has no release signing key (a development build); install a release to get updates".to_owned());
        let status = UpdateStatus {
            current_version: current.to_string(),
            latest_version: None,
            update_available: false,
            release_url: None,
            mode,
            last_checked: None,
            last_error: None,
            install_blocker,
            installing: false,
        };
        Self {
            mode: std::sync::Mutex::new(mode),
            api_base,
            repository: std::sync::Mutex::new(repository),
            wake: tokio::sync::Notify::new(),
            current,
            target,
            public_key,
            client: reqwest::Client::builder()
                .user_agent(concat!("rommd/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("building HTTP client"),
            status: Mutex::new(status),
            latest: Mutex::new(None),
        }
    }

    pub fn mode(&self) -> UpdateMode {
        *self.mode.lock().unwrap()
    }

    /// Apply a changed `[updates]` config section. A changed mode or source
    /// takes effect right away instead of at the next daily check.
    pub async fn reconfigure(&self, config: &UpdateConfig) {
        let repository = config
            .repository
            .clone()
            .unwrap_or_else(|| DEFAULT_REPOSITORY.into());
        let changed = {
            let mut mode = self.mode.lock().unwrap();
            let mut repo = self.repository.lock().unwrap();
            let changed = *mode != config.mode || *repo != repository;
            *mode = config.mode;
            *repo = repository;
            changed
        };
        if changed {
            self.status.lock().await.mode = config.mode;
            *self.latest.lock().await = None;
            self.wake.notify_one();
        }
    }

    pub async fn status(&self) -> UpdateStatus {
        self.status.lock().await.clone()
    }

    /// Ask GitHub for the latest release and record the result.
    pub async fn check(&self) -> UpdateStatus {
        let result = self.fetch_latest().await;
        let mut status = self.status.lock().await;
        match result {
            Ok(release) => {
                let newer = release.version > self.current;
                status.latest_version = Some(release.version.to_string());
                status.release_url = Some(release.url.clone());
                status.update_available = newer;
                status.last_checked = Some(chrono::Utc::now().to_rfc3339());
                status.last_error = None;
                if self.public_key.is_some() {
                    status.install_blocker = (newer && release.asset.is_none()).then(|| {
                        format!(
                            "release {} has no build for {}",
                            release.version, self.target
                        )
                    });
                }
                *self.latest.lock().await = Some(release);
            }
            Err(e) => status.last_error = Some(format!("{:#}", e)),
        }
        status.clone()
    }

    async fn fetch_latest(&self) -> Result<Release> {
        let repository = self.repository.lock().unwrap().clone();
        let url = format!("{}/repos/{}/releases/latest", self.api_base, repository);
        let gh: GhRelease = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .with_context(|| format!("GET {}", url))?
            .error_for_status()
            .context("fetching the latest release")?
            .json()
            .await
            .context("parsing the release")?;
        anyhow::ensure!(
            !gh.draft && !gh.prerelease,
            "latest release is a draft or prerelease"
        );
        let version = semver::Version::parse(gh.tag_name.trim_start_matches('v'))
            .with_context(|| format!("release tag {:?} is not a version", gh.tag_name))?;
        let name = asset_name(&self.target);
        let find = |n: &str| {
            gh.assets
                .iter()
                .find(|a| a.name == n)
                .map(|a| a.browser_download_url.clone())
        };
        Ok(Release {
            version,
            url: gh.html_url,
            asset: find(&name).map(|u| (name.clone(), u)),
            signature_url: find(&format!("{}.minisig", name)),
        })
    }

    /// Download the latest release's binary for this target and verify it.
    /// Returns the version and a temp file holding the verified binary.
    async fn download_verified(&self, dir: &Path) -> Result<(semver::Version, PathBuf)> {
        let key = self
            .public_key
            .as_deref()
            .context("this build cannot verify updates (no signing key)")?;
        let release = self
            .latest
            .lock()
            .await
            .clone()
            .context("no release checked yet")?;
        anyhow::ensure!(
            release.version > self.current,
            "already on {} or newer",
            release.version
        );
        let (name, asset_url) = release
            .asset
            .clone()
            .with_context(|| format!("no build for {}", self.target))?;
        let sig_url = release
            .signature_url
            .clone()
            .with_context(|| format!("{} has no signature", name))?;

        let binary = self.get_bytes(&asset_url).await?;
        let signature =
            String::from_utf8(self.get_bytes(&sig_url).await?).context("signature is not text")?;
        verify(
            &binary,
            &signature,
            key,
            &format!("rommd {} {}", release.version, name),
        )?;

        let path = dir.join(format!(".rommd-update-{}", std::process::id()));
        tokio::fs::write(&path, &binary)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).await?;
        }
        Ok((release.version, path))
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        self.client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {}", url))?
            .error_for_status()
            .with_context(|| format!("downloading {}", url))?
            .bytes()
            .await
            .map(|b| b.to_vec())
            .context("reading download")
    }

    /// Replace the running binary with the latest verified release. The new
    /// version runs after `restart`.
    pub async fn install(&self) -> Result<semver::Version> {
        self.status.lock().await.installing = true;
        let result = async {
            let exe = std::env::current_exe().context("locating the running binary")?;
            let dir = exe
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(std::env::temp_dir);
            let (version, file) = self.download_verified(&dir).await?;
            let replaced = tokio::task::spawn_blocking({
                let (exe, file) = (exe.clone(), file.clone());
                move || replace_binary(&exe, &file)
            })
            .await?;
            let _ = tokio::fs::remove_file(&file).await;
            replaced.with_context(|| format!("replacing {}", exe.display()))?;
            info!("installed rommd {}", version);
            Ok(version)
        }
        .await;
        let mut status = self.status.lock().await;
        status.installing = false;
        if let Err(e) = &result {
            status.last_error = Some(format!("{:#}", e));
        }
        result
    }
}

#[cfg(not(windows))]
fn replace_binary(_exe: &Path, new: &Path) -> std::io::Result<()> {
    self_replace::self_replace(new)
}

/// Windows lets a running program be renamed but not overwritten: move it
/// to `<exe>.old`, put the new binary in its place, and move it back if that
/// fails, so a failed update never leaves the install without a binary.
/// `self_replace` relocates the old binary first and then starts a helper
/// process, and an error in between left no binary at all.
#[cfg(windows)]
fn replace_binary(exe: &Path, new: &Path) -> std::io::Result<()> {
    let old = old_binary_path(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old)?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(e);
    }
    Ok(())
}

#[cfg(windows)]
fn old_binary_path(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

/// Delete the binary an update replaced; it ran until the restart.
pub fn remove_replaced_binary() {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_binary_path(&exe));
    }
}

/// Release asset name for a target triple.
pub fn asset_name(target: &str) -> String {
    if target.contains("windows") {
        format!("rommd-{}.exe", target)
    } else {
        format!("rommd-{}", target)
    }
}

fn verify(binary: &[u8], signature: &str, public_key: &str, expected_comment: &str) -> Result<()> {
    let key = minisign_verify::PublicKey::from_base64(public_key)
        .context("invalid built-in public key")?;
    let sig = minisign_verify::Signature::decode(signature).context("invalid signature file")?;
    key.verify(binary, &sig, false)
        .map_err(|e| anyhow::anyhow!("signature does not verify: {}", e))?;
    anyhow::ensure!(
        sig.trusted_comment() == expected_comment,
        "signature is for {:?}, expected {:?}",
        sig.trusted_comment(),
        expected_comment
    );
    Ok(())
}

/// Start the new binary. Under a service manager, exit and let it restart us;
/// otherwise start a new copy (which waits for the port) and exit.
pub fn restart() -> ! {
    if std::env::var_os(SUPERVISED_ENV).is_some() {
        info!("exiting so the service manager starts the new version");
        std::process::exit(0);
    }
    let started = std::env::current_exe().and_then(|exe| {
        let mut cmd = std::process::Command::new(exe);
        cmd.args(std::env::args_os().skip(1));
        // Running detached (`--background`), the new copy would otherwise
        // open a console window of its own.
        #[cfg(windows)]
        if !crate::service::console::has_window() {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.spawn()
    });
    match started {
        Ok(child) => {
            info!("started the new version (pid {}); exiting", child.id());
            std::process::exit(0)
        }
        Err(e) => {
            error!(
                "could not start the new version: {}; restart rommd manually",
                e
            );
            std::process::exit(1)
        }
    }
}

/// Daily checks; in auto mode, installs while no game is running. The loop
/// runs in every mode so a config change (see `reconfigure`) can turn checks
/// on without a restart.
pub fn spawn_background(updater: Arc<Updater>, processes: ProcessManager) {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        loop {
            if updater.mode() != UpdateMode::Off {
                check_and_apply(&updater, &processes).await;
            }
            tokio::select! {
                _ = tokio::time::sleep(CHECK_INTERVAL) => {}
                _ = updater.wake.notified() => {}
            }
        }
    });
}

async fn check_and_apply(updater: &Updater, processes: &ProcessManager) {
    let status = updater.check().await;
    if let Some(e) = &status.last_error {
        warn!("update check failed: {}", e);
        return;
    }
    if !status.update_available {
        return;
    }
    let latest = status.latest_version.clone().unwrap_or_default();
    match (updater.mode(), &status.install_blocker) {
        (UpdateMode::Auto, None) => {
            wait_for_no_games(processes).await;
            match updater.install().await {
                Ok(_) => {
                    // A game may have started during the download.
                    wait_for_no_games(processes).await;
                    restart();
                }
                Err(e) => warn!("installing rommd {} failed: {:#}", latest, e),
            }
        }
        (_, Some(blocker)) => info!("rommd {} is available, but {}", latest, blocker),
        _ => info!(
            "rommd {} is available: {}",
            latest,
            status.release_url.unwrap_or_default()
        ),
    }
}

async fn wait_for_no_games(processes: &ProcessManager) {
    while !processes.list().await.is_empty() {
        tokio::time::sleep(GAME_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::{Json, Router};

    const TARGET: &str = "x86_64-unknown-linux-musl";

    struct Signer {
        pk: minisign::PublicKey,
        sk: minisign::SecretKey,
    }

    impl Signer {
        fn new() -> Self {
            let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
            Self {
                pk: kp.pk,
                sk: kp.sk,
            }
        }

        fn sign(&self, data: &[u8], comment: &str) -> String {
            minisign::sign(
                Some(&self.pk),
                &self.sk,
                std::io::Cursor::new(data),
                Some(comment),
                None,
            )
            .unwrap()
            .into_string()
        }
    }

    /// A fake GitHub serving one release with the given assets.
    async fn github(tag: &'static str, assets: Vec<(String, Vec<u8>)>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let listed: Vec<serde_json::Value> = assets
            .iter()
            .map(|(name, _)| serde_json::json!({"name": name, "browser_download_url": format!("{}/dl/{}", base, name)}))
            .collect();
        let release = serde_json::json!({"tag_name": tag, "html_url": "https://example/release", "assets": listed});
        let files: std::collections::HashMap<String, Vec<u8>> = assets.into_iter().collect();
        let app = Router::new()
            .route(
                "/repos/o/r/releases/latest",
                get(move || async move { Json(release) }),
            )
            .route(
                "/dl/{name}",
                get(
                    move |axum::extract::Path(name): axum::extract::Path<String>| async move {
                        files[&name].clone()
                    },
                ),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    fn updater(base: String, key: Option<String>) -> Updater {
        Updater::with(
            UpdateMode::Auto,
            base,
            "o/r".into(),
            "0.1.0".parse().unwrap(),
            TARGET.into(),
            key,
        )
    }

    #[tokio::test]
    async fn installs_only_a_correctly_signed_newer_release() {
        let signer = Signer::new();
        let name = asset_name(TARGET);
        let binary = b"new rommd".to_vec();
        let base = github(
            "v0.2.0",
            vec![
                (name.clone(), binary.clone()),
                (
                    format!("{name}.minisig"),
                    signer
                        .sign(&binary, &format!("rommd 0.2.0 {name}"))
                        .into_bytes(),
                ),
            ],
        )
        .await;
        let up = updater(base, Some(signer.pk.to_base64()));
        let status = up.check().await;
        assert!(status.update_available, "{:?}", status);
        assert_eq!(status.latest_version.as_deref(), Some("0.2.0"));
        assert!(status.install_blocker.is_none());

        let dir = crate::testing::TempDir::new("update");
        let (version, file) = up.download_verified(dir.path()).await.unwrap();
        assert_eq!(version.to_string(), "0.2.0");
        assert_eq!(std::fs::read(file).unwrap(), binary);
    }

    #[tokio::test]
    async fn rejects_tampering_wrong_keys_and_relabelled_signatures() {
        let (signer, other) = (Signer::new(), Signer::new());
        let name = asset_name(TARGET);
        let dir = crate::testing::TempDir::new("update-bad");
        let cases: Vec<(&str, Vec<u8>, String)> = vec![
            (
                "tampered binary",
                b"evil".to_vec(),
                signer.sign(b"good", &format!("rommd 0.2.0 {name}")),
            ),
            (
                "other key",
                b"good".to_vec(),
                other.sign(b"good", &format!("rommd 0.2.0 {name}")),
            ),
            (
                "old release relabelled",
                b"good".to_vec(),
                signer.sign(b"good", &format!("rommd 0.0.9 {name}")),
            ),
            (
                "other platform's build",
                b"good".to_vec(),
                signer.sign(b"good", "rommd 0.2.0 rommd-aarch64-apple-darwin"),
            ),
        ];
        for (case, binary, sig) in cases {
            let base = github(
                "v0.2.0",
                vec![
                    (name.clone(), binary),
                    (format!("{name}.minisig"), sig.into_bytes()),
                ],
            )
            .await;
            let up = updater(base, Some(signer.pk.to_base64()));
            up.check().await;
            assert!(
                up.download_verified(dir.path()).await.is_err(),
                "{case} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn reports_but_never_installs_without_a_key_or_a_build() {
        let base = github("v0.2.0", vec![]).await;
        let up = updater(base.clone(), None);
        let status = up.check().await;
        assert!(status.update_available);
        assert!(status.install_blocker.is_some());
        assert!(
            up.download_verified(Path::new("/nonexistent"))
                .await
                .is_err()
        );

        let up = updater(base, Some(Signer::new().pk.to_base64()));
        let status = up.check().await;
        assert!(status.install_blocker.unwrap().contains("no build for"));
    }

    #[cfg(windows)]
    #[test]
    fn replaces_a_running_binary_and_keeps_it_on_failure() {
        let dir = crate::testing::TempDir::new("replace-binary");
        let exe = dir.path().join("app.exe");
        std::fs::copy(r"C:\Windows\System32\ping.exe", &exe).unwrap();
        let mut running = std::process::Command::new(&exe)
            .args(["-n", "5", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();

        let missing = dir.path().join("missing.exe");
        assert!(replace_binary(&exe, &missing).is_err());
        assert!(exe.is_file(), "a failed replace keeps the binary");

        let new = dir.path().join("new.exe");
        std::fs::write(&new, b"new version").unwrap();
        replace_binary(&exe, &new).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new version");
        assert!(old_binary_path(&exe).is_file());
        running.kill().unwrap();
        running.wait().unwrap();
    }

    #[tokio::test]
    async fn same_or_older_releases_are_not_updates() {
        let up = updater(github("v0.1.0", vec![]).await, None);
        assert!(!up.check().await.update_available);
        let up = updater(github("v0.0.5", vec![]).await, None);
        assert!(!up.check().await.update_available);
    }
}
