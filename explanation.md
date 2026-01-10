# Scramjet

**A high-performance Solana transaction client for ultra-low-latency transaction submission.**

## Overview

Scramjet bypasses traditional RPC-based transaction submission by sending transactions **directly to validator leaders over QUIC protocol**. This approach leverages Solana's stake-weighted Quality of Service (swQoS) to achieve lower latency and higher throughput compared to standard RPC submission.

### Target Use Cases

- **MEV Searchers** – Sub-millisecond transaction delivery for arbitrage opportunities
- **High-Frequency Traders** – Latency-sensitive trading bots and market makers
- **Transaction Spammers** – Rapid transaction submission with minimal overhead

### Why Direct-to-Leader?

When you submit a transaction via RPC, it travels through multiple hops before reaching the current slot leader. Scramjet eliminates this overhead by:

1. Tracking the leader schedule in real-time
2. Maintaining hot QUIC connections to upcoming leaders
3. Sending transactions directly to the leader's TPU (Transaction Processing Unit) port

---

## Table of Contents

- [Architecture](#architecture)
- [Core Components](#core-components)
  - [1. QuicEngine](#1-quicengine-scramjet-netsrcenginers)
  - [2. Cartographer](#2-cartographer-scramjet-netsrccartographerrs)
  - [3. GeyserListener](#3-geyserlistener-scramjet-netsrcgeyserrs)
  - [4. Identity](#4-identity-scramjet-commonsrcidentityrs)
- [Clock Modes](#clock-modes)
  - [Hybrid Mode (Recommended)](#hybrid-mode-recommended)
  - [Legacy Mode (Fallback)](#legacy-mode-fallback)
- [Connection Pre-warming (Scout)](#connection-pre-warming-scout)
- [Machine Gun Mode](#machine-gun-mode)
- [External Integrations](#external-integrations)
- [CLI Reference](#cli-reference)
  - [Commands](#commands)
  - [Options](#options)
  - [Examples](#examples)
- [Configuration](#configuration)
  - [Network Endpoints](#network-endpoints)
  - [Timing Intervals](#timing-intervals)
  - [QUIC Transport](#quic-transport)
  - [Transaction Defaults](#transaction-defaults)
  - [Geyser Reconnection](#geyser-reconnection)
- [Notable Implementation Patterns](#notable-implementation-patterns)
- [Dependencies](#dependencies)
- [Performance Considerations](#performance-considerations)
- [Security Notes](#security-notes)

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                       scramjet-cli                          │
│           (CLI parsing, command dispatch, runtime)          │
└─────────────────────────────────────────────────────────────┘
                            │
            ┌───────────────┼───────────────┐
            ▼               ▼               ▼
┌───────────────────┐ ┌───────────┐ ┌─────────────────┐
│   Cartographer    │ │  Engine   │ │ GeyserListener  │
│ (Leader Schedule) │ │  (QUIC)   │ │ (Slot Stream)   │
└───────────────────┘ └───────────┘ └─────────────────┘
                            │
                ┌───────────────────────┐
                │    scramjet-common    │
                │ (Config, Identity,    │
                │  Error Types)         │
                └───────────────────────┘
```

### Crate Responsibilities

| Crate | Purpose |
|-------|---------|
| **scramjet-common** | Shared utilities: configuration loading, QUIC identity/certificate generation, error types |
| **scramjet-net** | Network layer: QUIC engine, connection caching, Geyser integration, leader schedule tracking |
| **scramjet-cli** | CLI binary: command parsing, orchestration, transaction building |

---

## Core Components

### 1. QuicEngine (`scramjet-net/src/engine.rs`)

Manages QUIC connections to validator TPU ports with automatic connection caching.

**Key Features:**
- **Lock-free connection cache** using `DashMap` for concurrent access without mutex contention
- **Connection reuse** – avoids per-transaction handshake overhead
- **Stream multiplexing** – multiple transactions can share a single connection

**How it works:**
```
1. Check cache for existing connection to target validator
2. If valid connection exists → reuse it
3. If not → establish new QUIC connection and cache it
4. Open unidirectional stream, write transaction bytes, close stream
```

### 2. Cartographer (`scramjet-net/src/cartographer.rs`)

Maintains cluster topology and leader schedule mapping.

**Responsibilities:**
- Maps validator pubkeys to their QUIC socket addresses
- Tracks the leader schedule (which validator leads which slot)
- Provides slot → leader → socket address resolution
- Uses `AtomicU64` for lock-free slot tracking

**Data Flow:**
```
Slot Number → Leader Pubkey → QUIC Socket Address
     ↓              ↓                  ↓
 (schedule)     (node_map)         (engine)
```

### 3. GeyserListener (`scramjet-net/src/geyser.rs`)

Real-time slot updates via Yellowstone Geyser gRPC protocol.

**Why Geyser?**
- Lower latency than RPC polling (~10-50ms faster)
- Push-based updates vs pull-based polling
- Minimal bandwidth (subscribes only to slot updates)

**Reconnection Strategy:**
- Exponential backoff on connection failure
- Configurable initial delay (default: 1s) and max delay (default: 10s)
- Automatic reconnection with state preservation

### 4. Identity (`scramjet-common/src/identity.rs`)

Generates QUIC client identity from Solana keypairs.

**Process:**
1. Convert Ed25519 keypair to PKCS#8 format
2. Generate self-signed X.509 certificate
3. Set ALPN protocol to `"solana-tpu"` (required by validators)
4. Configure `rustls` to skip server certificate verification (validators use ephemeral certs)

---

## Clock Modes

Scramjet supports two modes for tracking the current slot:

### Hybrid Mode (Recommended)

Uses **Geyser gRPC** for real-time slot updates.

- **Latency**: ~10-50ms from slot confirmation
- **Requirement**: Yellowstone Geyser endpoint (`GEYSER_URL`)
- **Best for**: Production MEV/HFT systems

### Legacy Mode (Fallback)

Uses **RPC polling** for slot updates.

- **Latency**: Configurable polling interval (default: 400ms)
- **Requirement**: Standard Solana RPC endpoint
- **Best for**: Testing or when Geyser is unavailable

---

## Connection Pre-warming (Scout)

The Scout background task maintains hot connections to upcoming leaders:

```
┌──────────────────────────────────────────────────────┐
│  Current Slot: 250000                                │
│  Lookahead: 10 slots                                 │
│                                                      │
│  Pre-warmed connections:                             │
│    Slot 250001 → Validator A → Connection ✓         │
│    Slot 250002 → Validator A → Connection ✓         │
│    Slot 250003 → Validator B → Connection ✓         │
│    Slot 250004 → Validator C → Connection ✓         │
│    ...                                               │
└──────────────────────────────────────────────────────┘
```

**Benefits:**
- Eliminates QUIC handshake latency when leader rotates
- Connections ready before they're needed
- Configurable lookahead (default: 10 slots)

---

## Machine Gun Mode

For high-frequency transaction submission, Scramjet provides direct connection handles:

```
Traditional (slow):
  TX1: Connect → Send → Close
  TX2: Connect → Send → Close
  TX3: Connect → Send → Close

Machine Gun (fast):
  Connect once → [TX1, TX2, TX3, ...] → Close
```

This leverages **QUIC stream multiplexing** – multiple independent streams share a single connection, avoiding per-transaction handshake overhead.

---

## External Integrations

### 1. Solana RPC

- **Purpose**: Cluster topology, leader schedule, epoch info, blockhash
- **Crate**: `solana-client`
- **Default**: `https://api.mainnet-beta.solana.com`

### 2. Yellowstone Geyser gRPC

- **Purpose**: Real-time slot updates
- **Protocol**: gRPC over TLS with optional auth token
- **Crate**: `yellowstone-grpc-proto`

### 3. Solana TPU (QUIC)

- **Purpose**: Direct transaction submission
- **Protocol**: QUIC with TLS (Ed25519 client certificate)
- **ALPN**: `"solana-tpu"`
- **Crate**: `quinn`

---

## CLI Reference

### Commands

| Command | Description |
|---------|-------------|
| `monitor` | Continuously display current slot and leader IP |
| `fire` | Send a single transaction to current leader |
| `spam` | Send multiple transactions in rapid succession (machine gun mode) |

### Options

| Option | Description |
|--------|-------------|
| `-r, --rpc <URL>` | Override RPC endpoint |
| `--geyser <URL>` | Override Geyser gRPC endpoint |
| `-k, --keypair <PATH>` | Path to keypair (default: `~/.config/solana/id.json`) |
| `--recipient <PUBKEY>` | Recipient for transfer (default: self-transfer) |
| `--priority-fee <FEE>` | Priority fee in microlamports |
| `--count <N>` | Number of transactions (spam only) |

### Examples

```bash
# Monitor current slot and leader
scramjet-cli monitor

# Send a single transaction
scramjet-cli fire --keypair ./my-wallet.json --priority-fee 500000

# Spam 100 transactions
scramjet-cli spam --count 100 --priority-fee 1000000
```

---

## Configuration

All configuration is done via environment variables with sensible defaults.

### Network Endpoints

| Variable | Default | Description |
|----------|---------|-------------|
| `SOLANA_RPC_URL` | `https://api.mainnet-beta.solana.com` | RPC endpoint |
| `GEYSER_URL` | — | Yellowstone Geyser gRPC endpoint |

### Timing Intervals

| Variable | Default | Description |
|----------|---------|-------------|
| `RPC_POLL_INTERVAL_MS` | `400` | Slot polling interval (legacy mode) |
| `SCOUT_INTERVAL_MS` | `1000` | Connection pre-warming interval |
| `SCOUT_LOOKAHEAD_SLOTS` | `10` | Slots ahead to pre-warm |
| `MONITOR_INTERVAL_MS` | `400` | Monitor display refresh rate |

### QUIC Transport

| Variable | Default | Description |
|----------|---------|-------------|
| `QUIC_KEEP_ALIVE_SECS` | `5` | QUIC keep-alive interval |
| `QUIC_IDLE_TIMEOUT_SECS` | `10` | QUIC connection idle timeout |

### Transaction Defaults

| Variable | Default | Description |
|----------|---------|-------------|
| `DEFAULT_COMPUTE_UNIT_LIMIT` | `200000` | Compute budget per transaction |
| `DEFAULT_PRIORITY_FEE` | `100000` | Priority fee (microlamports) |

### Geyser Reconnection

| Variable | Default | Description |
|----------|---------|-------------|
| `GEYSER_RECONNECT_DELAY_MS` | `1000` | Initial reconnect delay |
| `GEYSER_MAX_RECONNECT_DELAY_MS` | `10000` | Max reconnect delay |

---

## Notable Implementation Patterns

### Lock-Free Slot Tracking

```rust
current_slot: Arc<AtomicU64>
// Read: Ordering::Relaxed for maximum performance
// Write: Atomic swap, no locks needed
```

### DashMap for Connection Cache

Uses the `dashmap` crate for lock-free concurrent HashMap access, enabling multiple tasks to read/write connections without contention.

### Fail-Fast Configuration

All configuration values are validated at startup:
- Minimum intervals prevent CPU spikes (50ms floor)
- Keep-alive must be less than idle timeout
- Compute unit limit must be > 0

Invalid configuration causes immediate startup failure with descriptive error messages.

### PKCS#8 Key Wrapping

Manually constructs PKCS#8 envelope for Ed25519 key conversion (Solana uses raw Ed25519, rustls requires PKCS#8):

```rust
const ED25519_PKCS8_HEADER: &[u8] = &[
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06,
    0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];
```

---

## Dependencies

### Core Runtime
- **tokio** – Async runtime
- **quinn** – QUIC implementation
- **rustls** – TLS for QUIC
- **rcgen** – Certificate generation

### Solana Ecosystem
- **solana-sdk** – Types (Keypair, Pubkey, Transaction)
- **solana-client** – RPC client
- **yellowstone-grpc-proto** – Geyser protocol definitions

### Utilities
- **clap** – CLI parsing
- **dashmap** – Lock-free concurrent HashMap
- **anyhow/thiserror** – Error handling
- **tonic** – gRPC client

---

## Performance Considerations

1. **Connection reuse** – QUIC connections are expensive to establish; Scramjet caches and reuses them
2. **Lock-free data structures** – Critical paths use atomics and DashMap to avoid mutex contention
3. **Pre-warming** – Scout ensures connections are ready before they're needed
4. **Stream multiplexing** – Machine gun mode amortizes connection overhead across many transactions
5. **Geyser over RPC** – Push-based slot updates are faster than polling

---

## Security Notes

- **Private keys** are loaded from disk and held in memory; ensure proper file permissions
- **Self-signed certificates** are used for QUIC identity; this is expected by Solana validators
- **No server verification** – Validators use ephemeral certificates, so client-side verification is disabled
