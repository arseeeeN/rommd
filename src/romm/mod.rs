pub mod auth;
pub mod types;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::config::RommConfig;
use auth::TokenManager;
use types::{PlatformSchema, RomDetail, RomFileSchema, RomPage, SaveSchema, StateSchema};

pub struct RommClient {
    base_url: String,
    tokens: TokenManager,
    client: reqwest::Client,
}

impl RommClient {
    pub fn new(config: &RommConfig) -> Self {
        // No overall timeout, which would cut off a large ROM download; a
        // read timeout instead catches a server that stops sending.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .build()
            .expect("building the HTTP client");
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
    /// Whether `base_url` is a RomM server that accepts our credentials.
    /// Returns its version. Run at startup, so a wrong address or token shows
    /// up in the log instead of only when a game is launched.
    pub async fn check(&self) -> Result<String> {
        let version = self.server_version().await?;
        self.check_login(&version).await?;
        Ok(version)
    }

    /// The RomM version at `base_url`, or why it isn't a RomM server.
    pub async fn server_version(&self) -> Result<String> {
        let url = format!("{}/api/heartbeat", self.base_url);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("cannot reach {}", self.base_url))?;
        let heartbeat: serde_json::Value = match response.error_for_status() {
            Ok(r) => r.json().await.ok(),
            Err(_) => None,
        }
        .with_context(|| {
            format!(
                "{} does not look like a RomM server (no /api/heartbeat); check romm.base_url",
                self.base_url
            )
        })?;
        let version = heartbeat["SYSTEM"]["VERSION"]
            .as_str()
            .unwrap_or("of unknown version")
            .to_owned();
        Ok(version)
    }

    /// Whether RomM accepts the credentials.
    pub async fn check_login(&self, version: &str) -> Result<()> {
        let token = self
            .tokens
            .get_token()
            .await
            .context("RomM rejected the username and password")?;
        let me = self
            .client
            .get(format!("{}/api/users/me", self.base_url))
            .bearer_auth(token)
            .send()
            .await
            .context("checking the RomM login")?;
        anyhow::ensure!(
            me.status().is_success(),
            "RomM {} rejected the credentials ({}); check romm.api_token or username and password",
            version,
            me.status()
        );
        Ok(())
    }

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

    pub async fn platforms(&self) -> Result<Vec<PlatformSchema>> {
        let token = self.tokens.get_token().await?;
        self.client
            .get(format!("{}/api/platforms", self.base_url))
            .bearer_auth(token)
            .send()
            .await
            .context("GET /api/platforms")?
            .error_for_status()
            .context("listing platforms")?
            .json()
            .await
            .context("parsing platforms")
    }

    /// ROMs whose name or file name contains `term`, on `platforms` if any,
    /// sorted by name.
    pub async fn search_roms(&self, term: &str, platforms: &[i64], limit: u32) -> Result<RomPage> {
        let token = self.tokens.get_token().await?;
        let mut query: Vec<(&str, String)> = vec![
            ("search_term", term.to_owned()),
            ("limit", limit.to_string()),
            ("order_by", "name".into()),
            ("order_dir", "asc".into()),
            ("with_char_index", "false".into()),
            ("with_filter_values", "false".into()),
        ];
        query.extend(platforms.iter().map(|id| ("platform_ids", id.to_string())));
        self.client
            .get(format!("{}/api/roms", self.base_url))
            .bearer_auth(token)
            .query(&query)
            .send()
            .await
            .context("GET /api/roms")?
            .error_for_status()
            .context("searching ROMs")?
            .json()
            .await
            .context("parsing ROM search results")
    }

