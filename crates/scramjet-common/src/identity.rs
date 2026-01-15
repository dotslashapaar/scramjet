use crate::config::Config;
use crate::error::ScramjetError;
use quinn::crypto::rustls::QuicClientConfig;
use rcgen::CertificateParams;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use solana_sdk::signature::Keypair;
use std::sync::Arc;

/// Creates a QUIC Client Config configured for Solana's swQoS
pub fn create_quic_config(
    identity_keypair: &Keypair,
    config: &Config,
) -> Result<quinn::ClientConfig, ScramjetError> {
    // STEP 1: Convert Solana Ed25519 keypair to rcgen format
    let rcgen_keypair = solana_to_rcgen_keypair(identity_keypair)?;

    // STEP 2: Generate self-signed certificate with Ed25519 (rcgen 0.13 API)
    let cert_params = CertificateParams::new(vec!["solana".to_string()])
        .map_err(|e| ScramjetError::CertError(e.to_string()))?;

    let cert = cert_params
        .self_signed(&rcgen_keypair)
        .map_err(|e| ScramjetError::CertError(e.to_string()))?;

    let cert_der = cert.der().to_vec();
    let private_key_der = rcgen_keypair.serialize_der();

    // STEP 3: Configure Rustls with custom cert verifier (skip validator cert checks)
    let cert_chain = vec![CertificateDer::from(cert_der)];
    let private_key = PrivatePkcs8KeyDer::from(private_key_der);

    let mut client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification::new()))
        .with_client_auth_cert(cert_chain, private_key.into())
        .map_err(|e| ScramjetError::ConfigError(e.to_string()))?;

    // CRITICAL: Set ALPN to "solana-tpu" for Solana protocol
    client_crypto.alpn_protocols = vec![b"solana-tpu".to_vec()];

    // STEP 4: Configure Quinn QUIC transport (keep-alive + timeout + FIFO scheduling)
    let quic_crypto = QuicClientConfig::try_from(client_crypto)
        .map_err(|e| ScramjetError::ConfigError(format!("QUIC crypto config error: {}", e)))?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_crypto));

    let mut transport_config = quinn::TransportConfig::default();
    transport_config.keep_alive_interval(Some(config.quic_keep_alive()));
    transport_config.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(config.quic_idle_timeout())
            .map_err(|e| ScramjetError::ConfigError(format!("Invalid idle timeout: {}", e)))?,
    ));
    // CRITICAL: Disable fairness to force FIFO stream scheduling
    // This ensures each transaction completes as an atomic UDP packet before the next starts
    transport_config.send_fairness(false);

    client_config.transport_config(Arc::new(transport_config));

    Ok(client_config)
}

// --- Helpers ---

/// # Security Notice
///
/// This verifier skips TLS certificate validation because Solana validators
/// use ephemeral self-signed certificates for TPU QUIC connections.
/// This is the expected behavior for the Solana network protocol.
///
/// The QUIC connection is still encrypted - we're just not verifying the
/// validator's identity via certificate chain. Validator identity is instead
/// verified through the Solana protocol itself (leader schedule, stake, etc.).
#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl SkipServerVerification {
    fn new() -> Self {
        Self(Arc::new(rustls::crypto::ring::default_provider()))
    }
}

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
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
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Convert Solana keypair to rcgen keypair by wrapping in PKCS#8 format
fn solana_to_rcgen_keypair(solana_pair: &Keypair) -> Result<rcgen::KeyPair, ScramjetError> {
    // PKCS#8 header for Ed25519 private keys
    const ED25519_PKCS8_HEADER: &[u8] = &[
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];

    // Use secret_bytes() instead of deprecated secret().to_bytes()
    // secret_bytes() returns [u8; 64], first 32 bytes are the private key
    let full_secret = solana_pair.secret_bytes();
    let mut pkcs8_bytes = Vec::with_capacity(ED25519_PKCS8_HEADER.len() + 32);
    pkcs8_bytes.extend_from_slice(ED25519_PKCS8_HEADER);
    pkcs8_bytes.extend_from_slice(&full_secret[0..32]);

    rcgen::KeyPair::try_from(pkcs8_bytes.as_slice())
        .map_err(|e| ScramjetError::CertError(format!("Key conversion failed: {}", e)))
}
