use crate::config::Config;
use crate::error::ScramjetError;
use rcgen::CertificateParams;
use solana_sdk::signature::Keypair;
use std::sync::Arc;

/// Creates a QUIC Client Config configured for Solana's swQoS
pub fn create_quic_config(
    identity_keypair: &Keypair,
    config: &Config,
) -> Result<quinn::ClientConfig, ScramjetError> {
    // STEP 1: Convert Solana Ed25519 keypair to rcgen format
    let rcgen_keypair = solana_to_rcgen_keypair(identity_keypair)?;

    // STEP 2: Generate self-signed certificate with Ed25519
    let mut cert_params = CertificateParams::new(vec!["solana".to_string()]);
    cert_params.key_pair = Some(rcgen_keypair);

    // Explicitly use Ed25519 algorithm
    cert_params.alg = &rcgen::PKCS_ED25519;

    let cert = rcgen::Certificate::from_params(cert_params)
        .map_err(|e| ScramjetError::CertError(e.to_string()))?;

    let cert_der = cert
        .serialize_der()
        .map_err(|e| ScramjetError::CertError(format!("Failed to serialize cert: {}", e)))?;

    let private_key_der = cert.serialize_private_key_der();

    // STEP 3: Configure Rustls with custom cert verifier (skip validator cert checks)
    let cert_chain = vec![rustls::Certificate(cert_der)];
    let private_key = rustls::PrivateKey(private_key_der);

    let mut client_crypto = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification)) // Validators use ephemeral certs
        .with_client_auth_cert(cert_chain, private_key)
        .map_err(|e| ScramjetError::ConfigError(e.to_string()))?;

    // CRITICAL: Set ALPN to "solana-tpu" for Solana protocol
    client_crypto.alpn_protocols = vec![b"solana-tpu".to_vec()];

    // STEP 4: Configure Quinn QUIC transport (keep-alive + timeout)
    let mut client_config = quinn::ClientConfig::new(Arc::new(client_crypto));

    let mut transport_config = quinn::TransportConfig::default();
    transport_config.keep_alive_interval(Some(config.quic_keep_alive()));
    transport_config.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(config.quic_idle_timeout())
            .map_err(|e| ScramjetError::ConfigError(format!("Invalid idle timeout: {}", e)))?,
    ));

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
struct SkipServerVerification;

impl rustls::client::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}

/// Convert Solana keypair to rcgen keypair by wrapping in PKCS#8 format
fn solana_to_rcgen_keypair(solana_pair: &Keypair) -> Result<rcgen::KeyPair, ScramjetError> {
    // PKCS#8 header for Ed25519 private keys
    const ED25519_PKCS8_HEADER: &[u8] = &[
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];

    let secret_bytes = solana_pair.secret().to_bytes();
    let mut pkcs8_bytes = Vec::with_capacity(ED25519_PKCS8_HEADER.len() + 32);
    pkcs8_bytes.extend_from_slice(ED25519_PKCS8_HEADER);
    pkcs8_bytes.extend_from_slice(&secret_bytes);

    rcgen::KeyPair::from_der(&pkcs8_bytes)
        .map_err(|e| ScramjetError::CertError(format!("Key conversion failed: {}", e)))
}
