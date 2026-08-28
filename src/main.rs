mod capture;
mod config;
mod device;
mod hid;
mod rtc;
mod web;

use std::env;
use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

use capture::engine::CaptureEngine;
use config::MouseMode;
use device::{CaptureDevice, CaptureSettings, Resolution};
use hid::Hid;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    log_startup_banner(env!("CARGO_PKG_VERSION"));
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "simple_kvm starting");

    let http_port: u16 = env_parsed("HTTP_PORT").unwrap_or(3000);

    // --- Capture: the card is never opened automatically right here at
    // startup. Opening it unprompted has reliably crashed the real hardware
    // this targets right at boot (see README's "boot-crash" known issue) -
    // `Device<CaptureDriver>`'s presence task (spawned by `CaptureDevice::
    // spawn` below) deliberately never probes the very first time it finds
    // the device already present, for exactly that reason. Settings are
    // in-memory only - every start uses a fixed default; nothing here is
    // ever read from or written to disk. ---
    let default_capture_settings = CaptureSettings { resolution: Resolution { width: 1280, height: 720 }, fps: 5 };
    let default_mouse_mode = MouseMode::Absolute;

    let (capture_settings_tx, capture_settings_rx) = watch::channel(default_capture_settings);
    let (mouse_mode_tx, _mouse_mode_rx) = watch::channel(default_mouse_mode);

    // Two independent handles to the same underlying presence task (see
    // `Device::clone`) - one feeds `CaptureEngine`'s own presence-driven
    // `request_stream()` gating, the other publishes `DeviceState` for the
    // web UI. Neither ever sees the raw device path - the device reads it
    // from its own config; everything here only ever sees
    // presence/capability state.
    let capture_device = CaptureDevice::spawn();
    let device_state_rx = capture::watch_device_state(capture_device.clone(), capture_settings_rx.clone());
    let capture_engine = Arc::new(CaptureEngine::new(capture_device));

    // --- Serial: same soft-unavailable treatment as capture. `Hid` owns
    // its own device, queue, drain worker and enumeration-settle delay;
    // commands sent before its port is open queue up rather than being
    // lost, so nothing here holds up the HTTP page starting. ---
    let hid = Hid::spawn();

    let channels = rtc::SharedChannels::new(capture_engine, hid, capture_settings_tx, mouse_mode_tx, device_state_rx);
    let http_addr = std::net::SocketAddr::from(([0, 0, 0, 0], http_port));
    tracing::info!(port = http_port, "page and WebRTC signaling server listening");
    let http_handle = tokio::spawn(async move {
        let listener = match TcpListener::bind(http_addr).await {
            Ok(listener) => listener,
            Err(err) => {
                tracing::error!(%err, "failed to bind HTTP listener");
                return;
            }
        };
        if let Err(err) = axum::serve(listener, web::router(channels)).await {
            tracing::error!(%err, "page server exited");
        }
    });

    let _ = http_handle.await;
    Ok(())
}

fn env_parsed<T: std::str::FromStr>(key: &str) -> Option<T> {
    env::var(key).ok().and_then(|v| v.parse().ok())
}

/// Used only when `RUST_LOG` isn't set. Everything else stays at `info`,
/// but keystroke/click handling logs at `debug` by default so input-lag
/// reports are visible in the log immediately, with no configuration step
/// needed first - setting `RUST_LOG` explicitly still overrides this
/// entirely, same as any `EnvFilter`.
const DEFAULT_LOG_FILTER: &str = "info,simple_kvm::rtc::session=debug,simple_kvm::hid::writer=debug";

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// A loud, hand-colored banner printed straight to stdout (not through
/// `tracing`, so it isn't buried under a timestamp/level/target prefix like
/// every other line) - meant to jump out visually when scrolling past
/// hundreds of dense per-frame/per-packet debug lines to find where the
/// service actually (re)started.
fn log_startup_banner(version: &str) {
    let line = format!("simple_kvm v{version} — service started");
    let bar = "=".repeat(line.chars().count() + 4);
    println!("\x1b[1;32m{bar}\x1b[0m");
    println!("\x1b[1;32m  {line}\x1b[0m");
    println!("\x1b[1;32m{bar}\x1b[0m");
}
