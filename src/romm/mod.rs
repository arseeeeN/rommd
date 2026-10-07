pub mod auth;
pub mod types;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio_util::io::ReaderStream;
use tracing::debug;

use crate::config::RommConfig;
use auth::TokenManager;
use types::{RomDetail, SaveSchema, StateSchema};

pub struct RommClient {
    base_url: String,
    tokens: TokenManager,
    client: reqwest::Client,
}

impl RommClient {
    pub fn new(config: &RommConfig) -> Self {
        let client = reqwest::Client::new();
        Self {
            base_url: config.base_url.trim_end_matches('/').to_owned(),
            tokens: TokenManager::new(
                &config.base_url,
                config.api_token.clone(),
                config.username.as_deref().unwrap_or(""),
                config.password.as_deref().unwrap_or(""),
                client.clone(),
            ),
            client,
        }
    }
}

// All methods are pub so callers can use the full client surface.
// Dead-code suppression covers methods not yet wired up (save download, etc.).
#[allow(dead_code)]
impl RommClient {
    // ── ROM ──────────────────────────────────────────────────────────────────

    pub async fn get_rom(&self, rom_id: i64) -> Result<RomDetail> {
        let token = self.tokens.get_token().await?;
        self.client
            .get(format!("{}/api/roms/{}", self.base_url, rom_id))
            .bearer_auth(token)
            .send()
            .await
            .context("GET /api/roms/{id}")?
            .error_for_status()
            .context("ROM not found")?
            .json::<RomDetail>()
            .await
            .context("parsing ROM response")
    }

