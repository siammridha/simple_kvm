//! The HTTPS page server on port 3000: serves the page/JS/CSS (embedded
//! in the binary — no files to copy alongside it, matching the
//! single-binary install model) plus the small JSON endpoints the page
//! needs: the resolutions the capture card actually supports, the current
//! TLS certificate hash, and (on demand, from the Save button) writing the
//! current settings to disk. HTTPS (not plain HTTP) because browsers only
//! expose the `WebTransport` API on a secure context — see `crate::tls`
//! for the shared identity.

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

use crate::config::{CaptureSettings, MouseMode, PersistedSettings, VideoMode};
use crate::settings_store;
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
    /// The video/mouse mode the server actually started with — either a
    /// value loaded from the settings file by `settings_store`, or the
    /// capture card's own default. The page uses this to pre-select its
    /// dropdowns to match reality instead of always defaulting to
    /// whichever `<option>` happens to be listed first.
    pub video_mode: VideoMode,
    pub mouse_mode: MouseMode,
    /// Live settings, read at save time (not just startup) so `POST
    /// /api/settings/save` always writes whatever's actually in effect
    /// right now, including changes made since the page loaded.
    pub capture_settings_rx: watch::Receiver<CaptureSettings>,
    pub mouse_mode_rx: watch::Receiver<MouseMode>,
    pub settings_path: PathBuf,
}

#[derive(Serialize)]
struct ConfigResponse {
    video_available: bool,
    resolutions: Vec<ResolutionOption>,
    default_resolution: Option<ResolutionOption>,
    webtransport_port: u16,
    video_mode: VideoMode,
    mouse_mode: MouseMode,
    version: &'static str,
}

#[derive(Serialize)]
struct CertInfoResponse {
    hash: String,
}

/// Every asset here is embedded in the binary itself, so the only way its
/// content ever changes is a new binary being installed - the browser must
/// never serve a cached copy from before an update, or the page can end up
/// running old JS against a new server (e.g. still showing removed
/// features).
const NO_CACHE: (header::HeaderName, &str) = (header::CACHE_CONTROL, "no-store");

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { ([NO_CACHE], Html(INDEX_HTML)) }))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript"), NO_CACHE], APP_JS) }))
        .route("/style.css", get(|| async { ([(header::CONTENT_TYPE, "text/css"), NO_CACHE], STYLE_CSS) }))
        .route("/api/config", get(config_handler))
        .route("/api/cert-info", get(cert_info_handler))
        .route("/api/settings/save", post(save_settings_handler))
        .with_state(state)
}

async fn config_handler(State(state): State<AppState>) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        video_available: state.video_available,
        resolutions: (*state.resolutions).clone(),
        default_resolution: state.default_resolution,
        webtransport_port: state.webtransport_port,
        video_mode: state.video_mode,
        mouse_mode: state.mouse_mode,
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn cert_info_handler(State(state): State<AppState>) -> Json<CertInfoResponse> {
    let identity = state.cert_manager.current();
    Json(CertInfoResponse { hash: tls::cert_hash(&identity) })
}

/// Writes whatever video mode/resolution/mouse mode are in effect right
/// now to the settings file — the only way the file gets written, since
/// dropdown changes themselves no longer save automatically (see
/// `settings_store`).
async fn save_settings_handler(State(state): State<AppState>) -> StatusCode {
    let settings = PersistedSettings { capture: *state.capture_settings_rx.borrow(), mouse_mode: *state.mouse_mode_rx.borrow() };
    let path = state.settings_path.clone();
    match tokio::task::spawn_blocking(move || settings_store::save(&path, settings)).await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(err) => {
            tracing::error!(%err, "settings save task panicked");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
