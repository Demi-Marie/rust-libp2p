// Copyright 2020 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! TLS configuration for `libp2p-quic`.
#![deny(
    const_err,
    deprecated,
    improper_ctypes,
    non_shorthand_field_patterns,
    nonstandard_style,
    no_mangle_generic_items,
    renamed_and_removed_lints,
    unknown_lints,
    type_alias_bounds,
    unconditional_recursion,
    while_true,
    elided_lifetimes_in_paths,
    missing_copy_implementations,
    missing_debug_implementations,
    missing_docs,
    single_use_lifetimes,
    trivial_casts,
    trivial_numeric_casts,
    rust_2018_idioms,
    unused,
    future_incompatible,
    clippy::all
)]
#![forbid(unsafe_code)]

mod certificate;
mod verifier;

use err_derive::Error;
use std::sync::Arc;
pub use verifier::extract_peerid_or_panic;

const LIBP2P_SIGNING_PREFIX: [u8; 21] = *b"libp2p-tls-handshake:";
const LIBP2P_SIGNING_PREFIX_LENGTH: usize = LIBP2P_SIGNING_PREFIX.len();
const LIBP2P_OID_BYTES: &[u8] = &[43, 6, 1, 4, 1, 131, 162, 90, 1, 1];

/// Error creating a configuration
#[derive(Debug, Error)]
pub enum ConfigError {
    /// TLS private key or certificate rejected
    #[error(display = "TLS private or certificate key rejected: {}", _0)]
    TLSError(#[error(source)] rustls::TLSError),
    /// Signing failed
    #[error(display = "Signing failed: {}", _0)]
    SigningError(#[error(source)] libp2p_core::identity::error::SigningError),
    /// Certificate generation error
    #[error(display = "Certificate generation error: {}", _0)]
    RcgenError(#[error(source)] rcgen::RcgenError),
}

fn make_client_config(
    certificate: rustls::Certificate, key: rustls::PrivateKey,
    verifier: Arc<verifier::Libp2pCertificateVerifier>,
) -> Result<rustls::ClientConfig, rustls::TLSError> {
    let mut crypto = rustls::ClientConfig::new();
    crypto.versions = vec![rustls::ProtocolVersion::TLSv1_3];
    crypto.alpn_protocols = vec![b"libp2p".to_vec()];
    crypto.enable_early_data = false;
    crypto.set_single_client_cert(vec![certificate], key)?;
    crypto.dangerous().set_certificate_verifier(verifier);
    Ok(crypto)
}

fn make_server_config(
    certificate: rustls::Certificate, key: rustls::PrivateKey,
    verifier: Arc<verifier::Libp2pCertificateVerifier>,
) -> Result<rustls::ServerConfig, rustls::TLSError> {
    let mut crypto = rustls::ServerConfig::new(verifier);
    crypto.versions = vec![rustls::ProtocolVersion::TLSv1_3];
    crypto.alpn_protocols = vec![b"libp2p".to_vec()];
    crypto.set_single_cert(vec![certificate], key)?;
    Ok(crypto)
}

/// Create TLS client and server configurations for libp2p.
pub fn make_tls_config(
    keypair: &libp2p_core::identity::Keypair,
) -> Result<(rustls::ClientConfig, rustls::ServerConfig), ConfigError> {
    let cert = certificate::make_cert(&keypair)?;
    let private_key = cert.serialize_private_key_der();
    let verifier = Arc::new(verifier::Libp2pCertificateVerifier);
    let cert = rustls::Certificate(cert.serialize_der()?);
    let key = rustls::PrivateKey(private_key);
    Ok((
        make_client_config(cert.clone(), key.clone(), verifier.clone())?,
        make_server_config(cert, key, verifier)?,
    ))
}