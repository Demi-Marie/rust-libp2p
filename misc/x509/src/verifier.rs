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

use libp2p_core::identity::PublicKey;
use ring::io::der;
use untrusted::{Input, Reader};
use webpki::Error;

/// Libp2p client and server certificate verifier.
pub(crate) struct Libp2pCertificateVerifier;

/// libp2p requires the following of X.509 server certificate chains:
///
/// * Exactly one certificate must be presented.
/// * The certificate must be self-signed.
/// * The certificate must have a valid libp2p extension that includes a
///   signature of its public key.
///
/// The check that the [`PeerId`] matches the expected `PeerId` must be done by
/// the caller.
///
/// [`PeerId`]: libp2p_core::PeerId
impl rustls::ServerCertVerifier for Libp2pCertificateVerifier {
    fn verify_server_cert(
        &self, _roots: &rustls::RootCertStore, presented_certs: &[rustls::Certificate],
        _dns_name: webpki::DNSNameRef<'_>, _ocsp_response: &[u8],
    ) -> Result<rustls::ServerCertVerified, rustls::TLSError> {
        verify_presented_certs(presented_certs).map(|()| rustls::ServerCertVerified::assertion())
    }

    fn verify_certificate_signature(
        &self, scheme: rustls::SignatureScheme, version: rustls::ProtocolVersion,
        certificate: &rustls::Certificate, msg: &[u8], signature: &[u8],
    ) -> Result<rustls::HandshakeSignatureValid, rustls::TLSError> {
        assert_eq!(version, rustls::ProtocolVersion::TLSv1_3);
        x509::parse_certificate(certificate.as_ref())
            .map_err(rustls::TLSError::WebPKIError)?
            .verify_signature_against_scheme(get_time()?, scheme, msg, signature)
            .map_err(rustls::TLSError::WebPKIError)
            .map(|()| rustls::HandshakeSignatureValid::assertion())
    }
}

fn verify_libp2p_signature(
    libp2p_extension: &Libp2pExtension<'_>, x509_pkey_bytes: &[u8],
) -> Result<(), Error> {
    let mut v = Vec::with_capacity(crate::LIBP2P_SIGNING_PREFIX_LENGTH + x509_pkey_bytes.len());
    v.extend_from_slice(&crate::LIBP2P_SIGNING_PREFIX[..]);
    v.extend_from_slice(x509_pkey_bytes);
    if libp2p_extension
        .peer_key
        .verify(&v, libp2p_extension.signature)
    {
        Ok(())
    } else {
        Err(Error::UnknownIssuer)
    }
}

fn get_time() -> Result<u64, rustls::TLSError> {
    use std::time::{SystemTime, SystemTimeError};
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_: SystemTimeError| rustls::TLSError::FailedToGetCurrentTime)
        .map(|e| e.as_secs())
}

fn parse_certificate(
    certificate: &[u8],
) -> Result<(x509::X509Certificate<'_>, Libp2pExtension<'_>), Error> {
    let parsed = x509::parse_certificate(certificate)?;
    let mut libp2p_extension = None;

    parsed
        .extensions()
        .iterate(&mut |oid, critical, extension| {
            Ok(match oid {
                crate::LIBP2P_OID_BYTES if libp2p_extension.is_some() => return Err(Error::BadDER),
                crate::LIBP2P_OID_BYTES =>
                    libp2p_extension = Some(parse_libp2p_extension(extension)?),
                _ if critical => return Err(Error::UnsupportedCriticalExtension),
                _ => {},
            })
        })?;
    let libp2p_extension = libp2p_extension.ok_or(Error::UnknownIssuer)?;
    Ok((parsed, libp2p_extension))
}

