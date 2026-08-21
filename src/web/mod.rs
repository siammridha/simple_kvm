//! The HTTPS page server on port 3000: serves the page/JS/CSS (embedded
//! in the binary — no files to copy alongside it, matching the
//! single-binary install model). HTTPS (not plain HTTP) because browsers
//! only expose the `WebTransport` API on a secure context — see
//! `crate::tls` for the shared identity. Everything else — resolutions,
//! settings, cert bootstrap — is either embedded straight into the page or
//! carried over the WebTransport control stream (see
//! `webtransport::session`), so this server has no JSON API of its own.

use axum::extract::State;
use axum::http::header;
use axum::response::Html;
use axum::routing::get;
use axum::Router;

const INDEX_HTML: &str = include_str!("../../assets/web/index.html");
const APP_JS: &str = include_str!("../../assets/web/app.js");
const STYLE_CSS: &str = include_str!("../../assets/web/style.css");

/// The placeholder in `index.html` replaced with the page's bootstrap data
/// (see `index_handler`) — the WebTransport port and version can't come
/// over WebTransport itself, since the page needs them before it can open
/// that connection at all.
const SERVER_CONFIG_PLACEHOLDER: &str = "<!--SERVER_CONFIG-->";

#[derive(Clone)]
pub struct AppState {
    pub webtransport_port: u16,
    pub cert_hash: [u8; 32],
}

/// Every asset here is embedded in the binary itself, so the only way its
/// content ever changes is a new binary being installed - the browser must
/// never serve a cached copy from before an update, or the page can end up
/// running old JS against a new server (e.g. still showing removed
/// features).
const NO_CACHE: (header::HeaderName, &str) = (header::CACHE_CONTROL, "no-store");

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript"), NO_CACHE], APP_JS) }))
        .route("/style.css", get(|| async { ([(header::CONTENT_TYPE, "text/css"), NO_CACHE], STYLE_CSS) }))
        .with_state(state)
}

async fn index_handler(State(state): State<AppState>) -> ([(header::HeaderName, &'static str); 1], Html<String>) {
    let cert_hash_json = serde_json::to_string(&state.cert_hash).expect("byte array serialization can't fail");
    let config = format!(
        r#"<script>window.SERVER_CONFIG = {{"webtransportPort":{},"version":"{}","certHash":{}}};</script>"#,
        state.webtransport_port,
        env!("CARGO_PKG_VERSION"),
        cert_hash_json,
    );
    let page = INDEX_HTML.replacen(SERVER_CONFIG_PLACEHOLDER, &config, 1);
    ([NO_CACHE], Html(page))
}