    /// Ensure the ROM is present in `roms_dir/{platform_fs_slug}/` and return
    /// the path to launch.
    ///
    /// - `fs_name` is not a zip → download directly, done.
    /// - `fs_name` is a zip, `files.len() <= 1` → download the zip as-is, extract.
    /// - `fs_name` is a zip, `files.len() > 1`  → bundle all files via RomM's
    ///   `file_ids` endpoint, extract (handles multi-disc / .cue+.bin sets).
    pub async fn download_rom_to_dir(&self, rom: &RomDetail, roms_dir: &Path) -> Result<PathBuf> {
        let platform_dir = roms_dir.join(local_file_name(&rom.platform_fs_slug));
        let local_name = local_file_name(&rom.fs_name);

        // Multi-file ROM stored as component files with no zip wrapper in RomM
        // (fs_name has no extension, files list is non-empty). RomM's content
        // endpoint bundles all component files into a zip when file_ids is
        // provided, so we use the same extract approach as zip ROMs.
        if rom.files.len() > 1 && !rom.fs_name.ends_with(".zip") {
            let stem = Path::new(&local_name)
                .file_stem()
                .unwrap_or_else(|| std::ffi::OsStr::new(&local_name))
                .to_string_lossy()
                .into_owned();
            let rom_dir = platform_dir.join(&stem);

            if rom_dir.is_dir()
                && let Some(launch) = find_launch_file(&rom_dir).await?
            {
                debug!("ROM cache hit (multi-file dir): {}", launch.display());
                return Ok(launch);
            }
            // If a stale single-file download sits at the directory path, remove it.
            if rom_dir.is_file() {
                tokio::fs::remove_file(&rom_dir).await.ok();
                debug!(
                    "removed stale single-file download at {}",
                    rom_dir.display()
                );
            }

            tokio::fs::create_dir_all(&platform_dir).await?;
            // Use fs_name (no extension) in the URL so RomM names the .m3u cleanly.
            // The file_ids parameter is what triggers zip-bundle output regardless.
            let zip_dest = platform_dir.join(format!("{}.zip", stem));
            let file_ids = rom
                .files
                .iter()
                .map(|f| f.id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            self.download_content(rom.id, &rom.fs_name, Some(&file_ids), &zip_dest)
                .await?;

            extract_zip(&zip_dest, &rom_dir).await?;
            tokio::fs::remove_file(&zip_dest).await.ok();

            return find_launch_file(&rom_dir).await?.ok_or_else(|| {
                anyhow::anyhow!(
                    "no launchable file found after extracting ROM at {}",
                    rom_dir.display()
                )
            });
        }

        if !rom.fs_name.ends_with(".zip") {
            // Plain single-file ROM — download directly, no extraction.
            let dest = platform_dir.join(&local_name);
            if dest.is_file() {
                debug!("ROM cache hit: {}", dest.display());
                return Ok(dest);
            }
            // `dest` might collide with a directory left over from a prior
            // extraction. Look inside for a launchable file before re-downloading.
            if dest.is_dir()
                && let Some(launch) = find_launch_file(&dest).await?
            {
                debug!("ROM cache hit (extracted dir): {}", launch.display());
                return Ok(launch);
            }
            tokio::fs::create_dir_all(&platform_dir).await?;
            self.download_content(rom.id, &rom.fs_name, None, &dest)
                .await?;
            return Ok(dest);
        }

        // fs_name is a zip — need to extract.
        let stem = Path::new(&local_name)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let rom_dir = platform_dir.join(&stem);

        if rom_dir.is_dir()
            && let Some(launch) = find_launch_file(&rom_dir).await?
        {
            debug!("ROM cache hit: {}", launch.display());
            return Ok(launch);
        }

        tokio::fs::create_dir_all(&platform_dir).await?;
        let zip_dest = platform_dir.join(&local_name);

        if rom.files.len() > 1 {
            // Multi-file zip: ask RomM to bundle everything into one zip.
            let file_ids = rom
                .files
                .iter()
                .map(|f| f.id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            self.download_content(rom.id, &rom.fs_name, Some(&file_ids), &zip_dest)
                .await?;
        } else {
            // Single ROM stored in a zip — download fs_name directly.
            self.download_content(rom.id, &rom.fs_name, None, &zip_dest)
                .await?;
        }

        extract_zip(&zip_dest, &rom_dir).await?;
        tokio::fs::remove_file(&zip_dest).await.ok();

        find_launch_file(&rom_dir).await?.ok_or_else(|| {
            anyhow::anyhow!(
                "no launchable file found after extracting ROM at {}",
                rom_dir.display()
            )
        })
    }

    // ── Saves ─────────────────────────────────────────────────────────────────

    pub async fn get_saves(&self, rom_id: i64) -> Result<Vec<SaveSchema>> {
        let token = self.tokens.get_token().await?;
        self.client
            .get(format!("{}/api/saves", self.base_url))
            .query(&[("rom_id", rom_id.to_string())])
            .bearer_auth(token)
            .send()
            .await
            .context("GET /api/saves")?
            .error_for_status()?
            .json::<Vec<SaveSchema>>()
            .await
            .context("parsing saves")
    }

    /// Upload a save. With a `slot`, RomM stores it as a new version in that
    /// slot (tagging the file name with the upload time); `keep_versions` then
    /// has RomM delete all but the newest N saves in the slot — of every file
    /// and emulator, since its cleanup is scoped by ROM and slot only. Without
    /// a slot, RomM updates the entry with the same file name in place.
    ///
    /// `overwrite` is always set: without it RomM answers a slotted upload
    /// whose content already exists in the slot with that older entry instead
    /// of a new version, and a newer entry would stay the slot's latest.
    pub async fn upload_save(
        &self,
        rom_id: i64,
        emulator: &str,
        slot: Option<&str>,
        keep_versions: Option<u32>,
        file_path: &Path,
    ) -> Result<SaveSchema> {
        let token = self.tokens.get_token().await?;
        let file_name = file_path
            .file_name()
            .context("save file has no name")?
            .to_string_lossy()
            .into_owned();

        let file = tokio::fs::File::open(file_path)
            .await
            .with_context(|| format!("opening save {}", file_path.display()))?;
        let part =
            reqwest::multipart::Part::stream(reqwest::Body::wrap_stream(ReaderStream::new(file)))
                .file_name(file_name);
        let form = reqwest::multipart::Form::new().part("saveFile", part);

        let mut req = self
            .client
            .post(format!("{}/api/saves", self.base_url))
            .query(&[
                ("rom_id", rom_id.to_string()),
                ("emulator", emulator.to_string()),
                ("overwrite", "true".to_string()),
            ])
            .bearer_auth(token)
            .multipart(form);

        if let Some(slot) = slot {
            req = req.query(&[("slot", slot)]);
        }
        if let Some(keep) = keep_versions {
            req = req.query(&[
                ("autocleanup", "true".to_owned()),
                ("autocleanup_limit", keep.to_string()),
            ]);
        }

        let resp = req.send().await.context("POST /api/saves")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("save upload failed: {} — {}", status, body);
        }

        debug!("uploaded save {}", file_path.display());
        resp.json::<SaveSchema>()
            .await
            .context("parsing save upload response")
    }

    /// Update an existing save in-place by ID.
    ///
    /// Use this instead of `upload_save` when a save already exists in RomM — the
    /// PUT endpoint overwrites the stored file without creating a new entry, so we
    /// never accumulate duplicate timestamped copies.
    pub async fn update_save(&self, save_id: i64, file_path: &Path) -> Result<SaveSchema> {
        let token = self.tokens.get_token().await?;
        let file_name = file_path
            .file_name()
            .context("save file has no name")?
            .to_string_lossy()
            .into_owned();

        let file = tokio::fs::File::open(file_path)
            .await
            .with_context(|| format!("opening save {}", file_path.display()))?;
        let part =
            reqwest::multipart::Part::stream(reqwest::Body::wrap_stream(ReaderStream::new(file)))
                .file_name(file_name);
        let form = reqwest::multipart::Form::new().part("saveFile", part);

        let resp = self
            .client
            .put(format!("{}/api/saves/{}", self.base_url, save_id))
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await
            .context("PUT /api/saves/{id}")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("save update failed: {} — {}", status, body);
        }

        debug!("updated save id={} with {}", save_id, file_path.display());
        resp.json::<SaveSchema>()
            .await
            .context("parsing save update response")
    }

