//! Self-signed TLS identity generation and rotation. Chrome caps a
//! self-signed cert used with `serverCertificateHashes` at 14 days'
//! validity, so this regenerates well inside that window and publishes
//! the new identity for `webtransport::mod` (which must rebuild its
//! `Endpoint` on rotation — wtransport has no live-identity-swap API) and
//! `web::mod` (which serves the current hash for the page to fetch).
//!
//! Self-signed only, for now — swapping in a step-ca-issued identity later
//! is just replacing `generate()`'s body with `Identity::load_pemfiles`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::watch;
use wtransport::tls::Sha256DigestFmt;
use wtransport::Identity;

/// Comfortably inside Chrome's 14-day self-signed certificate cap.
const ROTATE_EVERY: Duration = Duration::from_secs(12 * 24 * 60 * 60);

pub struct CertManager {
    identity: watch::Receiver<Arc<Identity>>,
}

impl CertManager {
    pub fn start(subject_alt_names: Vec<String>) -> Result<Self> {
        let initial = generate(&subject_alt_names)?;
        let (tx, rx) = watch::channel(Arc::new(initial));
        tokio::spawn(rotate_forever(subject_alt_names, tx));
        Ok(Self { identity: rx })
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
