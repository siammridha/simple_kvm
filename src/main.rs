mod capture;
mod ch9329;
mod config;
mod rtc;
mod uevent;
mod video_bus;
mod web;

use std::env;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

use capture::v4l2::Resolution;
use capture::CaptureManager;
use ch9329::writer::{self, SerialCommand};
use config::{CaptureSettings, MouseMode};

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    log_startup_banner(env!("CARGO_PKG_VERSION"));
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "simple_kvm starting");

    let serial_path = env::var("SERIAL_PATH").unwrap_or_else(|_| "/dev/ttyUSB0".to_string());
    let video_path = env::var("VIDEO_PATH").unwrap_or_else(|_| "/dev/video0".to_string());
    let http_port: u16 = env_parsed("HTTP_PORT").unwrap_or(3000);
    let serial_open_delay_secs: u64 = env_parsed("SERIAL_OPEN_DELAY_SECS").unwrap_or(30);

    // --- Capture: probing never fails — an absent card just means video
    // stays unavailable (soft "no device" state), so the rest of the
    // server still starts and the page still loads. ---
    let capture_manager = CaptureManager::probe(&video_path);

    // Settings are in-memory only - every start uses the capture card's own
    // probed default, falling back to a fixed value if there's no card to
    // probe. Nothing here is ever read from or written to disk.
    let default_capture_settings = capture_manager.default_settings().unwrap_or(CaptureSettings {
        resolution: Resolution { width: 1280, height: 720 },
        fps: 5,
    });
    let default_mouse_mode = MouseMode::Absolute;

    let (device_state_tx, device_state_rx) = watch::channel(capture_manager.device_state(&default_capture_settings));
    let (capture_settings_tx, capture_settings_rx) = watch::channel(default_capture_settings);
    let (mouse_mode_tx, _mouse_mode_rx) = watch::channel(default_mouse_mode);
    let (video_bus_tx, video_bus_rx) = video_bus::channel();
    let (hid_connected_tx, hid_connected_rx) = watch::channel(false);
    let force_keyframe = Arc::new(AtomicBool::new(false));
    let (client_count_tx, client_count_rx) = watch::channel(0u32);

    // --- Serial: same soft-unavailable treatment as capture. Commands sent
    // to `serial_tx` before the port is open just queue up in the channel,
    // so this delay doesn't hold up the HTTP page starting. ---
    let (serial_tx, serial_rx) = mpsc::channel::<SerialCommand>(256);
    tokio::spawn(open_serial_after_delay(serial_path, serial_open_delay_secs, serial_tx.clone(), serial_rx, hid_connected_tx));

    let channels = rtc::SharedChannels {
        video_bus: video_bus_rx,
        serial_tx,
        capture_settings_tx,
        mouse_mode_tx,
        device_state_rx,
        hid_connected_rx,
        force_keyframe: force_keyframe.clone(),
        client_count_tx,
    };
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

    let capture_handle = tokio::spawn(capture_manager.run(capture_settings_rx, video_bus_tx, device_state_tx, force_keyframe, client_count_rx));

    let _ = tokio::join!(http_handle, capture_handle);
    Ok(())
}

fn env_parsed<T: std::str::FromStr>(key: &str) -> Option<T> {
    env::var(key).ok().and_then(|v| v.parse().ok())
}

/// Waits `delay_secs` (giving the CH9329's USB enumeration time to settle,
/// the same crash-avoidance reasoning as the capture card's boot delay —
/// see `deploy/install.sh`), then runs the writer loop. The writer itself
/// checks whether the CH9329 is actually plugged in before every command,
/// so it's a no-op whenever the device isn't there and picks back up on
/// its own once it is — no need to decide that once, up front, here.
async fn open_serial_after_delay(
    serial_path: String,
    delay_secs: u64,
    serial_tx: mpsc::Sender<SerialCommand>,
    serial_rx: mpsc::Receiver<SerialCommand>,
    hid_connected_tx: watch::Sender<bool>,
) {
    if delay_secs > 0 {
        tracing::info!(seconds = delay_secs, "waiting before opening CH9329 serial port");
        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
    }
    tokio::spawn(writer::watch_connection(serial_tx));
    let writer = writer::SerialWriter::new(serial_path, hid_connected_tx);
    let _ = tokio::task::spawn_blocking(move || writer.run(serial_rx)).await;
}

/// Used only when `RUST_LOG` isn't set. Everything else stays at `info`,
/// but keystroke/click handling logs at `debug` by default so input-lag
/// reports are visible in the log immediately, with no configuration step
/// needed first - setting `RUST_LOG` explicitly still overrides this
/// entirely, same as any `EnvFilter`.
const DEFAULT_LOG_FILTER: &str = "info,simple_kvm::rtc::session=debug,simple_kvm::ch9329::writer=debug";

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