    pub async fn download_save(&self, save_id: i64, dest_path: &Path) -> Result<()> {
        let bytes = self
            .authed_get(&format!("{}/api/saves/{}/content", self.base_url, save_id))
            .await?
            .bytes()
            .await?;
        write_file(dest_path, &bytes).await
    }

    // ── States ────────────────────────────────────────────────────────────────

    pub async fn get_states(&self, rom_id: i64) -> Result<Vec<StateSchema>> {
        let token = self.tokens.get_token().await?;
        self.client
            .get(format!("{}/api/states", self.base_url))
            .query(&[("rom_id", rom_id.to_string())])
            .bearer_auth(token)
            .send()
            .await
            .context("GET /api/states")?
            .error_for_status()?
            .json::<Vec<StateSchema>>()
            .await
            .context("parsing states")
    }

    pub async fn upload_state(
        &self,
        rom_id: i64,
        emulator: &str,
        file_path: &Path,
    ) -> Result<StateSchema> {
        let token = self.tokens.get_token().await?;
        let file_name = file_path
            .file_name()
            .context("state file has no name")?
            .to_string_lossy()
            .into_owned();

        let file = tokio::fs::File::open(file_path).await?;
        let part =
            reqwest::multipart::Part::stream(reqwest::Body::wrap_stream(ReaderStream::new(file)))
                .file_name(file_name);
        let form = reqwest::multipart::Form::new().part("stateFile", part);

        let resp = self
            .client
            .post(format!("{}/api/states", self.base_url))
            .query(&[
                ("rom_id", rom_id.to_string()),
                ("emulator", emulator.to_string()),
            ])
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await
            .context("POST /api/states")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("state upload failed: {} — {}", status, body);
        }

