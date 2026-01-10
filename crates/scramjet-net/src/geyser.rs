use crate::cartographer::Cartographer;
use http::Uri;
use log::{error, info};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint};
use tonic::{service::Interceptor, Request, Status};
use yellowstone_grpc_proto::geyser::SubscribeRequest;
use yellowstone_grpc_proto::geyser::{
    geyser_client::GeyserClient, subscribe_update::UpdateOneof, SubscribeRequestFilterSlots,
};

/// Geyser listener for real-time slot updates via Yellowstone gRPC
pub struct GeyserListener {
    client: GeyserClient<tonic::service::interceptor::InterceptedService<Channel, AuthInterceptor>>,
    cartographer: Arc<Cartographer>,
}

#[derive(Clone)]
struct AuthInterceptor {
    token: Option<String>,
}

impl Interceptor for AuthInterceptor {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        if let Some(token) = &self.token {
            let val = tonic::metadata::MetadataValue::from_str(token)
                .map_err(|_| Status::invalid_argument("Invalid token format"))?;
            req.metadata_mut().insert("x-token", val);
        }
        Ok(req)
    }
}

impl GeyserListener {
    pub async fn connect(
        mut endpoint: String,
        cartographer: Arc<Cartographer>,
    ) -> anyhow::Result<Self> {
        info!("Geyser: Parsing endpoint...");

        // Extract auth token from URL path if present (e.g., https://host/token123)
        let mut x_token = None;
        if let Ok(uri) = endpoint.parse::<Uri>() {
            if let Some(path) = uri.path_and_query() {
                let path_str = path.as_str();
                if path_str.len() > 10 {
                    info!("Geyser: Extracting Auth Token from URL path.");
                    x_token = Some(path_str.trim_start_matches('/').to_string());

                    // Reconstruct clean endpoint without path
                    let scheme = uri.scheme_str().unwrap_or("https");
                    let authority = uri.authority().unwrap().as_str();
                    endpoint = format!("{}://{}", scheme, authority);
                }
            }
        }

        info!("Geyser: Connecting to {}", endpoint);

        // Create gRPC channel with TLS
        let channel = Endpoint::from_shared(endpoint)?
            .tls_config(tonic::transport::ClientTlsConfig::new())?
            .connect()
            .await?;

        let interceptor = AuthInterceptor { token: x_token };
        let client = GeyserClient::with_interceptor(channel, interceptor);

        info!("Geyser: Connected.");
        Ok(Self {
            client,
            cartographer,
        })
    }

    pub async fn start_tracking(&mut self) -> anyhow::Result<()> {
        info!("Geyser: Subscribing to Slot Updates.");

        // Subscribe to slot updates only (minimal data)
        let mut slots = std::collections::HashMap::new();
        slots.insert(
            "client".to_string(),
            SubscribeRequestFilterSlots {
                filter_by_commitment: None,
            },
        );

        let request = SubscribeRequest {
            slots,
            accounts: std::collections::HashMap::new(),
            transactions: std::collections::HashMap::new(),
            transactions_status: std::collections::HashMap::new(),
            blocks: std::collections::HashMap::new(),
            blocks_meta: std::collections::HashMap::new(),
            entry: std::collections::HashMap::new(),
            commitment: None,
            accounts_data_slice: vec![],
            ping: None,
        };

        let (tx, rx) = mpsc::channel(32);
        tx.send(request)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send request: {}", e))?;
        let request_stream = ReceiverStream::new(rx);

        let response = self.client.subscribe(request_stream).await?;
        let mut stream = response.into_inner();

        info!("Geyser: Stream Active.");

        // Process slot updates as they arrive (real-time)
        while let Some(message) = stream.message().await? {
            if let Some(UpdateOneof::Slot(slot_update)) = message.update_oneof {
                if slot_update.status == 0 {
                    // Processed slot
                    let slot = slot_update.slot;
                    self.cartographer.update_slot(slot);
                }
            }
        }

        Ok(())
    }
}

/// Spawn Geyser monitor with exponential backoff reconnection
pub async fn spawn_geyser_monitor(
    endpoint: String,
    cartographer: Arc<Cartographer>,
    initial_delay: Duration,
    max_delay: Duration,
) {
    tokio::spawn(async move {
        let mut retry_delay = initial_delay;

        // Reconnect loop with exponential backoff
        loop {
            match GeyserListener::connect(endpoint.clone(), cartographer.clone()).await {
                Ok(mut listener) => {
                    // Reset backoff on successful connection
                    retry_delay = initial_delay;

                    if let Err(e) = listener.start_tracking().await {
                        error!(
                            "Geyser Stream Error: {}. Reconnecting in {:?}...",
                            e, retry_delay
                        );
                    }
                }
                Err(e) => {
                    error!(
                        "Geyser Connection Failed: {}. Retrying in {:?}...",
                        e, retry_delay
                    );
                }
            }

            tokio::time::sleep(retry_delay).await;

            // Exponential backoff: double delay, capped at max
            retry_delay = std::cmp::min(retry_delay * 2, max_delay);
        }
    });
}
