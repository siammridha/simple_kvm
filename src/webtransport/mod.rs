pub mod protocol;
pub mod session;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, watch};
use wtransport::endpoint::endpoint_side::Server;
use wtransport::endpoint::IncomingSession;
use wtransport::{Endpoint, Identity, ServerConfig};

use crate::ch9329::writer::SerialCommand;
use crate::config::CaptureSettings;
use crate::tls::CertManager;
use crate::video_bus;
use session::SessionContext;

/// Serves the WebTransport endpoint forever, rebuilding it whenever
/// `cert_manager` rotates the TLS identity (wtransport has no live
/// identity-swap API — active sessions on the old endpoint end when it's
/// dropped; the client is expected to reconnect, see `assets/web/app.js`).
pub async fn serve(
    port: u16,
    cert_manager: Arc<CertManager>,
    video_bus: video_bus::Receiver,
    serial_tx: mpsc::Sender<SerialCommand>,
    capture_settings_tx: watch::Sender<CaptureSettings>,
) -> Result<()> {
    let identity_rx = cert_manager.watch();

    loop {
        let identity = identity_rx.borrow().clone_identity();
        let endpoint = build_endpoint(port, identity)?;
        tracing::info!(port, "WebTransport endpoint listening");

        let mut rotated = identity_rx.clone();
        tokio::select! {
            result = accept_forever(&endpoint, &video_bus, &serial_tx, &capture_settings_tx) => return result,
            changed = rotated.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                tracing::info!("TLS certificate rotated, rebuilding WebTransport endpoint");
            }
        }
    }
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
) -> Result<()> {
    loop {
        let incoming = endpoint.accept().await;
        let ctx = SessionContext {
            video_bus: video_bus.clone(),
            serial_tx: serial_tx.clone(),
            capture_settings_tx: capture_settings_tx.clone(),
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
