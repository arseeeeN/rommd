//! SteamGridDB, where Steam ROM Manager gets its artwork, picked the way SRM
//! picks it by default (`src/core/artwork-providers.ts`): the first game the
//! title search finds, then per kind the first static image that is neither
//! NSFW nor humor, in SteamGridDB's own order.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const API: &str = "https://www.steamgriddb.com/api/v2";

/// The kinds of artwork a Steam shortcut has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The portrait capsule, `<id>p`.
    Tall,
    /// The wide capsule, `<id>`.
    Long,
    Hero,
    Logo,
    Icon,
}

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::Tall, Kind::Long, Kind::Hero, Kind::Logo, Kind::Icon];

    /// What follows the app ID in the file's name in Steam's `grid` folder.
    pub fn suffix(self) -> &'static str {
        match self {
            Kind::Tall => "p",
            Kind::Long => "",
            Kind::Hero => "_hero",
            Kind::Logo => "_logo",
            Kind::Icon => "_icon",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Kind::Tall | Kind::Long => "grids",
            Kind::Hero => "heroes",
            Kind::Logo => "logos",
            Kind::Icon => "icons",
        }
    }

    fn dimensions(self) -> Option<&'static str> {
        match self {
            Kind::Tall => Some("600x900"),
            Kind::Long => Some("460x215,920x430"),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct Response<T> {
    success: bool,
    data: Option<T>,
    #[serde(default)]
    errors: Vec<String>,
}

#[derive(Deserialize)]
struct GameHit {
    id: i64,
}

#[derive(Deserialize)]
struct Image {
    url: String,
}

pub struct Client {
    key: String,
    http: reqwest::Client,
}

impl Client {
    pub fn new(key: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("building the HTTP client");
        Self {
            key: key.to_owned(),
            http,
        }
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<Option<T>> {
        let res = self
            .http
            .get(format!("{API}{path}"))
            .bearer_auth(&self.key)
            .send()
            .await
            .context("SteamGridDB")?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            bail!("SteamGridDB refused [steam] steamgriddb_api_key");
        }
        let body: Response<T> = res
            .error_for_status()
            .context("SteamGridDB")?
            .json()
            .await
            .context("reading SteamGridDB's answer")?;
        if !body.success {
            bail!("SteamGridDB: {}", body.errors.join(", "));
        }
        Ok(body.data)
    }

    /// The first game SteamGridDB's search finds for `title`.
    pub async fn game(&self, title: &str) -> Result<Option<i64>> {
        let path = format!("/search/autocomplete/{}", urlencoding::encode(title));
        let hits: Option<Vec<GameHit>> = self.get(&path).await?;
        Ok(hits.and_then(|h| h.into_iter().next()).map(|g| g.id))
    }

    /// The URL of the first artwork of `kind` for game `id`.
    pub async fn artwork(&self, id: i64, kind: Kind) -> Result<Option<String>> {
        let mut path = format!(
            "/{}/game/{id}?types=static&nsfw=false&humor=false",
            kind.path()
        );
        if let Some(dimensions) = kind.dimensions() {
            path.push_str(&format!("&dimensions={dimensions}"));
        }
        let images: Option<Vec<Image>> = self.get(&path).await?;
        Ok(images.and_then(|i| i.into_iter().next()).map(|i| i.url))
    }

    pub async fn download(&self, url: &str) -> Result<Vec<u8>> {
        let bytes = self
            .http
            .get(url)
            .send()
            .await
            .context("downloading artwork")?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }
}

/// The file extension Steam needs for an image: it goes by the name, and
/// SteamGridDB serves PNG, JPEG, WebP and ICO.
pub fn extension(image: &[u8]) -> &'static str {
    if image.starts_with(b"\x89PNG") {
        "png"
    } else if image.starts_with(b"RIFF") && image.get(8..12) == Some(b"WEBP") {
        "webp"
    } else if image.starts_with(&[0, 0, 1, 0]) {
        "ico"
    } else {
        "jpg"
    }
}
