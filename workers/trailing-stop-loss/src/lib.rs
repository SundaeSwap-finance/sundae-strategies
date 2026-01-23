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
//! - `slippage_tolerance`: Maximum acceptable slippage on exit (0.03 = 3%)
//! - `entry_price`: Optional initial peak price. If set, used instead of discovering
//!   from pool price. Useful when modifying positions to preserve the previous peak.
//!
//! ## Price Calculation
//!
//! Price is always calculated as: "how much exit_token per 1 position_token"
//! This means when your position_token loses value (you get less exit_token for it),
//! the price goes DOWN, which is when TSL should trigger.

mod config;

use std::time::Duration;

use balius_sdk::{_internal::Handler, Ack, Config, Json, Params, WorkerResult, wit};
use config::Config as StrategyConfig;
use serde::{Deserialize, Serialize};
use sundae_strategies::{
    ManagedStrategy, PoolState, Strategy, kv,
    types::{AssetId, Interval, Order, OutputReference, TransactionId, asset_amount},
};
use tracing::info;

/// Key prefix for storing peak prices per strategy
pub const PEAK_PRICE_PREFIX: &str = "peak_price:";

fn peak_price_key(output: &OutputReference) -> String {
    format!(
        "{}{}#{}",
        PEAK_PRICE_PREFIX,
        hex::encode(&output.transaction_id.0),
        output.output_index
    )
}

// ============================================================================
// Price Calculation
// ============================================================================

/// Calculate the price of position_token in terms of exit_token.
///
/// This is the critical function that determines the "price" we track for TSL.
/// We always want: "how much exit_token do I get for 1 position_token?"
///
/// ## Why This Matters
///
/// Pool stores assets as (asset_a, asset_b) with reserves. The raw_price from
/// PoolDatum gives us `reserves_a / reserves_b` which is "asset_a per asset_b".
///
/// But we need "exit_token per position_token":
/// - If position_token is asset_b: raw_price already gives us exit_per_position ✓
/// - If position_token is asset_a: raw_price gives us position_per_exit (inverted!)
///   so we must return 1/raw_price
///
/// ## Example
///
/// Pool: [ADA, SUNDAE] with 10,000 ADA and 1,000 SUNDAE
/// raw_price = 10,000/1,000 = 10 (meaning "10 ADA per SUNDAE")
///
/// - User protecting SUNDAE, exiting to ADA:
///   position=SUNDAE(asset_b), exit=ADA(asset_a)
///   We want "ADA per SUNDAE" = 10 → use raw_price directly ✓
///
/// - User protecting ADA, exiting to SUNDAE:
///   position=ADA(asset_a), exit=SUNDAE(asset_b)
///   We want "SUNDAE per ADA" = 0.1 → use 1/raw_price ✓
fn get_position_price(pool_state: &PoolState, position_token: &AssetId) -> f64 {
    let raw_price = pool_state.pool_datum.raw_price(&pool_state.utxo);

    // raw_price = reserves_a / reserves_b = "how much asset_a per 1 asset_b"
    // We want: "how much exit_token per 1 position_token"
    let (pool_asset_a, _pool_asset_b) = &pool_state.pool_datum.assets;
    let position_is_asset_a =
        position_token.policy_id == pool_asset_a.0 && position_token.asset_name == pool_asset_a.1;

    if position_is_asset_a {
        // position is asset_a, exit is asset_b
        // raw_price = asset_a/asset_b = position/exit (INVERTED from what we want)
        // We want exit/position, so invert
        if raw_price == 0.0 {
            0.0
        } else {
            1.0 / raw_price
        }
    } else {
        // position is asset_b, exit is asset_a
        // raw_price = asset_a/asset_b = exit/position (exactly what we want!)
        raw_price
    }
}

