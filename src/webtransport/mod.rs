pub mod protocol;
pub mod session;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, watch};
use wtransport::endpoint::endpoint_side::Server;
use wtransport::endpoint::IncomingSession;
use wtransport::{Endpoint, Identity, ServerConfig};

use crate::ch9329::writer::SerialCommand;
use crate::config::{CaptureSettings, DeviceState, MouseMode};
use crate::tls::CertManager;
use crate::video_bus;
use session::SessionContext;

/// Serves the WebTransport endpoint forever. The TLS identity is fixed for
/// the process lifetime (see `tls::CertManager`), so the endpoint is built
/// once up front.
pub async fn serve(
    port: u16,
    cert_manager: Arc<CertManager>,
    video_bus: video_bus::Receiver,
    serial_tx: mpsc::Sender<SerialCommand>,
    capture_settings_tx: watch::Sender<CaptureSettings>,
    mouse_mode_tx: watch::Sender<MouseMode>,
    device_state_rx: watch::Receiver<DeviceState>,
    hid_connected_rx: watch::Receiver<bool>,
    settings_path: PathBuf,
) -> Result<()> {
    let endpoint = build_endpoint(port, cert_manager.identity())?;
    tracing::info!(port, "WebTransport endpoint listening");

    accept_forever(&endpoint, &video_bus, &serial_tx, &capture_settings_tx, &mouse_mode_tx, &device_state_rx, &hid_connected_rx, &settings_path).await
}

fn build_endpoint(port: u16, identity: Identity) -> Result<Endpoint<Server>> {
    let config = ServerConfig::builder()
        .with_bind_default(port)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .build();
    Ok(Endpoint::server(config)?)
}

async fn accept_forever(
    endpoint: &Endpoint<Server>,
    video_bus: &video_bus::Receiver,
    serial_tx: &mpsc::Sender<SerialCommand>,
    capture_settings_tx: &watch::Sender<CaptureSettings>,
    mouse_mode_tx: &watch::Sender<MouseMode>,
    device_state_rx: &watch::Receiver<DeviceState>,
    hid_connected_rx: &watch::Receiver<bool>,
    settings_path: &PathBuf,
) -> Result<()> {
    loop {
        let incoming = endpoint.accept().await;
        let ctx = SessionContext {
            video_bus: video_bus.clone(),
            serial_tx: serial_tx.clone(),
            capture_settings_tx: capture_settings_tx.clone(),
            capture_settings_rx: capture_settings_tx.subscribe(),
            mouse_mode_tx: mouse_mode_tx.clone(),
            mouse_mode_rx: mouse_mode_tx.subscribe(),
            device_state_rx: device_state_rx.clone(),
            hid_connected_rx: hid_connected_rx.clone(),
            settings_path: settings_path.clone(),
        };
        tokio::spawn(async move {
            if let Err(err) = handle_incoming(incoming, ctx).await {
                tracing::debug!(%err, "WebTransport session ended");
            }
        });
    }
}

async fn handle_incoming(incoming: IncomingSession, ctx: SessionContext) -> Result<()> {
    let session_request = incoming.await?;
    tracing::info!(path = session_request.path(), "new WebTransport session");
    let connection = session_request.accept().await?;
    session::handle(connection, ctx).await
}