    /// Ensure the ROM is present in `roms_dir/{platform_fs_slug}/` and return
    /// the path to launch.
    ///
    /// - `fs_name` is not a zip → download directly, done.
    /// - `fs_name` is a zip, `files.len() <= 1` → download the zip as-is, extract.
    /// - `fs_name` is a zip, `files.len() > 1`  → bundle all files via RomM's
    ///   `file_ids` endpoint, extract (handles multi-disc / .cue+.bin sets).
    ///
    /// `transfer` reports progress and can cancel the download; extracting a
    /// finished download is not cancelled.
    pub async fn download_rom_to_dir(
        &self,
        rom: &RomDetail,
        roms_dir: &Path,
        transfer: &Transfer,
    ) -> Result<PathBuf> {
        let platform_dir = roms_dir.join(local_file_name(&rom.platform_fs_slug));
        let local_name = local_file_name(&rom.fs_name);

        // A ROM made of several files (a game and its update, the discs of a
        // multi-disc game): download them one by one into a folder laid out
        // as on the server. RomM's per-file endpoint resumes, unlike the zip
        // it bundles them into, and nothing needs extracting afterwards.
        if rom.files.len() > 1 && !rom.fs_name.ends_with(".zip") {
            let stem = Path::new(&local_name)
                .file_stem()
                .unwrap_or_else(|| std::ffi::OsStr::new(&local_name))
                .to_string_lossy()
                .into_owned();
            let rom_dir = platform_dir.join(&stem);
            // If a stale single-file download sits at the directory path, remove it.
            if rom_dir.is_file() {
                tokio::fs::remove_file(&rom_dir).await.ok();
                debug!(
                    "removed stale single-file download at {}",
                    rom_dir.display()
                );
            }
            let files = rom_files(rom, &rom_dir)?;
            let complete = |(f, path): &&(&RomFileSchema, PathBuf)| {
                std::fs::metadata(path)
                    .is_ok_and(|m| m.is_file() && f.file_size_bytes.is_none_or(|s| s == m.len()))
            };
            let missing: Vec<_> = files.iter().filter(|f| !complete(f)).collect();
            if missing.is_empty() {
                debug!("ROM cache hit (multi-file dir): {}", rom_dir.display());
            } else {
                let size = |(f, _): &&(&RomFileSchema, PathBuf)| f.file_size_bytes.unwrap_or(0);
                let total: u64 = files.iter().map(|f| size(&f)).sum();
                let done: u64 = files.iter().filter(|f| complete(f)).map(|f| size(&f)).sum();
                transfer.begin(Some(total));
                transfer.add(done);
                for (file, path) in missing {
                    if let Err(e) = self.download_file(file, path, transfer).await {
                        // A cancelled or refused download leaves folders it
                        // created empty; a `.part` kept for resuming isn't.
                        remove_empty_dirs(&rom_dir);
                        return Err(e);
                    }
                }
            }
            return launch_file(&rom_dir, &stem, &files).await;
        }

        // RomM's size for a ROM that is one file, to check the download.
        let single_size = match rom.files.as_slice() {
            [only] => only.file_size_bytes,
            _ => None,
        };
        if !rom.fs_name.ends_with(".zip") {
            // Plain single-file ROM — download directly, no extraction.
            let dest = platform_dir.join(&local_name);
            let size = std::fs::metadata(&dest)
                .ok()
                .filter(|m| m.is_file())
                .map(|m| m.len());
            if size.is_some() && (single_size.is_none() || size == single_size) {
                debug!("ROM cache hit: {}", dest.display());
                return Ok(dest);
            }
            if let (Some(have), Some(want)) = (size, single_size) {
                // Not rommd's doing (it only ever moves whole downloads into
                // place): download it again; the old file stays until then.
                warn!(
                    "{} has {have} bytes, RomM says {want}; downloading it again",
                    dest.display()
                );
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
            self.download_content(rom.id, &rom.fs_name, None, &dest, single_size, transfer)
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
            self.download_content(
                rom.id,
                &rom.fs_name,
                Some(&file_ids),
                &zip_dest,
                None,
                transfer,
            )
            .await?;
        } else {
            // Single ROM stored in a zip — download fs_name directly.
            self.download_content(rom.id, &rom.fs_name, None, &zip_dest, single_size, transfer)
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
        expected: Option<u64>,
        transfer: &Transfer,
    ) -> Result<()> {
        let url = format!(
            "{}/api/roms/{}/content/{}",
            self.base_url,
            rom_id,
            urlencoding::encode(output_name)
        );
        let query: Vec<(&str, &str)> = file_ids.map(|ids| ("file_ids", ids)).into_iter().collect();
        self.fetch_to(&url, &query, dest, expected, transfer).await
    }

    /// One file of a multi-file ROM, through RomM's per-file endpoint, which
    /// (unlike the zip bundle) answers range requests.
    async fn download_file(
        &self,
        file: &RomFileSchema,
        dest: &Path,
        transfer: &Transfer,
    ) -> Result<()> {
        let url = format!(
            "{}/api/roms/{}/files/content/{}",
            self.base_url,
            file.id,
            urlencoding::encode(&file.file_name)
        );
        self.fetch_to(&url, &[], dest, file.file_size_bytes, transfer)
            .await
    }

    /// Download to `dest` through `dest.part`, so `dest` only ever appears
    /// whole. A `.part` left by an earlier attempt is continued with a range
    /// request; a dropped connection (a laptop or Deck going to sleep, Wi-Fi
    /// gone) is retried for about a minute and otherwise keeps the `.part`
    /// for the next launch. A cancel or a refused download removes it.
    async fn fetch_to(
        &self,
        url: &str,
        query: &[(&str, &str)],
        dest: &Path,
        expected: Option<u64>,
        transfer: &Transfer,
    ) -> Result<()> {
        let part = with_suffix(dest, ".part");
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        debug!("downloading ROM content from {}", url);
        let earlier = part_len(&part).await;
        transfer.add(earlier);
        let mut retries = RETRY_DELAYS.iter();
        loop {
            match self.fetch_attempt(url, query, &part, transfer).await {
                Ok(()) => break,
                Err(Failed::Retry(e)) => match retries.next() {
                    Some(delay) => {
                        warn!(
                            "download interrupted ({:#}); continuing in {} s",
                            e,
                            delay.as_secs()
                        );
                        tokio::select! {
                            _ = transfer.cancel.cancelled() => {
                                let _ = tokio::fs::remove_file(&part).await;
                                anyhow::bail!("download cancelled");
                            }
                            _ = tokio::time::sleep(*delay) => {}
                        }
                    }
                    None => {
                        return Err(e.context(
                            "download interrupted; the next launch continues where it stopped",
                        ));
                    }
                },
                Err(Failed::Fatal(e)) => {
                    let _ = tokio::fs::remove_file(&part).await;
                    return Err(e);
                }
            }
        }
        let got = part_len(&part).await;
        if let Some(size) = expected.filter(|size| *size != got) {
            let _ = tokio::fs::remove_file(&part).await;
            anyhow::bail!(
                "downloaded {got} bytes of {}, RomM said {size}",
                dest.display()
            );
        }
        tokio::fs::rename(&part, dest)
            .await
            .with_context(|| format!("moving the download to {}", dest.display()))
    }

    /// One request, appending to `part` from where it ends.
    async fn fetch_attempt(
        &self,
        url: &str,
        query: &[(&str, &str)],
        part: &Path,
        transfer: &Transfer,
    ) -> std::result::Result<(), Failed> {
        use tokio::io::AsyncWriteExt;
        let have = part_len(part).await;
        let token = self.tokens.get_token().await.map_err(Failed::Retry)?;
        let mut request = self.client.get(url).bearer_auth(token).query(query);
        if have > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let response = tokio::select! {
            _ = transfer.cancel.cancelled() => return Err(Failed::Fatal(anyhow::anyhow!("download cancelled"))),
            response = request.send() => response.map_err(|e| Failed::Retry(e.into()))?,
        };
        let status = response.status();
        let resumed = status == reqwest::StatusCode::PARTIAL_CONTENT;
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(Failed::Retry(anyhow::anyhow!("RomM answered {status}")));
        }
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // The part is already whole (or longer than the file): start over.
            let _ = tokio::fs::remove_file(part).await;
            transfer.sub(have);
            return Err(Failed::Retry(anyhow::anyhow!(
                "RomM refused to continue the download"
            )));
        }
        if !status.is_success() {
            return Err(Failed::Fatal(anyhow::anyhow!(
                "ROM content download failed: RomM answered {status}"
            )));
        }
        if !resumed && have > 0 {
            debug!(
                "RomM ignored the range request; downloading {} again",
                part.display()
            );
            transfer.sub(have);
        }
        let already = if resumed { have } else { 0 };
        transfer.begin(response.content_length().map(|len| len + already));
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resumed)
            .truncate(!resumed)
            .open(part)
            .await
            .map_err(|e| {
                Failed::Fatal(anyhow::Error::new(e).context(format!("opening {}", part.display())))
            })?;
        let mut response = response;
        loop {
            let chunk = tokio::select! {
                _ = transfer.cancel.cancelled() => return Err(Failed::Fatal(anyhow::anyhow!("download cancelled"))),
                chunk = response.chunk() => chunk.map_err(|e| Failed::Retry(e.into()))?,
            };
            let Some(chunk) = chunk else { break };
            file.write_all(&chunk).await.map_err(|e| {
                Failed::Fatal(anyhow::Error::new(e).context(format!("writing {}", part.display())))
            })?;
            transfer.add(chunk.len() as u64);
        }
        file.flush().await.map_err(|e| Failed::Fatal(e.into()))
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

/// A ROM download as it runs: bytes so far, the size when the server sends
/// one, and a token that cancels it. Only the transfer itself is cancelled; a
/// finished download is always extracted.
#[derive(Clone)]
pub struct Transfer {
    pub cancel: CancellationToken,
    progress: Arc<Mutex<Progress>>,
    started: Arc<tokio::sync::watch::Sender<bool>>,
}

impl Default for Transfer {
    fn default() -> Self {
        Self {
            cancel: CancellationToken::new(),
            progress: Default::default(),
            started: Arc::new(tokio::sync::watch::Sender::new(false)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Progress {
    /// Whether bytes are being transferred; false while only the cache is
    /// checked.
    pub started: bool,
    pub done: u64,
    pub total: Option<u64>,
}

impl Transfer {
    pub fn progress(&self) -> Progress {
        *self.progress.lock().unwrap()
    }

    /// Resolves once bytes are being transferred: never for a ROM that is
    /// already downloaded.
    pub async fn wait_started(&self) {
        let mut rx = self.started.subscribe();
        let _ = rx.wait_for(|started| *started).await;
    }

    /// Bytes start flowing. The first call sets the total: a multi-file ROM
    /// announces the sum of its files before the first one starts.
    fn begin(&self, total: Option<u64>) {
        {
            let mut p = self.progress.lock().unwrap();
            if p.started {
                return;
            }
            p.started = true;
            p.total = total;
        }
        self.started.send_replace(true);
    }

    fn add(&self, bytes: u64) {
        self.progress.lock().unwrap().done += bytes;
    }

    fn sub(&self, bytes: u64) {
        let mut p = self.progress.lock().unwrap();
        p.done = p.done.saturating_sub(bytes);
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
/// How long to wait before each retry of a dropped download: about a minute
/// in all, enough for the network to come back after waking from sleep.
const RETRY_DELAYS: &[Duration] = if cfg!(test) {
    &[Duration::ZERO, Duration::ZERO, Duration::ZERO]
} else {
    &[
        Duration::from_secs(2),
        Duration::from_secs(5),
        Duration::from_secs(10),
        Duration::from_secs(20),
        Duration::from_secs(30),
    ]
};

enum Failed {
    /// The connection dropped or RomM is briefly unavailable: try again.
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

async fn part_len(part: &Path) -> u64 {
    tokio::fs::metadata(part).await.map_or(0, |m| m.len())
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

/// Each file of a multi-file ROM with where it goes: its folder on the
/// server, relative to the ROM's, under `rom_dir`.
fn rom_files<'a>(rom: &'a RomDetail, rom_dir: &Path) -> Result<Vec<(&'a RomFileSchema, PathBuf)>> {
    rom.files
        .iter()
        .map(|f| {
            let relative = f.file_path.strip_prefix(&rom.full_path).with_context(|| {
                format!(
                    "RomM lists {} in {}, outside the game's folder {}",
                    f.file_name, f.file_path, rom.full_path
                )
            })?;
            let mut path = rom_dir.to_owned();
            for part in relative.split('/').filter(|p| !p.is_empty()) {
                path.push(local_file_name(part));
            }
            path.push(local_file_name(&f.file_name));
            Ok((f, path))
        })
        .collect()
}

/// What to start for a multi-file ROM: a playlist for a multi-disc game,
/// else the one file RomM calls the game, else what `find_launch_file` finds
/// (e.g. in a folder downloaded as RomM's bundle, with its playlist).
async fn launch_file(
    rom_dir: &Path,
    stem: &str,
    files: &[(&RomFileSchema, PathBuf)],
) -> Result<PathBuf> {
    let top_level: Vec<&PathBuf> = files
        .iter()
        .map(|(_, p)| p)
        .filter(|p| p.parent() == Some(rom_dir))
        .collect();
    let with_ext = |exts: &[&str]| -> Vec<&PathBuf> {
        let mut found: Vec<&PathBuf> = top_level
            .iter()
            .copied()
            .filter(|p| extension(p).is_some_and(|e| exts.contains(&e.as_str())))
            .collect();
        found.sort();
        found
    };
    let mut discs = with_ext(&["cue"]);
    if discs.is_empty() {
        discs = with_ext(&["chd", "iso", "cso", "pbp"]);
    }
    if discs.len() > 1 {
        // RomM bundles a playlist in its zip; per-file downloads write their
        // own, listing the discs in name order. One already there is kept.
        let m3u = rom_dir.join(format!("{stem}.m3u"));
        if !m3u.exists() {
            let list: String = discs
                .iter()
                .filter_map(|p| p.file_name())
                .map(|n| format!("{}\n", n.to_string_lossy()))
                .collect();
            tokio::fs::write(&m3u, list)
                .await
                .with_context(|| format!("writing {}", m3u.display()))?;
        }
        return Ok(m3u);
    }
    let games: Vec<&PathBuf> = files
        .iter()
        .filter(|(f, p)| f.category.as_deref() == Some("game") && p.parent() == Some(rom_dir))
        .map(|(_, p)| p)
        .collect();
    if let [game] = games.as_slice()
        && extension(game).as_deref() != Some("bin")
    {
        return Ok((*game).clone());
    }
    find_launch_file(rom_dir)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no launchable file found in {}", rom_dir.display()))
}

/// Remove `dir` and the folders in it that hold no files at all.
fn remove_empty_dirs(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                remove_empty_dirs(&entry.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
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

    fn client(base_url: String, token: &str) -> RommClient {
        RommClient::new(&RommConfig {
            base_url,
            api_token: Some(token.into()),
            username: None,
            password: None,
        })
    }

    #[tokio::test]
    async fn check_names_what_is_wrong_with_the_connection() {
        use axum::{Json, Router, http::HeaderMap, http::StatusCode, routing::get};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/api/heartbeat",
                get(|| async { Json(serde_json::json!({"SYSTEM": {"VERSION": "5.2.0"}})) }),
            )
            .route(
                "/api/users/me",
                get(|headers: HeaderMap| async move {
                    let ok = headers
                        .get("authorization")
                        .is_some_and(|v| v == "Bearer good");
                    if ok {
                        StatusCode::OK
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        assert_eq!(client(base.clone(), "good").check().await.unwrap(), "5.2.0");

        let bad_token = format!(
            "{:#}",
            client(base.clone(), "bad").check().await.unwrap_err()
        );
        assert!(
            bad_token.contains("rejected the credentials"),
            "{bad_token}"
        );

        let not_romm = format!(
            "{:#}",
            client(format!("{base}/sub"), "good")
                .check()
                .await
                .unwrap_err()
        );
        assert!(
            not_romm.contains("does not look like a RomM server"),
            "{not_romm}"
        );

        let unreachable = format!(
            "{:#}",
            client("http://127.0.0.1:1".into(), "good")
                .check()
                .await
                .unwrap_err()
        );
        assert!(unreachable.contains("cannot reach"), "{unreachable}");
    }

    #[tokio::test]
    async fn downloads_land_whole_or_not_at_all() {
        let dir = TempDir::new("rom-download");
        let dest = dir.path().join("gba/game.gba");

        let failing = serve(500, b"").await;
        assert!(
            failing
                .download_content(1, "game.gba", None, &dest, None, &Transfer::default())
                .await
                .is_err()
        );
        assert!(!dest.exists());
        assert!(!with_suffix(&dest, ".part").exists());

        let ok = serve(200, b"rom bytes").await;
        ok.download_content(1, "game.gba", None, &dest, None, &Transfer::default())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"rom bytes");
        assert!(!with_suffix(&dest, ".part").exists());
    }

    /// Serves a ROM that sends one chunk and then stalls, like a big
    /// download still in progress.
    async fn serve_stalling() -> RommClient {
        use axum::{Router, body::Body, routing::get};
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stall = || async {
            let (mut tx, rx) = tokio::io::duplex(64);
            tokio::spawn(async move {
                tx.write_all(b"first chunk").await.unwrap();
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            });
            Body::from_stream(ReaderStream::new(rx))
        };
        let app = Router::new()
            .route("/api/roms/{id}/content/{name}", get(stall))
            .route("/api/roms/{id}/files/content/{name}", get(stall));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        client(base, "token")
    }

    #[tokio::test]
    async fn a_cancelled_download_leaves_nothing_behind() {
        let dir = TempDir::new("rom-cancel");
        let dest = dir.path().join("switch/game.nsp");
        let romm = serve_stalling().await;
        let transfer = Transfer::default();
        let download = {
            let (dest, transfer) = (dest.clone(), transfer.clone());
            tokio::spawn(async move {
                romm.download_content(1, "game.nsp", None, &dest, None, &transfer)
                    .await
            })
        };
        while transfer.progress().done == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(transfer.progress().started);
        assert!(with_suffix(&dest, ".part").exists());

        transfer.cancel.cancel();
        let err = download.await.unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
        assert!(!with_suffix(&dest, ".part").exists());
        assert!(!dest.exists());
    }

    /// Serves ROM files like RomM: by file ID (per-file endpoint) or by ROM
    /// ID (single-file ROMs). With `cut_first`, each file's first response
    /// stops halfway, like a connection dropped when a Deck goes to sleep;
    /// without `ranges`, range requests are ignored. Logs (id, Range header).
    struct FileServer {
        files: std::collections::HashMap<i64, Vec<u8>>,
        cut_first: bool,
        ranges: bool,
        log: Mutex<Vec<(i64, Option<String>)>>,
    }

    async fn serve_files(server: Arc<FileServer>) -> RommClient {
        use axum::extract::{Path as UrlPath, State};
        use axum::http::{HeaderMap, StatusCode, header};
        use axum::{Router, body::Body, response::Response, routing::get};
        use tokio::io::AsyncWriteExt;
        async fn file(
            State(s): State<Arc<FileServer>>,
            UrlPath((id, _name)): UrlPath<(i64, String)>,
            headers: HeaderMap,
        ) -> Response {
            let data = s.files[&id].clone();
            let range = headers
                .get(header::RANGE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let first = {
                let mut log = s.log.lock().unwrap();
                log.push((id, range.clone()));
                log.iter().filter(|(i, _)| *i == id).count() == 1
            };
            let start: usize = range
                .filter(|_| s.ranges)
                .and_then(|r| r.strip_prefix("bytes=")?.strip_suffix('-')?.parse().ok())
                .unwrap_or(0);
            let body = data[start..].to_vec();
            let len = body.len();
            let sent = if s.cut_first && first { len / 2 } else { len };
            let (mut tx, rx) = tokio::io::duplex(1 << 16);
            tokio::spawn(async move {
                let _ = tx.write_all(&body[..sent]).await;
                // Let the half that was sent reach the client before the
                // connection is cut.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            });
            Response::builder()
                .status(if start > 0 {
                    StatusCode::PARTIAL_CONTENT
                } else {
                    StatusCode::OK
                })
                .header(header::CONTENT_LENGTH, len)
                .body(Body::from_stream(ReaderStream::new(rx)))
                .unwrap()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/api/roms/{id}/files/content/{name}", get(file))
            .route("/api/roms/{id}/content/{name}", get(file))
            .with_state(server);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        client(base, "token")
    }

    fn server(files: &[(i64, &[u8])], cut_first: bool, ranges: bool) -> Arc<FileServer> {
        Arc::new(FileServer {
            files: files.iter().map(|(id, d)| (*id, d.to_vec())).collect(),
            cut_first,
            ranges,
            log: Mutex::new(Vec::new()),
        })
    }

    /// A multi-file ROM as RomM describes it: (file ID, name, folder under
    /// the game, size, category).
    fn multi_file_rom(files: &[(i64, &str, &str, usize, &str)]) -> RomDetail {
        let files: Vec<_> = files
            .iter()
            .map(|(id, name, sub, size, category)| {
                serde_json::json!({
                    "id": id, "file_name": name, "file_size_bytes": size, "category": category,
                    "file_path": format!("roms/switch/Game{}", if sub.is_empty() { String::new() } else { format!("/{sub}") }),
                })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "id": 7, "fs_name": "Game", "name": "Game", "platform_slug": "switch",
            "platform_fs_slug": "switch", "full_path": "roms/switch/Game", "files": files,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn multi_file_roms_download_per_file_and_resume_after_a_drop() {
        let (game, update) = (vec![1u8; 3000], vec![2u8; 1000]);
        let files = server(&[(251, &game), (252, &update)], true, true);
        let romm = serve_files(files.clone()).await;
        let rom = multi_file_rom(&[
            (251, "Game.xci", "", game.len(), "game"),
            (252, "Update.nsp", "update", update.len(), "update"),
        ]);
        let dir = TempDir::new("rom-per-file");
        let transfer = Transfer::default();

        let launch = romm
            .download_rom_to_dir(&rom, dir.path(), &transfer)
            .await
            .unwrap();
        let game_dir = dir.path().join("switch/Game");
        assert_eq!(
            launch,
            game_dir.join("Game.xci"),
            "the file RomM calls the game"
        );
        assert_eq!(std::fs::read(game_dir.join("Game.xci")).unwrap(), game);
        assert_eq!(
            std::fs::read(game_dir.join("update/Update.nsp")).unwrap(),
            update
        );
        let progress = transfer.progress();
        assert_eq!((progress.done, progress.total), (4000, Some(4000)));
        assert_eq!(
            *files.log.lock().unwrap(),
            vec![
                (251, None),
                (251, Some("bytes=1500-".into())),
                (252, None),
                (252, Some("bytes=500-".into())),
            ],
            "each file continues where its dropped connection stopped"
        );

        let again = romm
            .download_rom_to_dir(&rom, dir.path(), &Transfer::default())
            .await
            .unwrap();
        assert_eq!(again, launch);
        assert_eq!(
            files.log.lock().unwrap().len(),
            4,
            "a complete game is not fetched again"
        );
    }

    #[tokio::test]
    async fn a_game_downloaded_as_romms_bundle_is_not_fetched_again() {
        let files = server(&[], false, true);
        let romm = serve_files(files.clone()).await;
        let rom = multi_file_rom(&[
            (251, "Game.xci", "", 3, "game"),
            (252, "Update.nsp", "update", 2, "update"),
        ]);
        let dir = TempDir::new("rom-bundle-layout");
        let game_dir = dir.path().join("switch/Game");
        crate::testing::write(&game_dir.join("Game.xci"), b"xci");
        crate::testing::write(&game_dir.join("update/Update.nsp"), b"up");
        crate::testing::write(&game_dir.join("Game.m3u"), b"Game.xci\n");
        let launch = romm
            .download_rom_to_dir(&rom, dir.path(), &Transfer::default())
            .await
            .unwrap();
        assert_eq!(launch, game_dir.join("Game.xci"));
        assert!(files.log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_multi_disc_game_gets_a_playlist_of_its_discs() {
        let files = server(
            &[(1, b"c1"), (2, b"b1"), (3, b"c2"), (4, b"b2")],
            false,
            true,
        );
        let romm = serve_files(files).await;
        let rom = multi_file_rom(&[
            (3, "Game (Disc 2).cue", "", 2, "game"),
            (1, "Game (Disc 1).cue", "", 2, "game"),
            (2, "Game (Disc 1).bin", "", 2, "game"),
            (4, "Game (Disc 2).bin", "", 2, "game"),
        ]);
        let dir = TempDir::new("rom-discs");
        let launch = romm
            .download_rom_to_dir(&rom, dir.path(), &Transfer::default())
            .await
            .unwrap();
        assert_eq!(launch, dir.path().join("switch/Game/Game.m3u"));
        assert_eq!(
            std::fs::read_to_string(&launch).unwrap(),
            "Game (Disc 1).cue\nGame (Disc 2).cue\n"
        );
    }

    #[tokio::test]
    async fn a_server_ignoring_ranges_still_yields_the_whole_file_once() {
        let data = vec![9u8; 2000];
        let romm = serve_files(server(&[(7, &data)], true, false)).await;
        let dir = TempDir::new("rom-no-ranges");
        let dest = dir.path().join("gba/game.gba");
        let transfer = Transfer::default();
        romm.download_content(7, "game.gba", None, &dest, Some(2000), &transfer)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(
            transfer.progress().done,
            2000,
            "the restart isn't counted twice"
        );
    }

    #[tokio::test]
    async fn a_download_of_the_wrong_size_is_refused() {
        let romm = serve_files(server(&[(7, b"short")], false, true)).await;
        let dir = TempDir::new("rom-wrong-size");
        let dest = dir.path().join("gba/game.gba");
        let err = romm
            .download_content(7, "game.gba", None, &dest, Some(99), &Transfer::default())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("RomM said 99"), "{err:#}");
        assert!(!dest.exists() && !with_suffix(&dest, ".part").exists());
    }

    #[tokio::test]
    async fn a_single_file_rom_of_the_wrong_size_is_downloaded_again() {
        let data = vec![5u8; 1200];
        let files = server(&[(66, &data)], false, true);
        let romm = serve_files(files.clone()).await;
        let rom: RomDetail = serde_json::from_value(serde_json::json!({
            "id": 66, "fs_name": "Game.rvz", "name": "Game", "platform_slug": "ngc",
            "platform_fs_slug": "ngc", "full_path": "roms/ngc/Game.rvz",
            "files": [{ "id": 66, "file_name": "Game.rvz", "file_size_bytes": 1200,
                        "file_path": "roms/ngc", "category": "game" }],
        }))
        .unwrap();
        let dir = TempDir::new("rom-single-size");
        let dest = dir.path().join("ngc/Game.rvz");

        crate::testing::write(&dest, &data);
        romm.download_rom_to_dir(&rom, dir.path(), &Transfer::default())
            .await
            .unwrap();
        assert!(
            files.log.lock().unwrap().is_empty(),
            "the right size is complete"
        );

        crate::testing::write(&dest, b"cut short by a copy");
        let launch = romm
            .download_rom_to_dir(&rom, dir.path(), &Transfer::default())
            .await
            .unwrap();
        assert_eq!(launch, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(files.log.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_cancelled_multi_file_download_leaves_no_folders_behind() {
        let romm = serve_stalling().await;
        let rom = multi_file_rom(&[
            (251, "Game.xci", "", 1000, "game"),
            (252, "Update.nsp", "update", 10, "update"),
        ]);
        let dir = TempDir::new("rom-cancel-dirs");
        let transfer = Transfer::default();
        let download = {
            let transfer = transfer.clone();
            let roms = dir.path().to_owned();
            tokio::spawn(async move { romm.download_rom_to_dir(&rom, &roms, &transfer).await })
        };
        while transfer.progress().done == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        transfer.cancel.cancel();
        assert!(download.await.unwrap().is_err());
        assert!(
            !dir.path().join("switch/Game").exists(),
            "no empty game folder"
        );
    }
}
