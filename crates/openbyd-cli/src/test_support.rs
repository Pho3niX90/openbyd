//! Hooks for the tests only (feature `test-support`, never in a release).
//!
//! The tests' MQTT broker has a certificate made for the test run, which a
//! child process cannot know. With `OPENBYD_TEST_MQTT_TRUST_ANY` set, the
//! tool accepts any certificate.

use std::sync::Arc;

use openbyd_client::mqtt::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use openbyd_client::mqtt::rustls::crypto::{self, CryptoProvider};
use openbyd_client::mqtt::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use openbyd_client::mqtt::rustls::{ClientConfig, DigitallySignedStruct, Error, SignatureScheme};

#[derive(Debug)]
struct TrustAny(Arc<CryptoProvider>);

impl ServerCertVerifier for TrustAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// A TLS client configuration that trusts every server certificate.
pub fn trust_any_config() -> Arc<ClientConfig> {
    let provider = Arc::new(crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TrustAny(provider)))
        .with_no_client_auth();
    Arc::new(config)
}
