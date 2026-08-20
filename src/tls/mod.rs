//! TLS identity, shared by both the WebTransport endpoint and the page's own
//! HTTPS listener (the page must be HTTPS for browsers to expose the
//! `WebTransport` API at all outside a `localhost` origin). Two sources:
//!
//! - Self-signed, auto-rotated (the default). Chrome caps a self-signed
//!   cert used with `serverCertificateHashes` at 14 days' validity, so
//!   this regenerates well inside that window and publishes the new
//!   identity for `webtransport::mod` (which must rebuild its `Endpoint`
//!   on rotation — wtransport has no live-identity-swap API), `web::mod`
//!   (which serves the current hash for the page to fetch), and this
//!   module's own `https_config` (which reloads the page's `RustlsConfig`
//!   in place, no listener restart needed).
//! - Loaded once from a cert/key file pair (e.g. an operator-provided
//!   cert, or later a step-ca-issued one) and never rotated — rotating a
//!   file-provided identity would mean this process silently starts
//!   ignoring whatever the operator put on disk, which isn't ours to
//!   decide. Whoever manages that file pair owns its rotation.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use tokio::sync::watch;
use wtransport::tls::Sha256DigestFmt;
use wtransport::Identity;

/// Comfortably inside Chrome's 14-day self-signed certificate cap.
const ROTATE_EVERY: Duration = Duration::from_secs(12 * 24 * 60 * 60);

pub struct CertManager {
    identity: watch::Receiver<Arc<Identity>>,
    /// Kept alive so `identity`'s `changed()` blocks forever rather than
    /// erroring once there's no more rotation to wait for (the static
    /// file-loaded case never sends again, but must still hold the
    /// sender open).
    _sender: watch::Sender<Arc<Identity>>,
}

impl CertManager {
    pub fn start_self_signed(subject_alt_names: Vec<String>) -> Result<Self> {
        let initial = generate(&subject_alt_names)?;
        let (tx, rx) = watch::channel(Arc::new(initial));
        tokio::spawn(rotate_forever(subject_alt_names, tx.clone()));
        Ok(Self { identity: rx, _sender: tx })
    }

    /// Loads a fixed identity from a cert/key PEM pair and never rotates
    /// it.
    pub async fn start_from_files(cert_path: impl AsRef<Path>, key_path: impl AsRef<Path>) -> Result<Self> {
        let identity = Identity::load_pemfiles(cert_path.as_ref(), key_path.as_ref())
            .await
            .with_context(|| format!("loading TLS identity from {} / {}", cert_path.as_ref().display(), key_path.as_ref().display()))?;
        let (tx, rx) = watch::channel(Arc::new(identity));
        Ok(Self { identity: rx, _sender: tx })
    }

    pub fn current(&self) -> Arc<Identity> {
        self.identity.borrow().clone()
    }

    pub fn watch(&self) -> watch::Receiver<Arc<Identity>> {
        self.identity.clone()
    }

    /// Builds the `RustlsConfig` the page's HTTPS listener serves, from the
    /// current identity, and spawns a task that reloads it in place on
    /// rotation. For a file-loaded identity the watch channel never fires
    /// again, so the task just idles forever — nothing to reload.
    pub async fn https_config(&self) -> Result<RustlsConfig> {
        let (cert_der, key_der) = identity_der(&self.current());
        let config = RustlsConfig::from_der(cert_der, key_der).await.context("building HTTPS TLS config")?;

        let mut rx = self.watch();
        let reload_target = config.clone();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let identity = rx.borrow_and_update().clone();
                let (cert_der, key_der) = identity_der(&identity);
                match reload_target.reload_from_der(cert_der, key_der).await {
                    Ok(()) => tracing::info!("reloaded HTTPS certificate"),
                    Err(err) => tracing::error!(%err, "failed to reload HTTPS certificate, keeping the previous one"),
                }
            }
        });

        Ok(config)
    }
}

fn identity_der(identity: &Identity) -> (Vec<Vec<u8>>, Vec<u8>) {
    let cert_der = identity.certificate_chain().as_slice().iter().map(|cert| cert.der().to_vec()).collect();
    let key_der = identity.private_key().secret_der().to_vec();
    (cert_der, key_der)
}

fn generate(subject_alt_names: &[String]) -> Result<Identity> {
    Identity::self_signed(subject_alt_names).map_err(|err| anyhow!("generating self-signed certificate: {err:?}"))
}

async fn rotate_forever(subject_alt_names: Vec<String>, tx: watch::Sender<Arc<Identity>>) {
    loop {
        tokio::time::sleep(ROTATE_EVERY).await;
        match generate(&subject_alt_names) {
            Ok(identity) => {
                tracing::info!("rotated self-signed TLS certificate");
                let _ = tx.send(Arc::new(identity));
            }
            Err(err) => tracing::error!(%err, "failed to rotate TLS certificate, keeping the previous one"),
        }
    }
}

/// The current certificate's SHA-256 hash, formatted the way the
/// browser's `serverCertificateHashes` option expects.
pub fn cert_hash(identity: &Identity) -> String {
    identity.certificate_chain().as_slice()[0].hash().fmt(Sha256DigestFmt::BytesArray)
}
