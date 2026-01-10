use thiserror::Error;

#[derive(Error, Debug)]
pub enum ScramjetError {
    #[error("Configuration error: {0}")]
    ConfigError(String),
    #[error("Configuration validation error: {0}")]
    ConfigValidationError(String),
    #[error("Certificate generation error: {0}")]
    CertError(String),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Invalid pubkey: {0}")]
    InvalidPubkey(String),
    #[error("Keypair error: {0}")]
    KeypairError(String),
    #[error("Connection error: {0}")]
    ConnectionError(String),
    #[error("Geyser error: {0}")]
    GeyserError(String),
    #[error("RPC error: {0}")]
    RpcError(String),
}
