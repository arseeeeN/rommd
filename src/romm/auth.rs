use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::debug;

use super::types::TokenResponse;

struct TokenState {
    access_token: String,
    refresh_token: String,
    expires_at: Instant,
    refresh_expires_at: Instant,
}

/// Manages authentication: static API token (no refresh needed) or OAuth2
/// password grant with transparent token refresh.
pub struct TokenManager {
    /// When set, returned as-is on every `get_token()` call. No OAuth2 flow.
    api_token: Option<String>,
    state: Arc<Mutex<Option<TokenState>>>,
    base_url: String,
    username: String,
    password: String,
    client: reqwest::Client,
}

impl TokenManager {
    pub fn new(
        base_url: impl Into<String>,
        api_token: Option<String>,
        username: impl Into<String>,
        password: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            api_token,
            state: Arc::new(Mutex::new(None)),
            base_url: base_url.into(),
            username: username.into(),
            password: password.into(),
            client,
        }
    }

    /// Return a valid bearer token, logging in or refreshing as needed.
    pub async fn get_token(&self) -> Result<String> {
        if let Some(token) = &self.api_token {
            return Ok(token.clone());
        }

        let mut guard = self.state.lock().await;
        let now = Instant::now();

        match &*guard {
            Some(s) if s.expires_at > now => {
                return Ok(s.access_token.clone());
            }
            Some(s) if s.refresh_expires_at > now => {
                let refresh_token = s.refresh_token.clone();
                drop(guard);
                let new_state = self.refresh(&refresh_token).await?;
                let token = new_state.access_token.clone();
                *self.state.lock().await = Some(new_state);
                return Ok(token);
            }
            _ => {}
        }

        let new_state = self.login().await?;
        let token = new_state.access_token.clone();
        *guard = Some(new_state);
        Ok(token)
    }

    async fn login(&self) -> Result<TokenState> {
        debug!("authenticating with RomM (password grant)");
        let resp = self
            .client
            .post(format!("{}/api/token", self.base_url))
            .form(&[
                ("grant_type", "password"),
                ("username", &self.username),
                ("password", &self.password),
            ])
            .send()
            .await
            .context("sending login request")?
            .error_for_status()
            .context("login failed")?
            .json::<TokenResponse>()
            .await
            .context("parsing token response")?;

        Ok(self.token_state(resp))
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenState> {
        debug!("refreshing RomM token");
        let resp = self
            .client
            .post(format!("{}/api/token", self.base_url))
            .form(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token)])
            .send()
            .await
            .context("sending refresh request")?
            .error_for_status()
            .context("token refresh failed")?
            .json::<TokenResponse>()
            .await
            .context("parsing refresh response")?;

        Ok(self.token_state(resp))
    }

    fn token_state(&self, resp: TokenResponse) -> TokenState {
        let now = Instant::now();
        let margin = Duration::from_secs(60);
        TokenState {
            access_token: resp.access_token,
            refresh_token: resp.refresh_token,
            expires_at: now + Duration::from_secs(resp.expires).saturating_sub(margin),
            refresh_expires_at: now
                + Duration::from_secs(resp.refresh_expires).saturating_sub(margin),
        }
    }
}
