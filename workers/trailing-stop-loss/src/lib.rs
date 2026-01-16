//! # Trailing Stop Loss (TSL) Strategy
//!
//! This strategy protects a token position with a trailing stop loss.
//!
//! ## How It Works
//!
//! The strategy expects to receive a UTxO that already contains the position token
//! (via atomic entry from the frontend). It then monitors price movements:
//!
//! - **Price goes up**: The peak price updates to the new high, and trigger price
//!   is recalculated as `peak_price * (1 - trail_percent)`. This locks in gains
//!   as the price rises.
//!
//! - **Price goes down**: If the price drops below the trigger price, the strategy
//!   exits the position, selling all `position_token` for `exit_token`.
//!
//! ## Example
//!
//! With `trail_percent = 0.15` (15%):
//!
//! 1. Worker first observes pool price at 100 → initial trigger price = 85
//! 2. Price rises to 120 → trigger price trails up to 102
//! 3. Price rises to 150 → trigger price trails up to 127.5
//! 4. Price drops to 125 → below trigger price 127.5? Stop loss triggered! Exit position.
//!
//! The strategy captured gains from 100→125 instead of riding it back down.
//!
//! > **Note:** The initial trigger price is set from either the configured `entry_price`
//! > (if provided) or the pool price at the worker's first observation of the position.
//! > When modifying an existing position, use the `get-peak-price` request handler to
//! > retrieve the current peak and pass it as `entry_price` to preserve trailing gains.
//!
//! ## Configuration
//!
//! - `position_token`: The token being protected (what you're holding)
//! - `exit_token`: The token to swap into when TSL triggers
//! - `trail_percent`: How far below the peak the stop triggers (0.15 = 15%)
//! - `entry_price`: Optional initial peak price. If set, used instead of discovering
//!   from pool price. Useful when modifying positions to preserve the previous peak.

mod config;

use std::time::Duration;

use balius_sdk::{
    Ack, Config, Json, Params, WorkerResult,
    _internal::Handler,
    wit,
};
use config::Config as StrategyConfig;
use serde::{Deserialize, Serialize};
use sundae_strategies::{
    ManagedStrategy, PoolState, Strategy, kv,
    types::{Interval, Order, OutputReference, TransactionId, asset_amount},
};
use tracing::info;

pub const PEAK_PRICE_PREFIX: &str = "peak_price:";

fn peak_price_key(output: &OutputReference) -> String {
    format!(
        "{}{}#{}",
        PEAK_PRICE_PREFIX,
        hex::encode(&output.transaction_id.0),
        output.output_index
    )
}

#[allow(clippy::ptr_arg)] // Signature must match NewPoolStateCallback type
fn on_new_pool_state(
    config: &Config<StrategyConfig>,
    pool_state: &PoolState,
    strategies: &Vec<ManagedStrategy>,
) -> WorkerResult<Ack> {
    let pool_price = pool_state.pool_datum.raw_price(&pool_state.utxo);
    let now = config.network.to_unix_time(pool_state.slot);

    // Filter to strategies with positions in this pool
    let active: Vec<_> = strategies
        .iter()
        .filter(|s| {
            pool_state.is_correct_pool(&s.order, &config.position_token, &config.exit_token)
        })
        .filter(|s| asset_amount(&s.utxo, &config.position_token) > 0)
        .collect();

    if active.is_empty() {
        return Ok(Ack);
    }

    // Process each strategy individually (per-strategy peak prices)
    for strategy in active {
        let key = peak_price_key(&strategy.output);
        let stored_peak: Option<f64> = kv::get::<f64>(&key)?;

        // Compute peak price from stored value or initialize from entry_price/pool_price
        let peak_price = match stored_peak {
            None => {
                // Use entry_price from config if provided, otherwise use current pool price
                let initial_peak = config.entry_price.unwrap_or(pool_price);
                info!(
                    "initializing peak price for {}#{} to {} (entry_price: {:?})",
                    hex::encode(&strategy.output.transaction_id.0),
                    strategy.output.output_index,
                    initial_peak,
                    config.entry_price
                );
                kv::set(&key, &initial_peak)?;
                initial_peak
            }
            Some(peak) if pool_price > peak => {
                // Update peak price (only goes up)
                info!(
                    "updating peak price for {}#{} to {}",
                    hex::encode(&strategy.output.transaction_id.0),
                    strategy.output.output_index,
                    pool_price
                );
                kv::set(&key, &pool_price)?;
                pool_price
            }
            Some(peak) => peak,
        };

        // Calculate trigger from peak - always uses current trail_percent
        let trigger_price = peak_price * (1.0 - config.trail_percent);

        info!(
            "strategy {}#{}: price={}, peak={}, trigger={}",
            hex::encode(&strategy.output.transaction_id.0),
            strategy.output.output_index,
            pool_price,
            peak_price,
            trigger_price
        );

        // Check if TSL should trigger for this strategy
        if pool_price < trigger_price {
            info!(
                "TSL triggered for {}#{}: price {} < trigger {}",
                hex::encode(&strategy.output.transaction_id.0),
                strategy.output.output_index,
                pool_price,
                trigger_price
            );
            trigger_exit(config, now, strategy)?;
        }
    }

    Ok(Ack)
}

