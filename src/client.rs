//! A client of the running daemon, for `rommd search` / `rommd play` and the
//! tray: the same HTTP API anything else uses.

use anyhow::{Context, Result};
use std::time::Duration;

use crate::config::{Config, ServerConfig};
use crate::launches::LaunchInfo;
use crate::profile::ProfileBlob;
use crate::recent::RecentGame;
use crate::server::types::{GamesResponse, StatusResponse};
use crate::update::UpdateStatus;

/// What `Daemon::launch` got: the game started (its PID), or its ROM is
/// downloading (the launch's ID to follow).
pub enum Launched {
    Started(u64),
    Downloading(u64),
}

pub struct Daemon {
    client: reqwest::Client,
    base: String,
}

impl Daemon {
    pub fn new() -> Result<Self> {
        let port = Config::load_or_create().map_or(ServerConfig::default().port, |c| c.server.port);
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .build()?,
            base: format!("http://127.0.0.1:{port}"),
        })
    }

    pub async fn games(&self, query: &str, limit: u32) -> Result<GamesResponse> {
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
    pub async fn launch(
        &self,
        rom_id: i64,
        emulator: &str,
        attach: bool,
        fullscreen: Option<bool>,
        profile: Option<&ProfileBlob>,
    ) -> Result<Launched> {
        let request = self
            .client
            .post(format!("{}/launch", self.base))
            .json(&serde_json::json!({
                "rom_id": rom_id,
                "emulator": emulator,
                "attach": attach,
                "fullscreen": fullscreen,
                "profile": profile,
            }));
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

    /// Running games and launches in progress.
    pub async fn status(&self) -> Result<StatusResponse> {
        self.get_json("/status").await
    }

    /// The games started last, newest first.
    pub async fn recent(&self) -> Result<Vec<RecentGame>> {
        self.get_json("/recent").await
    }

    /// The daemon's last update check, or with `check` a new one.
    pub async fn update(&self, check: bool) -> Result<UpdateStatus> {
        let path = if check {
            "/update?check=true"
        } else {
            "/update"
        };
        self.get_json(path).await
    }

    /// Install the update the last check found; the daemon restarts.
    pub async fn install_update(&self) -> Result<()> {
        let request = self
            .client
            .post(format!("{}/update", self.base))
            .timeout(Duration::from_secs(300));
        self.send(request).await.map(|_| ())
    }

    /// Stop the game with this PID, politely first.
    pub async fn stop(&self, pid: u32) -> Result<()> {
        let request = self
            .client
            .post(format!("{}/stop", self.base))
            .json(&serde_json::json!({ "pid": pid }))
            .timeout(Duration::from_secs(30));
        self.send(request).await.map(|_| ())
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let request = self
            .client
            .get(format!("{}{path}", self.base))
            .timeout(Duration::from_secs(10));
        Ok(self.send(request).await?.json().await?)
    }

    pub async fn launch_info(&self, id: u64) -> Result<LaunchInfo> {
        let request = self
            .client
            .get(format!("{}/launches/{id}", self.base))
            .timeout(Duration::from_secs(5));
        Ok(self.send(request).await?.json().await?)
    }

    /// Report on an attached launch: the game's PID, or why it didn't start.
    pub async fn attached(&self, id: u64, started: std::result::Result<u32, String>) -> Result<()> {
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
    pub async fn cancel(&self, id: u64) -> Result<bool> {
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
