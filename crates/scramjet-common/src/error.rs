use thiserror::Error;

#[derive(Error, Debug)]
pub enum ScramjetError {
    // --- Configuration ---
    #[error("Configuration error: {0}")]
    ConfigError(String),
    #[error("Configuration validation error: {0}")]
    ConfigValidationError(String),

    // --- Certificate/Identity ---
    #[error("Certificate generation error: {0}")]
    CertError(String),
    #[error("Keypair error: {0}")]
    KeypairError(String),
    #[error("Home directory not found")]
    HomeDirNotFound,

    // --- IO ---
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    // --- Parsing ---
    #[error("Invalid pubkey: {0}")]
    InvalidPubkey(String),
    #[error("Invalid URI: {0}")]
    InvalidUri(String),
    #[error("Serialization error: {0}")]
    SerializationError(String),

    // --- QUIC/Transport ---
    #[error("Connection error: {0}")]
    ConnectionError(String),
    #[error("QUIC transport error: {0}")]
    TransportError(#[from] quinn::ConnectionError),
    #[error("QUIC write error: {0}")]
    WriteError(#[from] quinn::WriteError),
    #[error("QUIC stream closed: {0}")]
    ClosedStreamError(#[from] quinn::ClosedStream),
    #[error("Stream error: {0}")]
    StreamError(String),

    // --- gRPC/Tonic (boxed to reduce Result size) ---
    #[error("gRPC transport error: {0}")]
    GrpcTransportError(#[from] tonic::transport::Error),
    #[error("gRPC status error: {0}")]
    GrpcStatusError(#[source] Box<tonic::Status>),

    // --- Geyser ---
    #[error("Geyser error: {0}")]
    GeyserError(String),
    #[error("Geyser stream closed unexpectedly")]
    GeyserStreamClosed,

    // --- RPC/Solana Client (boxed - 224 bytes otherwise) ---
    #[error("RPC error: {0}")]
    RpcError(String),
    #[error("Solana client error: {0}")]
    SolanaClientError(#[source] Box<solana_client::client_error::ClientError>),

    // --- Topology ---
    #[error("No leader found for slot {0}")]
    NoLeaderFound(u64),
    #[error("Leader schedule unavailable")]
    ScheduleUnavailable,

    // --- Async/Channel ---
    #[error("Channel error: {0}")]
    ChannelError(String),
    #[error("Startup timeout")]
    StartupTimeout,
}

// Manual From implementations for boxed types
impl From<tonic::Status> for ScramjetError {
    fn from(err: tonic::Status) -> Self {
        ScramjetError::GrpcStatusError(Box::new(err))
    }
}

impl From<solana_client::client_error::ClientError> for ScramjetError {
    fn from(err: solana_client::client_error::ClientError) -> Self {
        ScramjetError::SolanaClientError(Box::new(err))
    }
}
