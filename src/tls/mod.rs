//! TLS identity for the WebTransport endpoint (and, once it serves HTTPS
//! too, the plain page). Two sources:
//!
//! - Self-signed, auto-rotated (the default). Chrome caps a self-signed
//!   cert used with `serverCertificateHashes` at 14 days' validity, so
//!   this regenerates well inside that window and publishes the new
//!   identity for `webtransport::mod` (which must rebuild its `Endpoint`
//!   on rotation — wtransport has no live-identity-swap API) and
//!   `web::mod` (which serves the current hash for the page to fetch).
//! - Loaded once from a cert/key file pair (e.g. an operator-provided
//!   cert, or later a step-ca-issued one) and never rotated — rotating a
//!   file-provided identity would mean this process silently starts
//!   ignoring whatever the operator put on disk, which isn't ours to
//!   decide. Whoever manages that file pair owns its rotation.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
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