#[allow(clippy::ptr_arg)] // Signature must match NewPoolStateCallback type
fn on_new_pool_state(
    config: &Config<StrategyConfig>,
    pool_state: &PoolState,
    strategies: &Vec<ManagedStrategy>,
) -> WorkerResult<Ack> {
    // Calculate price correctly based on token ordering in pool
    let pool_price = get_position_price(pool_state, &config.position_token);
    let now = config.network.to_unix_time(pool_state.slot);
    tracing::info!("New pool price: {pool_price}");

    // Filter to strategies with positions in this pool
    let active: Vec<_> = strategies
        .iter()
        .filter(|s| {
            pool_state.is_correct_pool(&s.order, &config.position_token, &config.exit_token)
        })
        .filter(|s| asset_amount(&s.utxo, &config.position_token) > 0)
        .collect();

    if active.is_empty() {
        tracing::info!("No active strategies");
        return Ok(Ack);
    }

    // Process each strategy individually (per-strategy peak prices)
    for strategy in active {
        let peak_key = peak_price_key(&strategy.output);
        let stored_peak = match kv::get::<f64>(&peak_key) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("kv get failed: {e}");
                None
            }
        };

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
                if let Err(e) = kv::set(&peak_key, &initial_peak) {
                    tracing::error!(
                        "failed to initialize peak price for {}#{}: {}",
                        hex::encode(&strategy.output.transaction_id.0),
                        strategy.output.output_index,
                        e
                    );
                }
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
                if let Err(e) = kv::set(&peak_key, &pool_price) {
                    tracing::error!(
                        "failed to update peak price for {}#{}: {}",
                        hex::encode(&strategy.output.transaction_id.0),
                        strategy.output.output_index,
                        e
                    );
                }
                pool_price
            }
            Some(peak) => peak,
        };

        // Calculate trigger from peak - always uses current trail_percent
        let trigger_price = peak_price * (1.0 - config.trail_percent);

        info!(
            "strategy {}#{}: price={:.8}, peak={:.8}, trigger={:.8}",
            hex::encode(&strategy.output.transaction_id.0),
            strategy.output.output_index,
            pool_price,
            peak_price,
            trigger_price
        );

        // Check if TSL should trigger for this strategy
        if pool_price < trigger_price {
            info!(
                "TSL triggered for {}#{}: price {:.8} < trigger {:.8}",
                hex::encode(&strategy.output.transaction_id.0),
                strategy.output.output_index,
                pool_price,
                trigger_price
            );
            if let Err(e) = trigger_exit(config, now, strategy, trigger_price) {
                tracing::error!(
                    "failed to trigger exit for {}#{}: {}",
                    hex::encode(&strategy.output.transaction_id.0),
                    strategy.output.output_index,
                    e
                );
            }
        }
    }

    Ok(Ack)
}

/// Exit: Swap position_token back to exit_token when TSL triggers
///
/// ## Slippage Protection
///
/// We calculate a minimum received amount based on:
/// - The amount of position_token being sold
/// - The trigger_price (exit_token per position_token)
/// - The configured slippage_tolerance
///
/// This ensures the user doesn't get an unexpectedly bad fill if the price
/// drops further between trigger and execution.
///
/// Example: Selling 1000 SUNDAE at trigger_price=8 ADA/SUNDAE with 3% slippage:
/// - Expected: 1000 * 8 = 8000 ADA
/// - Minimum:  8000 * (1 - 0.03) = 7760 ADA
fn trigger_exit(
    config: &Config<StrategyConfig>,
    now: u64,
    strategy: &ManagedStrategy,
    trigger_price: f64,
) -> WorkerResult<Ack> {
    let valid_for = Duration::from_secs(20 * 60);
    // Validity range extends into the past to handle clock skew and tx propagation delays
    let validity_range = Interval::inclusive_range(
        now.saturating_sub(valid_for.as_millis() as u64),
        now.saturating_add(valid_for.as_millis() as u64),
    );

    let position_amount = asset_amount(&strategy.utxo, &config.position_token);

    // Calculate minimum received with slippage protection
    // trigger_price = exit_token per position_token
    // expected_output = position_amount * trigger_price
    // min_output = expected_output * (1 - slippage_tolerance)
    let expected_output = position_amount as f64 * trigger_price;
    let min_received = (expected_output * (1.0 - config.slippage_tolerance)) as u64;

    // Ensure we receive at least 1 unit (sanity check)
    let min_received = min_received.max(1);

    info!(
        "exit order: selling {} {} for min {} {} (trigger_price={:.8}, slippage={}%)",
        position_amount,
        config.position_token.name_to_string(),
        min_received,
        config.exit_token.name_to_string(),
        trigger_price,
        config.slippage_tolerance * 100.0
    );

    let swap = Order::Swap {
        offer: (
            config.position_token.policy_id.clone(),
            config.position_token.asset_name.clone(),
            position_amount,
        ),
        min_received: (
            config.exit_token.policy_id.clone(),
            config.exit_token.asset_name.clone(),
            min_received,
        ),
    };

    if let Err(e) =
        sundae_strategies::submit_execution(&config.network, &strategy.output, validity_range, swap)
    {
        tracing::error!(
            "failed to submit exit execution for {}#{}: {}",
            hex::encode(&strategy.output.transaction_id.0),
            strategy.output.output_index,
            e
        );
        return Ok(Ack);
    }
    info!("exit order submitted successfully");
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
        let params: Params<GetPeakPriceParams> =
            event.try_into().map_err(|_| wit::HandleError {
                message: "invalid request parameters".to_string(),
                code: 400,
            })?;

        let tx_hash_bytes = hex::decode(&params.tx_hash).map_err(|_| wit::HandleError {
            message: "invalid tx_hash hex encoding".to_string(),
            code: 400,
        })?;

        let output_ref = OutputReference {
            transaction_id: TransactionId(tx_hash_bytes),
            output_index: params.output_index,
        };

        let key = peak_price_key(&output_ref);
        let peak_price = kv::get::<f64>(&key).map_err(|e| wit::HandleError {
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
