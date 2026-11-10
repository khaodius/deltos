//! rustls client settings shared by the drivers that bring their own TLS setup.
//!
//! [`TlsMode::Prefer`] and [`TlsMode::Require`] encrypt without checking who answers,
//! as `libpq` does for `sslmode=prefer` and `require`; [`TlsMode::VerifyFull`] checks the
//! certificate chain against the system's roots (falling back to the Mozilla roots when
//! the system offers none) and the host name.

use std::sync::{Arc, OnceLock};

use deltos_data::TlsMode;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::ring::default_provider()))
        .clone()
}

fn roots() -> Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let mut store = RootCertStore::empty();
            let native = rustls_native_certs::load_native_certs();
            for cert in native.certs {
                let _ = store.add(cert);
            }
            if store.is_empty() {
                store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }
            Arc::new(store)
        })
        .clone()
}

/// A client configuration for `mode`, or an error message when rustls refuses the
/// protocol versions (which does not happen with the ring provider).
pub(crate) fn client_config(mode: TlsMode) -> Result<ClientConfig, String> {
    let builder = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup failed: {e}"))?;
    let config = match mode {
        TlsMode::VerifyFull => builder
            .with_root_certificates(roots())
            .with_no_client_auth(),
        TlsMode::Disable | TlsMode::Prefer | TlsMode::Require => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate {
                algorithms: provider().signature_verification_algorithms,
            }))
            .with_no_client_auth(),
    };
    Ok(config)
}

/// Encrypts without authenticating the server. Signatures are still checked, so the
/// session is bound to the certificate the server presented.
#[derive(Debug)]
struct AcceptAnyCertificate {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_every_mode() {
        for mode in [
            TlsMode::Disable,
            TlsMode::Prefer,
            TlsMode::Require,
            TlsMode::VerifyFull,
        ] {
            assert!(client_config(mode).is_ok());
        }
    }
}
