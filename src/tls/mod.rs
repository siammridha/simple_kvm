//! TLS identity, shared by both the WebTransport endpoint and the page's own
//! HTTPS listener (the page must be HTTPS for browsers to expose the
//! `WebTransport` API at all outside a `localhost` origin). Loaded once from
//! an operator-provided cert/key file pair and never rotated — that's on
//! whoever manages the file pair. The server does no certificate-hash
//! pinning of its own, so this needs to be a certificate chaining to a CA
//! the browser already trusts — a self-signed certificate does not work
//! even if manually added to the browser's trust store, since Chrome's
//! `WebTransport` connection verifies only against its built-in CA list
//! and ignores locally-added trust (confirmed by testing; regular page
//! loads over plain HTTPS aren't affected by this, only the WebTransport
//! connection itself).

use std::path::Path;

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use wtransport::Identity;

pub struct CertManager {
    identity: Identity,
}

impl CertManager {
    pub async fn start_from_files(cert_path: impl AsRef<Path>, key_path: impl AsRef<Path>) -> Result<Self> {
        let identity = Identity::load_pemfiles(cert_path.as_ref(), key_path.as_ref())
            .await
            .with_context(|| format!("loading TLS identity from {} / {}", cert_path.as_ref().display(), key_path.as_ref().display()))?;
        Ok(Self { identity })
    }

    /// A fresh owned copy for building a `wtransport::Endpoint`, which takes
    /// its identity by value.
    pub fn identity(&self) -> Identity {
        self.identity.clone_identity()
    }

    pub async fn https_config(&self) -> Result<RustlsConfig> {
        let (cert_der, key_der) = identity_der(&self.identity);
        RustlsConfig::from_der(cert_der, key_der).await.context("building HTTPS TLS config")
    }
}

fn identity_der(identity: &Identity) -> (Vec<Vec<u8>>, Vec<u8>) {
    let cert_der = identity.certificate_chain().as_slice().iter().map(|cert| cert.der().to_vec()).collect();
    let key_der = identity.private_key().secret_der().to_vec();
    (cert_der, key_der)
}
