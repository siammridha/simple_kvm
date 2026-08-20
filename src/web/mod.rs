//! The HTTPS page server on port 3000: serves the page/JS/CSS (embedded
//! in the binary — no files to copy alongside it, matching the
//! single-binary install model) plus two small JSON endpoints the page
//! needs before it can open its WebTransport session: the resolutions the
//! capture card actually supports, and the current TLS certificate hash.
//! HTTPS (not plain HTTP) because browsers only expose the `WebTransport`
//! API on a secure context — see `crate::tls` for the shared identity.

use axum::extract::State;
use axum::http::header;
use axum::response::Html;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use std::sync::Arc;

use crate::tls::{self, CertManager};

const INDEX_HTML: &str = include_str!("../../assets/web/index.html");
const APP_JS: &str = include_str!("../../assets/web/app.js");
const STYLE_CSS: &str = include_str!("../../assets/web/style.css");

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ResolutionOption {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone)]
pub struct AppState {
    pub video_available: bool,
    pub resolutions: Arc<Vec<ResolutionOption>>,
    /// The resolution actually in effect for the first stream a session
    /// gets (see `CaptureManager::default_format_resolutions`) — the page
    /// pre-selects this in the dropdown rather than just assuming
    /// whichever resolution happens to be listed first.
    pub default_resolution: Option<ResolutionOption>,
    pub webtransport_port: u16,
    pub cert_manager: Arc<CertManager>,
}

#[derive(Serialize)]
struct ConfigResponse {
    video_available: bool,
    resolutions: Vec<ResolutionOption>,
    default_resolution: Option<ResolutionOption>,
    webtransport_port: u16,
}

#[derive(Serialize)]
struct CertInfoResponse {
    hash: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], APP_JS) }))
        .route("/style.css", get(|| async { ([(header::CONTENT_TYPE, "text/css")], STYLE_CSS) }))
        .route("/api/config", get(config_handler))
        .route("/api/cert-info", get(cert_info_handler))
        .with_state(state)
}

async fn config_handler(State(state): State<AppState>) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        video_available: state.video_available,
        resolutions: (*state.resolutions).clone(),
        default_resolution: state.default_resolution,
        webtransport_port: state.webtransport_port,
    })
}

async fn cert_info_handler(State(state): State<AppState>) -> Json<CertInfoResponse> {
    let identity = state.cert_manager.current();
    Json(CertInfoResponse { hash: tls::cert_hash(&identity) })
}
