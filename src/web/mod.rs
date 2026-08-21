//! The plain HTTP page server on port `HTTP_PORT`: serves the page/JS/CSS
//! (embedded in the binary — no files to copy alongside it, matching the
//! single-binary install model) and the `/rtc/offer` WebRTC signaling
//! route (see `crate::rtc`). No TLS anywhere — WebRTC's own DTLS-SRTP
//! handles encryption for the video/input connection, generated fresh and
//! automatically per session, with nothing for an operator to provide or
//! manage.

use axum::http::header;
use axum::response::Html;
use axum::routing::{get, post};
use axum::Router;

use crate::rtc;

const INDEX_HTML: &str = include_str!("../../assets/web/index.html");
const APP_JS: &str = include_str!("../../assets/web/app.js");
const STYLE_CSS: &str = include_str!("../../assets/web/style.css");

/// The placeholder in `index.html` replaced with the page's bootstrap data
/// (just the version, today — see `index_handler`).
const SERVER_CONFIG_PLACEHOLDER: &str = "<!--SERVER_CONFIG-->";

/// Every asset here is embedded in the binary itself, so the only way its
/// content ever changes is a new binary being installed - the browser must
/// never serve a cached copy from before an update, or the page can end up
/// running old JS against a new server (e.g. still showing removed
/// features).
const NO_CACHE: (header::HeaderName, &str) = (header::CACHE_CONTROL, "no-store");

pub fn router(channels: rtc::SharedChannels) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript"), NO_CACHE], APP_JS) }))
        .route("/style.css", get(|| async { ([(header::CONTENT_TYPE, "text/css"), NO_CACHE], STYLE_CSS) }))
        .route("/rtc/offer", post(rtc::offer_handler))
        .with_state(channels)
}

async fn index_handler() -> ([(header::HeaderName, &'static str); 1], Html<String>) {
    let config = format!(r#"<script>window.SERVER_CONFIG = {{"version":"{}"}};</script>"#, env!("CARGO_PKG_VERSION"));
    let page = INDEX_HTML.replacen(SERVER_CONFIG_PLACEHOLDER, &config, 1);
    ([NO_CACHE], Html(page))
}
