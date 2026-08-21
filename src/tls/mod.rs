//! TLS identity, shared by both the WebTransport endpoint and the page's own
//! HTTPS listener (the page must be HTTPS for browsers to expose the
//! `WebTransport` API at all outside a `localhost` origin). Loaded once from
//! an operator-provided cert/key file pair and never rotated — that's on
//! whoever manages the file pair. The WebTransport connection pins itself
//! to this certificate's exact SHA-256 hash (`serverCertificateHashes`,
//! see `cert_hash` below and `assets/web/app.js`) rather than relying on it
//! chaining to a CA the browser trusts, so the cert must be valid for 14
//! days or less and use an ECDSA key - browsers reject hash-pinned
//! connections outside those bounds regardless of whether the hash
//! matches. The page's own plain HTTPS load is unaffected by any of this
//! and still just needs the browser to accept (or be told to accept) the
//! certificate normally.

use std::path::Path;

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use sha2::{Digest, Sha256};
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

    /// SHA-256 of the leaf certificate's DER bytes, for pinning the
    /// WebTransport connection to it via `serverCertificateHashes` instead
    /// of requiring it to chain to a trusted CA.
    pub fn cert_hash(&self) -> [u8; 32] {
        let leaf = &self.identity.certificate_chain().as_slice()[0];
        Sha256::digest(leaf.der()).into()
    }
}

fn identity_der(identity: &Identity) -> (Vec<Vec<u8>>, Vec<u8>) {
    let cert_der = identity.certificate_chain().as_slice().iter().map(|cert| cert.der().to_vec()).collect();
    let key_der = identity.private_key().secret_der().to_vec();
    (cert_der, key_der)
}
