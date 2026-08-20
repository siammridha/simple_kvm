mod capture;
mod ch9329;
mod config;
mod tls;
mod video_bus;
mod web;
mod webtransport;

use std::env;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

use capture::v4l2::Resolution;
use capture::CaptureManager;
use ch9329::writer::{self, SerialCommand};
use config::{CaptureSettings, VideoMode};

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    tracing::info!("simple_kvm starting");

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

    // --- Capture: probing never fails — an absent card just means video
    // stays unavailable (soft "no device" state), so the rest of the
    // server still starts and the page still loads. ---
    let capture_manager = CaptureManager::probe(&video_path);
    let video_available = capture_manager.is_available();
    let resolutions: Vec<web::ResolutionOption> =
        capture_manager.default_format_resolutions().iter().map(|r| web::ResolutionOption { width: r.width, height: r.height }).collect();
    // Both derived from the same `pick_default` call, so the dropdown's
    // pre-selected value always matches what the first stream actually
    // uses (see `default_format_resolutions`'s doc comment).
    let default_settings = capture_manager.default_settings();
    let default_resolution = default_settings.map(|s| web::ResolutionOption { width: s.resolution.width, height: s.resolution.height });
    let default_capture_settings = default_settings.unwrap_or(CaptureSettings {
        video_mode: VideoMode::Mjpeg,
        resolution: Resolution { width: 1280, height: 720 },
    });
    let (capture_settings_tx, capture_settings_rx) = watch::channel(default_capture_settings);
    let (video_bus_tx, video_bus_rx) = video_bus::channel();

    // --- Serial: same soft-unavailable treatment as capture. ---
    let (serial_tx, serial_rx) = mpsc::channel::<SerialCommand>(256);
    match writer::open(&serial_path) {
        Ok(Some(port)) => {
            tracing::info!(serial_path, "opened CH9329 serial port");
            let writer = writer::SerialWriter::new(port);
            tokio::task::spawn_blocking(move || writer.run(serial_rx));
        }
        Ok(None) => {
            tracing::warn!(serial_path, "no CH9329 serial device found, input will be a no-op");
            tokio::spawn(drain_serial_commands(serial_rx));
        }
        Err(err) => {
            tracing::error!(%err, serial_path, "failed to open CH9329 serial port, input will be a no-op");
            tokio::spawn(drain_serial_commands(serial_rx));
        }
    }

    let cert_manager = Arc::new(match (tls_cert_path, tls_key_path) {
        (Some(cert_path), Some(key_path)) => {
            tracing::info!(cert_path, key_path, "loading TLS identity from files (no auto-rotation)");
            tls::CertManager::start_from_files(cert_path, key_path).await?
        }
        _ => tls::CertManager::start_self_signed(tls_sans)?,
    });

    let app_state = web::AppState {
        video_available,
        resolutions: Arc::new(resolutions),
        default_resolution,
        webtransport_port,
        cert_manager: Arc::clone(&cert_manager),
    };
    let https_config = cert_manager.https_config().await?;
    let http_addr = std::net::SocketAddr::from(([0, 0, 0, 0], http_port));
    tracing::info!(port = http_port, "HTTPS page server listening");
    let http_handle = tokio::spawn(async move {
        if let Err(err) = axum_server::bind_rustls(http_addr, https_config).serve(web::router(app_state).into_make_service()).await {
            tracing::error!(%err, "HTTPS page server exited");
        }
    });

    let capture_handle = tokio::spawn(capture_manager.run(capture_settings_rx, video_bus_tx));

    let webtransport_handle = tokio::spawn(async move {
        if let Err(err) = webtransport::serve(webtransport_port, cert_manager, video_bus_rx, serial_tx, capture_settings_tx).await {
            tracing::error!(%err, "WebTransport server exited");
        }
    });

    let _ = tokio::join!(http_handle, capture_handle, webtransport_handle);
    Ok(())
}

fn env_parsed<T: std::str::FromStr>(key: &str) -> Option<T> {
    env::var(key).ok().and_then(|v| v.parse().ok())
}

async fn drain_serial_commands(mut rx: mpsc::Receiver<SerialCommand>) {
    while rx.recv().await.is_some() {
        // No CH9329 attached: silently discard input commands.
    }
}

fn init_logging() {
    let filter = EnvFilter::builder()
        .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
