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

    /// Ensure the ROM is present in `roms_dir` and return the path to launch.
    ///
    /// - Single-file ROM → `roms_dir/{platform_fs_slug}/{fs_name}`
    /// Download a ROM and return the path to launch.
    ///
    /// - `fs_name` is not a zip → download directly, done.
    /// - `fs_name` is a zip, `files.len() <= 1` → download the zip as-is, extract.
    /// - `fs_name` is a zip, `files.len() > 1`  → bundle all files via RomM's
    ///   `file_ids` endpoint, extract (handles multi-disc / .cue+.bin sets).
    pub async fn download_rom_to_dir(&self, rom: &RomDetail, roms_dir: &Path) -> Result<PathBuf> {
        let platform_dir = roms_dir.join(&rom.platform_fs_slug);

        // Multi-file ROM stored as component files with no zip wrapper in RomM
        // (fs_name has no extension, files list is non-empty). RomM's content
        // endpoint bundles all component files into a zip when file_ids is
        // provided, so we use the same extract approach as zip ROMs.
        if rom.files.len() > 1 && !rom.fs_name.ends_with(".zip") {
            let stem = Path::new(&rom.fs_name)
                .file_stem()
                .unwrap_or_else(|| std::ffi::OsStr::new(&rom.fs_name))
                .to_string_lossy()
                .into_owned();
            let rom_dir = platform_dir.join(&stem);

            if rom_dir.is_dir() {
                if let Some(launch) = find_launch_file(&rom_dir).await? {
                    debug!("ROM cache hit (multi-file dir): {}", launch.display());
                    return Ok(launch);
                }
            }
            // If a stale single-file download sits at the directory path, remove it.
            if rom_dir.is_file() {
                tokio::fs::remove_file(&rom_dir).await.ok();
                debug!("removed stale single-file download at {}", rom_dir.display());
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
            self.download_content(rom.id, &rom.fs_name, Some(&file_ids), &zip_dest).await?;

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
            let dest = platform_dir.join(&rom.fs_name);
            if dest.is_file() {
                debug!("ROM cache hit: {}", dest.display());
                return Ok(dest);
            }
            // `dest` might collide with a directory left over from a prior
            // extraction. Look inside for a launchable file before re-downloading.
            if dest.is_dir() {
                if let Some(launch) = find_launch_file(&dest).await? {
                    debug!("ROM cache hit (extracted dir): {}", launch.display());
                    return Ok(launch);
                }
            }
            tokio::fs::create_dir_all(&platform_dir).await?;
            self.download_content(rom.id, &rom.fs_name, None, &dest).await?;
            return Ok(dest);
        }

        // fs_name is a zip — need to extract.
        let stem = Path::new(&rom.fs_name)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let rom_dir = platform_dir.join(&stem);

        if rom_dir.exists() {
            if let Some(launch) = find_launch_file(&rom_dir).await? {
                debug!("ROM cache hit: {}", launch.display());
                return Ok(launch);
            }
        }

        tokio::fs::create_dir_all(&platform_dir).await?;
        let zip_dest = platform_dir.join(&rom.fs_name);

        if rom.files.len() > 1 {
            // Multi-file zip: ask RomM to bundle everything into one zip.
            let file_ids = rom
                .files
                .iter()
                .map(|f| f.id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            self.download_content(rom.id, &rom.fs_name, Some(&file_ids), &zip_dest).await?;
        } else {
            // Single ROM stored in a zip — download fs_name directly.
            self.download_content(rom.id, &rom.fs_name, None, &zip_dest).await?;
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

    pub async fn upload_save(
        &self,
        rom_id: i64,
        emulator: &str,
        slot: Option<&str>,
        file_path: &Path,
    ) -> Result<()> {
        let token = self.tokens.get_token().await?;
        let file_name = file_path
            .file_name()
            .context("save file has no name")?
            .to_string_lossy()
            .into_owned();

        let file = tokio::fs::File::open(file_path)
            .await
            .with_context(|| format!("opening save {}", file_path.display()))?;
        let part = reqwest::multipart::Part::stream(reqwest::Body::wrap_stream(
            ReaderStream::new(file),
        ))
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

        let resp = req.send().await.context("POST /api/saves")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("save upload failed: {} — {}", status, body);
        }

        debug!("uploaded save {}", file_path.display());
        Ok(())
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
    ) -> Result<()> {
        let token = self.tokens.get_token().await?;
        let file_name = file_path
            .file_name()
            .context("state file has no name")?
            .to_string_lossy()
            .into_owned();

        let file = tokio::fs::File::open(file_path).await?;
        let part = reqwest::multipart::Part::stream(reqwest::Body::wrap_stream(
            ReaderStream::new(file),
        ))
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
        Ok(())
    }

    pub async fn download_state(&self, state_id: i64, dest_path: &Path) -> Result<()> {
        let bytes = self
            .authed_get(&format!("{}/api/states/{}/content", self.base_url, state_id))
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
        let bytes = req
            .send()
            .await
            .context("GET rom content")?
            .error_for_status()
            .context("ROM content download failed")?
            .bytes()
            .await?;

        write_file(dest, &bytes).await
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
        match path.extension().and_then(|e| e.to_str()) {
            Some("m3u") => m3u = Some(path),
            Some("cue") => cue = Some(path),
            Some("zip") => {} // skip inner zips
            _ => others.push(path),
        }
    }

    // Prefer playlist > cue sheet > lone file. .bin files alongside a .cue are
    // not launch targets on their own — the .cue references them.
    if let Some(m3u) = m3u { return Ok(Some(m3u)); }
    if let Some(cue) = cue { return Ok(Some(cue)); }
    if others.len() == 1 { return Ok(others.into_iter().next()); }
    Ok(None)
}

async fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<()> {
    let zip_path = zip_path.to_owned();
    let dest_dir = dest_dir.to_owned();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let file = std::fs::File::open(&zip_path)
            .with_context(|| format!("opening zip {}", zip_path.display()))?;
        let mut archive = zip::ZipArchive::new(file).context("reading zip archive")?;
        std::fs::create_dir_all(&dest_dir)?;
        archive.extract(&dest_dir).context("extracting zip")?;
        Ok(())
    })
    .await
    .context("zip extraction task panicked")?
}
