# Sundae Strategies - Skills Reference

This document provides a comprehensive reference for understanding and working with the Sundae Strategies framework.

## Overview

Sundae Strategies is a Rust-based framework for building automated trading strategies on the Cardano blockchain for Sundae v3 orders. The project compiles to WebAssembly (WASM) and runs on the Balius runtime from TxPipe.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│ Blockchain (Cardano)                                        │
│   Strategy Order UTxO ◄── Contains OrderDatum               │
└────────────────────────────────────────────────────────────┘
                │ Chain Sync Events
                ▼
┌──────────────────────────────────────────────────────────────┐
│ Balius Runtime                                               │
│   WASM Worker Module                                         │
│   ├── on_new_strategy()   → Discovers orders                 │
│   ├── on_new_pool_state() → Monitors prices                  │
│   └── on_each_tx()        → Time-based logic                 │
│                                                              │
│   KV Store: managed_orders, peak_price:...                   │
│   Signing: sign_payload("default", bytes)                    │
└───────┬──────────────────────────────────────────────────────┘
        │ Signed StrategyExecution
        ▼
┌───────────────────────┐
│ SSE Relay → Scoopers  │
└───────────────────────┘
```

## Project Structure

| Directory | Purpose |
|-----------|---------|
| `sundae-strategies/` | Core library with Strategy builder and utilities |
| `workers/trailing-stop-loss/` | Trailing stop-loss strategy implementation |
| `workers/stop-loss/` | Basic stop-loss strategy |
| `workers/dollar-cost-average/` | DCA periodic buying strategy |
| `balius-server/` | HTTP server for local development (deprecated, use `baliusd`) |
| `balius-worker-builder/` | CLI to compile workers to WASM |

## How Strategies Work

### Core Concept

A **Strategy** is a special Sundae v3 order type that delegates execution authority to a public key:

1. User creates an order with `Order::Strategy { auth: Signature { signer } }`
2. This locks the order on-chain with a specified signer's public key
3. The authorized signer can later submit a `StrategyExecution` containing trade details
4. Scoopers validate the signature and execute the swap

### Strategy Lifecycle

```
[SETUP]      User places Strategy order with public key
     ↓
[DISCOVERY]  Worker observes UTxO via Balius blockchain sync
     ↓
[MONITORING] Worker tracks pool prices and managed orders
     ↓
[DECISION]   Worker logic determines if trigger conditions are met
     ↓
[EXECUTION]  Worker signs and submits StrategyExecution to SSE relay
     ↓
[COMPLETION] Scoopers process swap on-chain
     ↓
[CLEANUP]    Worker detects order consumed, updates internal state
```

## Core Library API

### Strategy Builder

```rust
use sundae_strategies::{Strategy, Config, ManagedStrategy, PoolState};

Strategy::<MyConfig>::new()
    .on_new_strategy(on_new_strategy)
    .on_new_pool_state(on_pool_state)
    .on_each_tx(on_each_tx)
    .worker()
```

### Callback Types

```rust
// Triggered when a new strategy order is discovered
NewStrategyCallback<T> = fn(&Config<T>, &ManagedStrategy) -> WorkerResult<Ack>

// Triggered when a pool's state changes (price updates)
NewPoolStateCallback<T> = fn(&Config<T>, &PoolState, &Vec<ManagedStrategy>) -> WorkerResult<Ack>

// Triggered for every transaction on the network
EachTxCallback<T> = fn(&Config<T>, &Tx, &Vec<ManagedStrategy>) -> WorkerResult<Ack>
```

### Key Structs

**ManagedStrategy** - Tracks owned strategy orders:
- `slot` - Block slot when first observed
- `output` - OutputReference (tx hash + index)
- `utxo` - Full UTXO content
- `order` - Parsed OrderDatum

**PoolState** - Tracks Sundae pool state:
- `slot` - Block slot when observed
- `output` - UTXO reference
- `pool_datum` - Parsed PoolDatum with assets and fees
- Methods: `is_correct_pool()`, `price()`, `get_validity_range()`

**OrderDatum** - On-chain order structure:
- `pool_ident` - Optional specific pool identifier
- `owner` - Owner's signature requirements
- `details` - Order type (Strategy or Swap)

### Submitting Executions

```rust
sundae_strategies::submit_execution(
    network,
    output_ref,      // Original strategy order UTxO
    validity_range,  // Time window (typically 20 min each direction)
    swap_details,    // The actual Swap to execute
)?;
```

## Asset Identifier Format

Tokens are identified as `policy_id.asset_name`:

| Format | Token |
|--------|-------|
| `"."` | ADA (native currency) |
| `"policy_hex.asset_hex"` | Any native token |

Example: `"99b071ce8580d6a3a11b4902145adb8bfd0d2a03935af8cf66403e15.524245525259"`

## Worker Implementations

### Trailing Stop Loss

**Purpose:** Protects positions with a trailing stop that locks in gains.

**Config:**
```json
{
  "network": "preview",
  "position_token": "policy.asset",
  "exit_token": ".",
  "trail_percent": 0.15,
  "entry_price": null
}
```

**Logic:**
1. Maintains peak price per strategy in KV store (`peak_price:{tx_hash}#{index}`)
2. On pool state change:
   - Initialize peak from `entry_price` or current price
   - If price > peak → update peak (only increases)
   - Calculate trigger = peak × (1 - trail_percent)
   - If price < trigger → submit exit swap

