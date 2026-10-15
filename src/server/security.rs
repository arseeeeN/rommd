//! Who may talk to the daemon. rommd listens on localhost only, but any web
//! page in the user's browser can still send requests to `localhost:7497`, so
//! three checks keep other sites out:
//!
//! - **Origin**: a request carrying an `Origin` header (every cross-site
//!   request from a browser does) must come from the RomM server's own origin
//!   or a configured `server.allowed_origins` entry. CORS alone only stops a
//!   page from *reading* responses; "simple" cross-site requests such as a
//!   form POST would still reach the handlers.
//! - **CORS**: the same origins may read responses.
//! - **Host**: the `Host` header must name localhost. This defeats DNS
//!   rebinding, where a site points its own domain at 127.0.0.1 so that its
//!   requests count as same-origin and send no foreign `Origin`.
//!
//! Non-browser clients (scripts, launchers) send no `Origin` and are allowed.

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::warn;

use crate::config::Config;
use crate::live::Live;

/// Origins allowed to call the API from a browser, normalised to
/// `scheme://host[:port]`.
pub fn allowed_origins(config: &Config) -> Vec<String> {
    let romm = config.romm.as_ref().map(|r| r.base_url.as_str());
    romm.into_iter()
        .chain(config.server.allowed_origins.iter().map(String::as_str))
        .filter_map(|url| match reqwest::Url::parse(url) {
            Ok(u) => Some(u.origin().ascii_serialization()),
            Err(e) => {
                warn!("ignoring invalid origin {:?}: {}", url, e);
                None
            }
        })
        .collect()
}

/// CORS for the origins in the current config; follows config reloads.
pub fn cors_layer(live: Arc<Live>) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin: &HeaderValue, _| {
            origin
                .to_str()
                .is_ok_and(|o| live.get().origins.iter().any(|a| a == o))
        }))
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
}

pub async fn check_request(State(live): State<Arc<Live>>, req: Request, next: Next) -> Response {
    if !host_is_local(req.headers().get(header::HOST)) {
        return (
            StatusCode::FORBIDDEN,
            "rommd only answers requests addressed to localhost",
        )
            .into_response();
    }
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let allowed = origin
            .to_str()
            .is_ok_and(|o| live.get().origins.iter().any(|a| a == o));
        if !allowed {
            warn!("rejected request from origin {:?}", origin);
            return (
                StatusCode::FORBIDDEN,
                "origin not allowed; see server.allowed_origins",
            )
                .into_response();
        }
    }
    next.run(req).await
}

fn host_is_local(host: Option<&HeaderValue>) -> bool {
    let Some(host) = host.and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RommConfig;

    #[test]
    fn origins_come_from_romm_and_config() {
        let mut config = Config {
            romm: Some(RommConfig {
                base_url: "https://romm.example.com/".into(),
                api_token: None,
                username: None,
                password: None,
            }),
            ..Default::default()
        };
        config.server.allowed_origins = vec!["http://localhost:5173".into(), "not a url".into()];
        assert_eq!(
            allowed_origins(&config),
            ["https://romm.example.com", "http://localhost:5173"]
        );
    }

    #[test]
    fn only_localhost_hosts_pass() {
        let h = |v: &str| Some(HeaderValue::from_str(v).unwrap());
        for ok in [
            "localhost:7497",
            "127.0.0.1:7497",
            "[::1]:7497",
            "LOCALHOST",
            "127.0.0.1",
        ] {
            assert!(host_is_local(h(ok).as_ref()), "{ok}");
        }
        for bad in [
            "evil.example.com",
            "evil.example.com:7497",
            "127.0.0.1.evil.com:7497",
            "[::2]:7497",
        ] {
            assert!(!host_is_local(h(bad).as_ref()), "{bad}");
        }
        assert!(!host_is_local(None));
    }
}
