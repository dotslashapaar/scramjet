use anyhow::{anyhow, Result};
use dashmap::DashMap;
use log::{debug, info};
use quinn::{Connection, Endpoint};
use scramjet_common::{create_quic_config, Config};
use solana_sdk::signature::Keypair;
use std::net::SocketAddr;
use std::sync::Arc;

/// The Engine manages QUIC connections to validator TPU ports
pub struct QuicEngine {
    endpoint: Endpoint,
    /// Cache: Target IP -> Active QUIC Connection (lock-free via DashMap)
    connection_cache: Arc<DashMap<SocketAddr, Connection>>,
}

impl QuicEngine {
    pub fn new(identity: &Keypair, config: &Config) -> Result<Self> {
        // Create QUIC client config with Solana identity certificate
        let client_config = create_quic_config(identity, config)?;

        // Bind to any available port (IPv4)
        let mut endpoint = Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))?;
        endpoint.set_default_client_config(client_config);

        Ok(Self {
            endpoint,
            connection_cache: Arc::new(DashMap::new()),
        })
    }

    /// Standard single-shot send (Thread-safe via DashMap)
    pub async fn send_transaction(&self, target: SocketAddr, tx_bytes: Vec<u8>) -> Result<()> {
        // Get or create connection from cache
        let connection = self.get_connection(target).await?;

        // Open unidirectional stream for this transaction
        let mut send_stream = connection
            .open_uni()
            .await
            .map_err(|e| anyhow!("Failed to open stream: {}", e))?;

        // Write transaction bytes to stream
        send_stream
            .write_all(&tx_bytes)
            .await
            .map_err(|e| anyhow!("Failed to write bytes: {}", e))?;

        // Close stream to signal completion
        send_stream
            .finish()
            .await
            .map_err(|e| anyhow!("Failed to finish stream: {}", e))?;

        Ok(())
    }

    /// MACHINE GUN OPTIMIZATION:
    /// Returns direct handle for high-frequency sending.
    /// Caller can open multiple streams on same connection (multiplexing).
    pub async fn get_connection_handle(&self, target: SocketAddr) -> Result<Connection> {
        self.get_connection(target).await
    }

    /// Internal: Manage connection cache with lock-free reads
    async fn get_connection(&self, addr: SocketAddr) -> Result<Connection> {
        // Fast path: check cache without blocking
        if let Some(conn) = self.connection_cache.get(&addr) {
            if conn.close_reason().is_none() {
                return Ok(conn.clone());
            }
        }

        // Remove stale connection if exists
        self.connection_cache.remove(&addr);

        // Handshake OUTSIDE of any lock (avoids blocking other lookups)
        info!("Handshake: Connecting to leader at {}...", addr);
        let connecting = self.endpoint.connect(addr, "solana")?;
        let connection = connecting
            .await
            .map_err(|e| anyhow!("Connection failed: {}", e))?;

        // Insert with minimal contention
        self.connection_cache.insert(addr, connection.clone());
        debug!("Connection cached for {}", addr);

        Ok(connection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::Keypair;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn make_server_config() -> (quinn::ServerConfig, Vec<u8>) {
        let cert = rcgen::generate_simple_self_signed(vec!["solana".into()]).unwrap();
        let cert_der = cert.serialize_der().unwrap();
        let key_der = cert.serialize_private_key_der();
        let key = rustls::PrivateKey(key_der);
        let cert_chain = vec![rustls::Certificate(cert_der.clone())];

        let mut server_crypto = rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .unwrap();

        server_crypto.alpn_protocols = vec![b"solana-tpu".to_vec()];

        (
            quinn::ServerConfig::with_crypto(Arc::new(server_crypto)),
            cert_der,
        )
    }

    #[tokio::test]
    async fn test_connection_reuse_multiplexing() {
        // 1. SETUP: Server (IPv4 to match client)
        let (server_config, _) = make_server_config();
        let server_endpoint =
            Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = server_endpoint.local_addr().unwrap();
        println!("Test Server listening on: {}", server_addr);

        let (tx, mut rx) = mpsc::channel(100);

        // 2. SERVER LOGIC
        tokio::spawn(async move {
            if let Some(conn) = server_endpoint.accept().await {
                let connection = conn.await.expect("Handshake failed");
                // Keep accepting streams on this ONE connection
                loop {
                    match connection.accept_uni().await {
                        Ok(mut stream) => {
                            let tx = tx.clone();
                            tokio::spawn(async move {
                                // Read up to 1KB
                                let _ = stream.read_to_end(1024).await;
                                tx.send(1).await.unwrap();
                            });
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        // 3. CLIENT LOGIC
        let identity = Keypair::new();
        let config = Config::from_env().expect("Failed to load config");
        let engine = QuicEngine::new(&identity, &config).expect("Failed to init engine");

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // A. Handshake ONCE
        let connection_handle = engine
            .get_connection_handle(server_addr)
            .await
            .expect("Failed to get connection handle");

        // B. Fire 10 streams in parallel using the SAME handle
        for i in 0..10 {
            let conn = connection_handle.clone();
            let payload = vec![i as u8];

            tokio::spawn(async move {
                let mut stream = conn.open_uni().await.expect("Failed to open stream");
                stream.write_all(&payload).await.expect("Write failed");
                stream.finish().await.expect("Finish failed");
            });
        }

        // 4. VERIFICATION
        let mut received_count = 0;
        while let Some(_) = rx.recv().await {
            received_count += 1;
            if received_count == 10 {
                break;
            }
        }

        assert_eq!(received_count, 10, "Multiplexing failed");
    }
}