/// Exit: Swap position_token back to exit_token when TSL triggers
fn trigger_exit(
    config: &Config<StrategyConfig>,
    now: u64,
    strategy: &ManagedStrategy,
) -> WorkerResult<Ack> {
    let valid_for = Duration::from_secs(20 * 60);
    // Validity range extends into the past to handle clock skew and tx propagation delays
    let validity_range = Interval::inclusive_range(
        now.saturating_sub(valid_for.as_millis() as u64),
        now.saturating_add(valid_for.as_millis() as u64),
    );

    let swap = Order::Swap {
        offer: (
            config.position_token.policy_id.clone(),
            config.position_token.asset_name.clone(),
            asset_amount(&strategy.utxo, &config.position_token),
        ),
        min_received: (
            config.exit_token.policy_id.clone(),
            config.exit_token.asset_name.clone(),
            1,
        ),
    };

    sundae_strategies::submit_execution(&config.network, &strategy.output, validity_range, swap)?;
    info!("exit order submitted");
    Ok(Ack)
}

// ============================================================================
// get-peak-price request handler
// ============================================================================

/// Request parameters for get-peak-price
#[derive(Deserialize)]
struct GetPeakPriceParams {
    /// Transaction hash of the strategy UTxO (hex-encoded)
    tx_hash: String,
    /// Output index of the strategy UTxO
    output_index: u64,
}

/// Response for get-peak-price
#[derive(Serialize)]
struct GetPeakPriceResponse {
    /// The current peak price for this strategy, or null if not found
    peak_price: Option<f64>,
}

/// Handler for get-peak-price requests
#[derive(Clone)]
struct GetPeakPriceHandler;

impl Handler for GetPeakPriceHandler {
    fn handle(
        &self,
        _config: wit::Config,
        event: wit::Event,
    ) -> Result<wit::Response, wit::HandleError> {
        let params: Params<GetPeakPriceParams> = event
            .try_into()
            .map_err(|_| wit::HandleError {
                message: "invalid request parameters".to_string(),
                code: 400,
            })?;

        let tx_hash_bytes = hex::decode(&params.tx_hash)
            .map_err(|_| wit::HandleError {
                message: "invalid tx_hash hex encoding".to_string(),
                code: 400,
            })?;

        let output_ref = OutputReference {
            transaction_id: TransactionId(tx_hash_bytes),
            output_index: params.output_index,
        };

        let key = peak_price_key(&output_ref);
        let peak_price = kv::get::<f64>(&key)
            .map_err(|e| wit::HandleError {
                message: e.to_string(),
                code: 500,
            })?;

        info!(
            "get-peak-price for {}#{}: {:?}",
            params.tx_hash, params.output_index, peak_price
        );

        let response = GetPeakPriceResponse { peak_price };
        let json = Json(response);

        json.try_into()
            .map_err(|e: balius_sdk::Error| wit::HandleError {
                message: e.to_string(),
                code: 500,
            })
    }
}

#[balius_sdk::main]
fn main() -> Worker {
    balius_sdk::logging::init();

    Strategy::<StrategyConfig>::new()
        .on_new_pool_state(on_new_pool_state)
        .worker_with(|w| w.with_request_handler("get-peak-price", GetPeakPriceHandler))
}