fn verify_presented_certs(presented_certs: &[rustls::Certificate]) -> Result<(), rustls::TLSError> {
    if presented_certs.len() != 1 {
        return Err(rustls::TLSError::NoCertificatesPresented);
    }
    let (certificate, extension) =
        parse_certificate(presented_certs[0].as_ref()).map_err(rustls::TLSError::WebPKIError)?;
    let now = get_time()?;
    certificate
        .verify_data_algorithm_signature(now, &certificate.das())
        .map_err(rustls::TLSError::WebPKIError)?;
    verify_libp2p_signature(&extension, certificate.subject_public_key_info().key())
        .map_err(rustls::TLSError::WebPKIError)
}

struct Libp2pExtension<'a> {
    peer_key: PublicKey,
    signature: &'a [u8],
}

#[inline(always)]
fn read_bit_string<'a>(input: &mut Reader<'a>, e: Error) -> Result<Input<'a>, Error> {
    der::bit_string_with_no_unused_bits(input).map_err(|_| e)
}

fn parse_libp2p_extension<'a>(extension: Input<'a>) -> Result<Libp2pExtension<'a>, Error> {
    let e = Error::ExtensionValueInvalid;
    Input::read_all(&extension, e, |input| {
        der::nested(input, der::Tag::Sequence, e, |input| {
            let public_key = read_bit_string(input, e)?.as_slice_less_safe();
            let signature = read_bit_string(input, e)?.as_slice_less_safe();
            // We deliberately discard the error information because this is
            // either a broken peer or an attack.
            let peer_key = PublicKey::from_protobuf_encoding(public_key).map_err(|_| e)?;
            Ok(Libp2pExtension {
                signature,
                peer_key,
            })
        })
    })
}
/// libp2p requires the following of X.509 client certificate chains:
///
/// * Exactly one certificate must be presented. In particular, client
///   authentication is mandatory in libp2p.
/// * The certificate must be self-signed.
/// * The certificate must have a valid libp2p extension that includes a
///   signature of its public key.
///
/// The check that the [`PeerId`] matches the expected `PeerId` must be done by
/// the caller.
///
/// [`PeerId`]: libp2p_core::PeerId
impl rustls::ClientCertVerifier for Libp2pCertificateVerifier {
    fn offer_client_auth(&self) -> bool { true }

    fn client_auth_root_subjects(
        &self, _dns_name: Option<&webpki::DNSName>,
    ) -> Option<rustls::DistinguishedNames> {
        Some(vec![])
    }

    fn verify_client_cert(
        &self, presented_certs: &[rustls::Certificate], _dns_name: Option<&webpki::DNSName>,
    ) -> Result<rustls::ClientCertVerified, rustls::TLSError> {
        verify_presented_certs(presented_certs).map(|()| rustls::ClientCertVerified::assertion())
    }

    fn verify_certificate_signature(
        &self, scheme: rustls::SignatureScheme, version: rustls::ProtocolVersion,
        certificate: &rustls::Certificate, msg: &[u8], signature: &[u8],
    ) -> Result<rustls::HandshakeSignatureValid, rustls::TLSError> {
        assert_eq!(version, rustls::ProtocolVersion::TLSv1_3);
        x509::parse_certificate(certificate.as_ref())
            .map_err(rustls::TLSError::WebPKIError)?
            .verify_signature_against_scheme(get_time()?, scheme, msg, signature)
            .map_err(rustls::TLSError::WebPKIError)
            .map(|()| rustls::HandshakeSignatureValid::assertion())
    }
}

/// Extracts the `PeerId` from a certificate’s libp2p extension. It is erroneous
/// to call this unless the certificate is known to be a well-formed X.509
/// certificate with a valid libp2p extension. The certificate verifiers in this
/// crate validate check this.
///
/// # Panics
///
/// Panics if called on an invalid certificate.
pub fn extract_peerid_or_panic(certificate: &[u8]) -> libp2p_core::PeerId {
    let r = parse_certificate(certificate)
        .expect("we already checked that the certificate was valid during the handshake; qed");
    libp2p_core::PeerId::from_public_key(r.1.peer_key)
}