use anyhow::Context;
use clap::{Parser, Subcommand};
use dotenv::dotenv;
use log::{debug, error, info};
use scramjet_common::Config;
use scramjet_net::{cartographer::Cartographer, engine::QuicEngine, geyser::spawn_geyser_monitor};
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "scramjet")]
struct Cli {
    // Optional Override via Command Line
    #[arg(short, long)]
    rpc: Option<String>,

    #[arg(long)]
    geyser: Option<String>,

    #[arg(short, long)]
    keypair: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Monitor,
    Fire {
        #[arg(short, long)]
        recipient: Option<String>,
        #[arg(long)]
        priority_fee: Option<u64>,
    },
    Spam {
        #[arg(short, long, default_value = "10")]
        count: u64,
        #[arg(short, long)]
        recipient: Option<String>,
        #[arg(long)]
        priority_fee: Option<u64>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // STEP 1: Load environment variables and initialize logging
    dotenv().ok();
    env_logger::init();

    let cli = Cli::parse();

    // STEP 2: Load and validate config (fail-fast on invalid values)
    let mut config = Config::from_env().context("Invalid configuration")?;

    // STEP 3: Apply CLI overrides (CLI > env > default)
    if let Some(rpc) = cli.rpc {
        config.rpc_url = rpc;
    }
    if let Some(geyser) = cli.geyser {
        config.geyser_url = Some(geyser);
    }

    let keypair_path = cli.keypair.unwrap_or_else(|| {
        dirs::home_dir()
            .expect("No home directory found. Set --keypair explicitly.")
            .join(".config/solana/id.json")
    });
    let identity = read_keypair_file(&keypair_path)
        .map_err(|e| anyhow::anyhow!("Failed to load keypair from {:?}: {}", keypair_path, e))?;
    info!("Identity: {}", identity.pubkey());

    // STEP 4: Initialize Cartographer (cluster map + leader schedule)
    info!("Initializing Cartographer with RPC: {}", config.rpc_url);
    let cartographer = Arc::new(Cartographer::new(config.rpc_url.clone()));
    cartographer.refresh_topology().await?; // Fetch validator pubkey -> QUIC socket map
    cartographer.update_schedule().await?; // Fetch leader schedule for current epoch

    // STEP 5: Initialize Clock (Geyser hybrid vs RPC polling mode)
    if let Some(ref url) = config.geyser_url {
        info!("MODE: HYBRID (RPC Map + Geyser Clock)");
        info!("   Geyser Endpoint: {}", url);
        // Use Yellowstone Geyser for real-time slot updates (lowest latency)
        spawn_geyser_monitor(
            url.clone(),
            cartographer.clone(),
            config.geyser_reconnect_delay(),
            config.geyser_max_reconnect_delay(),
        )
        .await;
    } else {
        info!("MODE: LEGACY (RPC Polling)");
        info!("   (Geyser URL not found in .env or args. Using fallback.)");
        // Fall back to RPC polling for slot updates
        let cart_clone = cartographer.clone();
        let poll_interval = config.rpc_poll_interval();
        tokio::spawn(async move {
            loop {
                if let Err(e) = cart_clone.fetch_rpc_slot().await {
                    debug!("RPC slot fetch failed: {}", e);
                }
                tokio::time::sleep(poll_interval).await;
            }
        });
    }

    // STEP 6: Initialize QUIC Engine with client certificate
    info!("Initializing Engine...");
    let engine = Arc::new(QuicEngine::new(&identity, &config)?);

    // STEP 7: Start Scout (pre-warm connections to upcoming leaders)
    let cart_clone = cartographer.clone();
    let engine_clone = engine.clone();
    let scout_interval = config.scout_interval();
    let lookahead = config.scout_lookahead_slots;
    tokio::spawn(async move {
        loop {
            let current_slot = cart_clone.get_known_slot();
            if current_slot > 0 {
                // Get unique upcoming leader IPs to pre-warm
                let upcoming = cart_clone
                    .get_upcoming_leaders(current_slot, lookahead)
                    .await;
                for target in upcoming {
                    debug!("Scout: Warming up connection to {}", target);
                    // Pre-warm connections (best-effort, failures are OK)
                    let _ = engine_clone.get_connection_handle(target).await;
                }
            }
            tokio::time::sleep(scout_interval).await;
        }
    });

    match cli.command {
        Commands::Monitor => monitor_loop(cartographer, config.monitor_interval()).await,
        Commands::Fire {
            recipient,
            priority_fee,
        } => {
            let to = parse_recipient(recipient, &identity)?;
            let fee = priority_fee.unwrap_or(config.default_priority_fee);
            fire_transaction(&cartographer, &engine, &identity, to, fee, &config).await?;
        }
        Commands::Spam {
            count,
            recipient,
            priority_fee,
        } => {
            let to = parse_recipient(recipient, &identity)?;
            let fee = priority_fee.unwrap_or(config.default_priority_fee);
            spam_transactions(&cartographer, &engine, &identity, to, count, fee, &config).await?;
        }
    }

    Ok(())
}

/// Parse recipient pubkey from CLI arg, defaulting to identity pubkey.
fn parse_recipient(recipient: Option<String>, identity: &Keypair) -> anyhow::Result<Pubkey> {
    match recipient {
        Some(s) => s
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid recipient pubkey: '{}'. Expected base58.", s)),
        None => Ok(identity.pubkey()),
    }
}

async fn monitor_loop(cartographer: Arc<Cartographer>, interval: std::time::Duration) {
    info!("Starting Monitor Mode...");
    loop {
        let slot = cartographer.get_known_slot();
        if slot > 0 {
            if let Some(target) = cartographer.get_target(slot).await {
                println!("Slot: {} | Leader IP: {}", slot, target);
            } else {
                println!("Slot: {} | Leader IP: UNKNOWN", slot);
            }
        }
        tokio::time::sleep(interval).await;
    }
}

async fn fire_transaction(
    cartographer: &Cartographer,
    engine: &QuicEngine,
    identity: &Keypair,
    recipient: Pubkey,
    priority_fee: u64,
    config: &Config,
) -> anyhow::Result<()> {
    // Get fresh blockhash for transaction
    let rpc = cartographer.rpc_client();
    let latest_blockhash = rpc.get_latest_blockhash().await?;

    // Build transaction: compute budget + priority fee + transfer
    let instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(config.default_compute_unit_limit),
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee),
        system_instruction::transfer(&identity.pubkey(), &recipient, 1),
    ];

    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&identity.pubkey()),
        &[identity],
        latest_blockhash,
    );
    let tx_bytes = bincode::serialize(&tx)?;

    // Resolve current leader and send via QUIC
    let slot = cartographer.get_known_slot();
    if let Some(addr) = cartographer.get_target(slot).await {
        info!("Target: {}. Firing (Fee: {})...", addr, priority_fee);
        engine.send_transaction(addr, tx_bytes).await?;
        info!("Sent! Sig: {}", tx.signatures[0]);
    } else {
        error!("No leader found for slot {}", slot);
    }
    Ok(())
}