**Nuance:** The `entry_price` parameter allows preserving peak price when modifying positions.

### Stop Loss

**Purpose:** Static stop-loss at fixed price threshold.

**Config:**
```json
{
  "network": "preview",
  "token_a": ".",
  "token_a_decimals": 6,
  "token_b": "policy.asset",
  "token_b_decimals": 0,
  "sell_token": "policy.asset",
  "execution_price": 0.000168
}
```

**Logic:** Triggers sell when price drops below `execution_price`.

### Dollar Cost Average

**Purpose:** Periodic buying at fixed intervals.

**Config:**
```json
{
  "network": "preview",
  "interval": 10,
  "offer_token": ".",
  "offer_amount": 1000000,
  "receive_token": "policy.asset",
  "receive_amount_min": 1
}
```

**Logic:** Uses `on_each_tx()` callback (time-based, not price-based).

## State Management

### Managed Orders

- Key: `managed_orders` in KV store
- Value: Vector of `ManagedStrategy`
- Updated on UTxO discovery and transaction processing

### Per-Strategy State

Pattern for individual order state:
```rust
let key = format!("peak_price:{}#{}", tx_hash, index);
kv::set(&key, &peak_price)?;
```

## Network Configuration

| Network | Slot Offset | Relay URL |
|---------|-------------|-----------|
| Preview | 1666656000 | `http://sse-relay.preview.sundae.fi/publish` |
| Mainnet | 1591566291 | `http://sse-relay.sundae.fi/publish` |

**Slot to UNIX time:** `unix_ms = (slot + offset) * 1000`

## Validity Ranges

All strategies use 20-minute validity windows:

```rust
let valid_for = Duration::from_secs(20 * 60);
Interval::inclusive_range(
    now.saturating_sub(valid_for.as_millis() as u64),
    now.saturating_add(valid_for.as_millis() as u64),
)
```

## Price Calculations

**Raw price:** `reserves_a / reserves_b`

**Decimal-adjusted:** `raw_price * 10^(decimals_a - decimals_b)`

## Development Workflow

### Building Workers

```bash
# Install builder
cargo install --git https://github.com/SundaeSwap-finance/sundae-strategies balius-worker-builder

# Build worker
just build
# or
balius-worker-builder
```

### Running Locally

```bash
# Start worker
baliusd

# Get public key
baliusd show-keys default

# Debug mode (replays events)
baliusd --debug
```

### Creating New Strategies

1. Generate from template:
   ```bash
   cargo generate SundaeSwap-finance/sundae-strategy-template
   ```

2. Implement config struct and callbacks

3. Set in Cargo.toml:
   ```toml
   [lib]
   crate-type = ["cdylib"]
   ```

4. Compile and deploy

## Adding HTTP Handlers

```rust
Strategy::<Config>::new()
    .on_new_pool_state(on_pool_state)
    .worker_with(|worker| {
        worker.with_request_handler("my-handler", my_handler)
    })
```

## Error Handling

- All callbacks return `WorkerResult<Ack>`
- Unparseable datums gracefully skip with `Ok(Ack)`
- Errors propagate and may halt worker processing

## Key Nuances

1. **Order Discovery:** Workers only manage orders where the signer matches the worker's public key

2. **State Persistence:** KV store survives restarts; use unique keys per order

3. **Execution Signing:** Uses ed25519 via `balius_sdk::wit::balius::app::sign::sign_payload`

4. **Pool Matching:** Always verify `pool_state.is_correct_pool(&order)` before executing

5. **Transaction Filtering:** `on_each_tx` filters spent orders automatically

6. **Decimal Handling:** Raw prices don't account for token decimals; adjust manually

7. **Single Instance:** `baliusd` runs one worker per process; use multiple instances for parallelism

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| balius-sdk | 0.5 | WASM runtime integration |
| balius-runtime | 0.5 | Worker lifecycle management |
| plutus-parser | git | Plutus datum parsing |
| pallas-crypto | 0.32 | Cardano cryptography |
