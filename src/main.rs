mod capture;
mod ch9329;
mod config;
mod settings_store;
mod tls;
mod uevent;
mod video_bus;
mod web;
mod webtransport;

use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

use capture::v4l2::Resolution;
use capture::CaptureManager;
use ch9329::writer::{self, SerialCommand};
use config::{CaptureSettings, MouseMode, VideoMode};

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "simple_kvm starting");

    // axum-server's rustls integration needs a process-default crypto
    // provider installed; wtransport builds its own provider explicitly
    // per-config and doesn't need or set this, so there's no conflict.
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("installing rustls crypto provider"))?;

    let serial_path = env::var("SERIAL_PATH").unwrap_or_else(|_| "/dev/ttyUSB0".to_string());
    let video_path = env::var("VIDEO_PATH").unwrap_or_else(|_| "/dev/video0".to_string());
    let http_port: u16 = env_parsed("HTTP_PORT").unwrap_or(3000);
    let webtransport_port: u16 = env_parsed("WEBTRANSPORT_PORT").unwrap_or(4433);
    let tls_sans: Vec<String> = env::var("TLS_SAN")
        .unwrap_or_else(|_| "localhost".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    let tls_cert_path = env::var("TLS_CERT_PATH").ok();
    let tls_key_path = env::var("TLS_KEY_PATH").ok();
    let settings_path = PathBuf::from(env::var("SETTINGS_PATH").unwrap_or_else(|_| "/etc/simple_kvm-settings.json".to_string()));
    let serial_open_delay_secs: u64 = env_parsed("SERIAL_OPEN_DELAY_SECS").unwrap_or(30);

    // --- Capture: probing never fails — an absent card just means video
    // stays unavailable (soft "no device" state), so the rest of the
    // server still starts and the page still loads. ---
    let capture_manager = CaptureManager::probe(&video_path);

    // A settings file from a previous run wins over the capture card's own
    // default, but only if the card on hand can actually still run it —
    // it may have changed since the setting was saved.
    let persisted = settings_store::load(&settings_path);
    let persisted_capture = persisted.filter(|p| capture_manager.supports(p.capture.video_mode, p.capture.resolution)).map(|p| p.capture);
    let default_capture_settings = persisted_capture.or_else(|| capture_manager.default_settings()).unwrap_or(CaptureSettings {
        video_mode: VideoMode::Mjpeg,
        resolution: Resolution { width: 1280, height: 720 },
        fps: 5,
    });
    let default_mouse_mode = persisted.map(|p| p.mouse_mode).unwrap_or(MouseMode::Absolute);

    let (device_state_tx, device_state_rx) = watch::channel(capture_manager.device_state(&default_capture_settings));
    let (capture_settings_tx, capture_settings_rx) = watch::channel(default_capture_settings);
    let (mouse_mode_tx, mouse_mode_rx) = watch::channel(default_mouse_mode);
    let (video_bus_tx, video_bus_rx) = video_bus::channel();

    // --- Serial: same soft-unavailable treatment as capture. Commands sent
    // to `serial_tx` before the port is open just queue up in the channel,
    // so this delay doesn't hold up the HTTP page or WebTransport server
    // starting. ---
    let (serial_tx, serial_rx) = mpsc::channel::<SerialCommand>(256);
    tokio::spawn(open_serial_after_delay(serial_path, serial_open_delay_secs, serial_tx.clone(), serial_rx));

    let cert_manager = Arc::new(match (tls_cert_path, tls_key_path) {
        (Some(cert_path), Some(key_path)) => {
            tracing::info!(cert_path, key_path, "loading TLS identity from files (no auto-rotation)");
            tls::CertManager::start_from_files(cert_path, key_path).await?
        }
        _ => tls::CertManager::start_self_signed(tls_sans)?,
    });

    let app_state = web::AppState {
        device_state_rx: device_state_rx.clone(),
        webtransport_port,
        cert_manager: Arc::clone(&cert_manager),
        video_mode: default_capture_settings.video_mode,
        fps: default_capture_settings.fps,
        mouse_mode: default_mouse_mode,
        capture_settings_rx: capture_settings_rx.clone(),
        mouse_mode_rx,
        settings_path,
    };
    let https_config = cert_manager.https_config().await?;
    let http_addr = std::net::SocketAddr::from(([0, 0, 0, 0], http_port));
    tracing::info!(port = http_port, "HTTPS page server listening");
    let http_handle = tokio::spawn(async move {
        if let Err(err) = axum_server::bind_rustls(http_addr, https_config).serve(web::router(app_state).into_make_service()).await {
            tracing::error!(%err, "HTTPS page server exited");
        }
    });

    let capture_handle = tokio::spawn(capture_manager.run(capture_settings_rx, video_bus_tx, device_state_tx));

    let webtransport_handle = tokio::spawn(async move {
        if let Err(err) = webtransport::serve(webtransport_port, cert_manager, video_bus_rx, serial_tx, capture_settings_tx, mouse_mode_tx, device_state_rx).await {
            tracing::error!(%err, "WebTransport server exited");
        }
    });

    let _ = tokio::join!(http_handle, capture_handle, webtransport_handle);
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
async fn open_serial_after_delay(serial_path: String, delay_secs: u64, serial_tx: mpsc::Sender<SerialCommand>, serial_rx: mpsc::Receiver<SerialCommand>) {
    if delay_secs > 0 {
        tracing::info!(seconds = delay_secs, "waiting before opening CH9329 serial port");
        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
    }
    tokio::spawn(writer::watch_connection(serial_tx));
    let writer = writer::SerialWriter::new(serial_path);
    let _ = tokio::task::spawn_blocking(move || writer.run(serial_rx)).await;
}

/// Used only when `RUST_LOG` isn't set. Everything else stays at `info`,
/// but keystroke/click handling logs at `debug` by default so input-lag
/// reports are visible in the log immediately, with no configuration step
/// needed first - setting `RUST_LOG` explicitly still overrides this
/// entirely, same as any `EnvFilter`.
const DEFAULT_LOG_FILTER: &str = "info,simple_kvm::webtransport::session=debug,simple_kvm::ch9329::writer=debug";

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