        debug!("uploaded state {}", file_path.display());
        resp.json::<StateSchema>()
            .await
            .context("parsing state upload response")
    }

    /// Update an existing state in-place by ID (see `update_save`).
    pub async fn update_state(&self, state_id: i64, file_path: &Path) -> Result<StateSchema> {
        let token = self.tokens.get_token().await?;
        let file_name = file_path
            .file_name()
            .context("state file has no name")?
            .to_string_lossy()
            .into_owned();

        let file = tokio::fs::File::open(file_path)
            .await
            .with_context(|| format!("opening state {}", file_path.display()))?;
        let part =
            reqwest::multipart::Part::stream(reqwest::Body::wrap_stream(ReaderStream::new(file)))
                .file_name(file_name);
        let form = reqwest::multipart::Form::new().part("stateFile", part);

        let resp = self
            .client
            .put(format!("{}/api/states/{}", self.base_url, state_id))
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await
            .context("PUT /api/states/{id}")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("state update failed: {} — {}", status, body);
        }

        debug!("updated state id={} with {}", state_id, file_path.display());
        resp.json::<StateSchema>()
            .await
            .context("parsing state update response")
    }

    pub async fn download_state(&self, state_id: i64, dest_path: &Path) -> Result<()> {
        let bytes = self
            .authed_get(&format!(
                "{}/api/states/{}/content",
                self.base_url, state_id
            ))
            .await?
            .bytes()
            .await?;
        write_file(dest_path, &bytes).await
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    async fn download_content(
        &self,
        rom_id: i64,
        output_name: &str,
        file_ids: Option<&str>,
        dest: &Path,
    ) -> Result<()> {
        let token = self.tokens.get_token().await?;
        let encoded = urlencoding::encode(output_name);
        let url = format!("{}/api/roms/{}/content/{}", self.base_url, rom_id, encoded);

        let mut req = self.client.get(&url).bearer_auth(token);
        if let Some(ids) = file_ids {
            req = req.query(&[("file_ids", ids)]);
        }

        debug!("downloading ROM content from {}", url);
        let response = req
            .send()
            .await
            .context("GET rom content")?
            .error_for_status()
            .context("ROM content download failed")?;
        write_response(response, dest).await
    }

    async fn authed_get(&self, url: &str) -> Result<reqwest::Response> {
        let token = self.tokens.get_token().await?;
        self.client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .with_context(|| format!("GET {}", url))?
            .error_for_status()
            .with_context(|| format!("error response from {}", url))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// A name from RomM (`fs_name`, a platform slug) made safe to use as one path
/// component: never a separator or `..`, and on Windows none of the
/// characters it rejects or treats specially (`:` would write to an NTFS
/// alternate data stream instead of the file), no trailing dots or spaces
/// (Windows drops them) and no device name such as `CON`.
fn local_file_name(name: &str) -> String {
    let reserved = |c: char| {
        c == '/'
            || c == '\0'
            || (cfg!(windows)
                && (c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')))
    };
    let mut local: String = name
        .chars()
        .map(|c| if reserved(c) { '_' } else { c })
        .collect();
    if cfg!(windows) {
        local.truncate(local.trim_end_matches(['.', ' ']).len());
        let device = local.split('.').next().unwrap_or("").trim_end();
        let is_device = matches!(
            device.to_ascii_uppercase().as_str(),
            "CON" | "PRN" | "AUX" | "NUL"
        ) || (device.len() == 4
            && device.is_ascii()
            && ["COM", "LPT"]
                .iter()
                .any(|p| device[..3].eq_ignore_ascii_case(p))
            && device.as_bytes()[3].is_ascii_digit());
        if is_device {
            local.insert(0, '_');
        }
    }
    if local.is_empty() || local == "." || local == ".." {
        local = "_".into();
    }
    local
}

/// `path` with `suffix` appended to its file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Stream a download to `dest` through `<dest>.part`, so it never sits in
/// memory whole (Switch ROMs run to tens of GB) and an interrupted download
/// never leaves a truncated file that looks complete.
async fn write_response(mut response: reqwest::Response, dest: &Path) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let part = with_suffix(dest, ".part");
    let result = async {
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::File::create(&part)
            .await
            .with_context(|| format!("creating {}", part.display()))?;
        while let Some(chunk) = response.chunk().await.context("downloading")? {
            file.write_all(&chunk)
                .await
                .with_context(|| format!("writing {}", part.display()))?;
        }
        file.flush().await?;
        drop(file);
        tokio::fs::rename(&part, dest)
            .await
            .with_context(|| format!("moving the download to {}", dest.display()))
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    result
}

async fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, bytes)
        .await
        .with_context(|| format!("writing {}", path.display()))
}

/// Find the file to launch from an extracted ROM directory.
/// Prefers .m3u (multi-disc games); falls back to the single non-zip file
/// for ROMs that RomM stores compressed but are really single-file (e.g. GBA).
async fn first_m3u_entry(m3u_path: &Path, dir: &Path) -> Option<PathBuf> {
    let content = tokio::fs::read_to_string(m3u_path).await.ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Skip entries in subdirectories — the base game is always at the root.
        if line.contains('/') || line.contains('\\') {
            continue;
        }
        let candidate = dir.join(line);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

async fn find_launch_file(dir: &Path) -> Result<Option<PathBuf>> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut m3u: Option<PathBuf> = None;
    let mut cue: Option<PathBuf> = None;
    let mut others: Vec<PathBuf> = Vec::new();

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        match extension(&path).as_deref() {
            Some("m3u") => m3u = Some(path),
            Some("cue") => cue = Some(path),
            Some("zip") => {} // skip inner zips
            _ => others.push(path),
        }
    }

    // .m3u is the right launch target for PS1/PS2 multi-disc games where the
    // playlist ties together multiple .cue/.bin pairs. For modern single-container
    // formats (.nsp, .xci) RomM still generates an .m3u, but the emulator expects
    // the actual game file — skip .m3u when the other files are not disc images.
    let is_disc_game = !others.is_empty()
        && others.iter().all(|p| {
            matches!(
                extension(p).as_deref(),
                Some("bin") | Some("iso") | Some("img") | Some("chd") | Some("sbi")
            )
        });
    if is_disc_game && let Some(m3u) = m3u {
        return Ok(Some(m3u));
    }
    if let Some(cue) = cue {
        return Ok(Some(cue));
    }
    if others.len() == 1 {
        return Ok(others.into_iter().next());
    }
    // Multiple non-disc files (e.g. base + update + DLC all at the top level):
    // treat the .m3u as an ordered index — the base game is listed first.
    if let Some(m3u_path) = m3u
        && let Some(primary) = first_m3u_entry(&m3u_path, dir).await
    {
        return Ok(Some(primary));
    }
    Ok(None)
}

/// Lowercase extension, since disc dumps often come as `GAME.CUE`.
fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

/// Extract into `<dest_dir>.extracting` and move it into place when done, so
/// an interrupted extraction never leaves a partial game at `dest_dir`.
async fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<()> {
    let zip_path = zip_path.to_owned();
    let dest_dir = dest_dir.to_owned();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let staging = with_suffix(&dest_dir, ".extracting");
        if staging.exists() {
            std::fs::remove_dir_all(&staging)
                .with_context(|| format!("removing {}", staging.display()))?;
        }
        let file = std::fs::File::open(&zip_path)
            .with_context(|| format!("opening zip {}", zip_path.display()))?;
        let mut archive = zip::ZipArchive::new(file).context("reading zip archive")?;
        std::fs::create_dir_all(&staging)?;
        if let Err(e) = archive.extract(&staging) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e).context("extracting zip");
        }
        if dest_dir.is_dir() {
            std::fs::remove_dir_all(&dest_dir)
                .with_context(|| format!("removing the old {}", dest_dir.display()))?;
        }
        std::fs::rename(&staging, &dest_dir)
            .with_context(|| format!("moving the extracted game to {}", dest_dir.display()))
    })
    .await
    .context("zip extraction task panicked")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn local_names_stay_one_safe_component() {
        assert_eq!(
            local_file_name("Golden Sun (USA).gba"),
            "Golden Sun (USA).gba"
        );
        assert_eq!(local_file_name("../../evil.gba"), ".._.._evil.gba");
        assert_eq!(local_file_name(".."), "_");
        assert_eq!(local_file_name(""), "_");
        if cfg!(windows) {
            assert_eq!(
                local_file_name("Zelda: Link's Awakening?.gb"),
                "Zelda_ Link's Awakening_.gb"
            );
            assert_eq!(local_file_name(r"a\b.gb"), "a_b.gb");
            assert_eq!(local_file_name("Game. "), "Game");
            assert_eq!(local_file_name("con.gb"), "_con.gb");
            assert_eq!(local_file_name("COM1"), "_COM1");
            assert_eq!(local_file_name("Console.gb"), "Console.gb");
        }
    }

    async fn serve(status: u16, body: &'static [u8]) -> RommClient {
        use axum::{Router, http::StatusCode, routing::get};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/api/roms/{id}/content/{name}",
            get(move || async move { (StatusCode::from_u16(status).unwrap(), body) }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        RommClient::new(&RommConfig {
            base_url: base,
            api_token: Some("token".into()),
            username: None,
            password: None,
        })
    }

    #[tokio::test]
    async fn downloads_land_whole_or_not_at_all() {
        let dir = TempDir::new("rom-download");
        let dest = dir.path().join("gba/game.gba");

        let failing = serve(500, b"").await;
        assert!(
            failing
                .download_content(1, "game.gba", None, &dest)
                .await
                .is_err()
        );
        assert!(!dest.exists());
        assert!(!with_suffix(&dest, ".part").exists());

        let ok = serve(200, b"rom bytes").await;
        ok.download_content(1, "game.gba", None, &dest)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"rom bytes");
        assert!(!with_suffix(&dest, ".part").exists());
    }
}
