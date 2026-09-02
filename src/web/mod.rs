//! The plain HTTP front door: the server itself, its port, the route
//! table, the page/JS/CSS (embedded in the binary — no files to copy
//! alongside it, matching the single-binary install model) and the
//! `/rtc/offer` signaling endpoint. Every handler here parses a request,
//! calls `crate::rtc`, and serializes what comes back — no SDP, media or
//! HID logic lives in this module (`ARCHITECTURE.md` §3.5).
//!
//! No TLS anywhere — WebRTC's own DTLS-SRTP handles encryption for the
//! video/input connection, generated fresh and automatically per session,
//! with nothing for an operator to provide or manage.

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::rtc;

const INDEX_HTML: &str = include_str!("../../assets/web/index.html");
const APP_JS: &str = include_str!("../../assets/web/app.js");
const STYLE_CSS: &str = include_str!("../../assets/web/style.css");

const PORT_ENV_VAR: &str = "HTTP_PORT";
const DEFAULT_PORT: u16 = 3000;

/// The placeholder in `index.html` replaced with the page's bootstrap data
/// (just the version, today — see `index_handler`).
const SERVER_CONFIG_PLACEHOLDER: &str = "<!--SERVER_CONFIG-->";

/// Every asset here is embedded in the binary itself, so the only way its
/// content ever changes is a new binary being installed - the browser must
/// never serve a cached copy from before an update, or the page can end up
/// running old JS against a new server (e.g. still showing removed
/// features).
const NO_CACHE: (header::HeaderName, &str) = (header::CACHE_CONTROL, "no-store");

/// Binds this module's own configured port and serves until the process
/// ends. Returns (after logging) rather than panicking if the port is
/// already taken, so a second instance fails visibly but quietly.
pub async fn serve(rtc: rtc::Rtc) {
    let port = configured_port();
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(%err, port, "failed to bind HTTP listener");
            return;
        }
    };
    tracing::info!(port, "server listening");
    if let Err(err) = axum::serve(listener, router(rtc)).await {
        tracing::error!(%err, "server exited");
    }
}

fn configured_port() -> u16 {
    std::env::var(PORT_ENV_VAR).ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_PORT)
}

fn router(rtc_state: rtc::Rtc) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript"), NO_CACHE], APP_JS) }))
        .route("/style.css", get(|| async { ([(header::CONTENT_TYPE, "text/css"), NO_CACHE], STYLE_CSS) }))
        .route("/rtc/offer", post(offer_handler))
        .with_state(rtc_state)
}

async fn index_handler() -> ([(header::HeaderName, &'static str); 1], Html<String>) {
    let config = format!(r#"<script>window.SERVER_CONFIG = {{"version":"{}"}};</script>"#, env!("CARGO_PKG_VERSION"));
    let page = INDEX_HTML.replacen(SERVER_CONFIG_PLACEHOLDER, &config, 1);
    ([NO_CACHE], Html(page))
}

#[derive(Deserialize)]
struct OfferRequest {
    sdp: String,
}

#[derive(Serialize)]
struct AnswerResponse {
    sdp: String,
}

/// `POST /rtc/offer`: the browser's entire signaling exchange in one round
/// trip (see `crate::rtc::Rtc::handle_offer`). Everything a failure can
/// mean is "that offer was no good", so every one of them is a 400.
async fn offer_handler(State(rtc): State<rtc::Rtc>, Json(body): Json<OfferRequest>) -> Result<Json<AnswerResponse>, StatusCode> {
    match rtc.handle_offer(body.sdp).await {
        Ok(answer_sdp) => Ok(Json(AnswerResponse { sdp: answer_sdp })),
        Err(err) => {
            tracing::warn!(%err, "failed to negotiate WebRTC session");
            Err(StatusCode::BAD_REQUEST)
        }
    }
}