async fn spam_transactions(
    cartographer: &Cartographer,
    engine: &QuicEngine,
    identity: &Keypair,
    recipient: Pubkey,
    count: u64,
    priority_fee: u64,
    config: &Config,
) -> anyhow::Result<()> {
    // Build transaction once (reused for all sends)
    let rpc = cartographer.rpc_client();
    let latest_blockhash = rpc.get_latest_blockhash().await?;

    // Build transaction: compute budget + priority fee + transfer
    let instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(config.default_compute_unit_limit),
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee),
        system_instruction::transfer(&identity.pubkey(), &recipient, 1),
    ];

    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&identity.pubkey()),
        &[identity],
        latest_blockhash,
    );
    let tx_bytes = bincode::serialize(&tx)?;

    // Lock onto current leader and get connection handle
    let slot = cartographer.get_known_slot();
    let target = cartographer
        .get_target(slot)
        .await
        .ok_or(anyhow::anyhow!("No leader found"))?;

    info!("Target Locked: {}", target);
    let connection = engine.get_connection_handle(target).await?; // Handshake once
    info!("Pipe Open. Firing {} rounds.", count);

    // Machine gun: fire all transactions in parallel using same connection
    let mut tasks = Vec::new();
    for _ in 0..count {
        let conn_clone = connection.clone();
        let bytes_clone = tx_bytes.clone();
        tasks.push(tokio::spawn(async move {
            // Open new QUIC stream on same connection (multiplexing)
            match conn_clone.open_uni().await {
                Ok(mut stream) => {
                    if let Err(e) = stream.write_all(&bytes_clone).await {
                        debug!("Stream write failed: {}", e);
                    }
                    if let Err(e) = stream.finish().await {
                        debug!("Stream finish failed: {}", e);
                    }
                }
                Err(e) => {
                    debug!("Failed to open stream: {}", e);
                }
            }
        }));
    }
    // Wait for all sends to complete
    for task in tasks {
        // Task join errors (panic/cancel) are acceptable to silence
        let _ = task.await;
    }
    info!("Firing Complete.");
    Ok(())
}
