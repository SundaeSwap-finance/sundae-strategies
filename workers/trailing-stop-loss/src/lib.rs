//! # Trailing Stop Loss (TSL) Strategy
//!
//! This strategy protects a token position with a trailing stop loss.
//!
//! ## How It Works
//!
//! The strategy expects to receive a UTxO that already contains the position token
//! (via atomic entry from the frontend). It then monitors price movements:
//!
//! - **Price goes up**: The trigger price "trails" upward toward
//!   `current_price * (1 - trail_percent)` whenever the price reaches a new high,
//!   and stays unchanged when the price moves sideways or down. This locks in gains
//!   as the price rises.
//!
//! - **Price goes down**: If the price drops below the trigger price, the strategy
//!   exits the position, selling all `position_token` for `exit_token`.
//!
//! ## Example
//!
//! With `trail_percent = 0.15` (15%):
//!
//! 1. Position entered at price 100 → trigger price = 85
//! 2. Price rises to 120 → trigger price trails up to 102
//! 3. Price rises to 150 → trigger price trails up to 127.5
//! 4. Price drops to 125 → below trigger price 127.5? Stop loss triggered! Exit position.
//!
//! The strategy captured gains from 100→125 instead of riding it back down.
//!
//! ## Configuration
//!
//! - `position_token`: The token being protected (what you're holding)
//! - `exit_token`: The token to swap into when TSL triggers
//! - `trail_percent`: How far below the peak the stop triggers (0.15 = 15%)

mod config;

use std::time::Duration;

use balius_sdk::{Ack, Config, WorkerResult};
use config::Config as StrategyConfig;
use sundae_strategies::{
    ManagedStrategy, PoolState, Strategy, kv,
    types::{Interval, Order, asset_amount},
};
use tracing::info;

pub const TRIGGER_PRICE_PREFIX: &str = "trigger_price:";

fn trigger_price_key(pool_ident: &str) -> String {
    format!("{TRIGGER_PRICE_PREFIX}{pool_ident}")
}

fn on_new_pool_state(
    config: &Config<StrategyConfig>,
    pool_state: &PoolState,
    strategies: &Vec<ManagedStrategy>,
) -> WorkerResult<Ack> {
    let pool_price = pool_state.pool_datum.raw_price(&pool_state.utxo);
    let pool_ident = hex::encode(&pool_state.pool_datum.identifier);
    let now = config.network.to_unix_time(pool_state.slot);

    // Get the current trigger price for this pool (0.0 if not yet set)
    // Made mutable so we can update it after initialization
    let mut trigger_price = kv::get::<f64>(&trigger_price_key(&pool_ident))?.unwrap_or(0.0);

    info!(
        "pool update: price={}, trigger_price={}",
        pool_price, trigger_price
    );

    // Track if any strategy has a position (for trigger price updates)
    let mut any_has_position = false;
    // Track if this is the first observation (trigger price was just initialized)
    // All strategies should skip exit checks on the first observation
    let mut first_observation = false;

    // Initialize trigger_price if this is the first time seeing a position with any strategy
    if trigger_price.abs() < f64::EPSILON {
        // Check if any strategy has a position
        let has_any_position = strategies
            .iter()
            .any(|s| asset_amount(&s.utxo, &config.position_token) > 0);

        if has_any_position {
            let initial_trigger_price = pool_price * (1. - config.trail_percent);
            info!(
                "initializing trigger price to {} ({}% below current price {})",
                initial_trigger_price,
                config.trail_percent * 100.0,
                pool_price
            );
            kv::set(&trigger_price_key(&pool_ident), &initial_trigger_price)?;
            trigger_price = initial_trigger_price;
            first_observation = true;
            any_has_position = true;
        }
    }

    // Process each strategy based on its token holdings (skip exit checks on first observation)
    if !first_observation {
        for strategy in strategies {
            let position_amount = asset_amount(&strategy.utxo, &config.position_token);

            // Only act if strategy has position tokens to protect
            if position_amount > 0 {
                any_has_position = true;

                // Check if TSL should trigger
                if pool_price < trigger_price {
                    info!(
                        "TSL triggered: price {} < trigger_price {}. Exiting position...",
                        pool_price, trigger_price
                    );
                    trigger_exit(config, now, strategy)?;
                }
            }
        }
    }

    // Update the trailing trigger price (only goes up) if any strategy has a position
    // Skip if trigger_price is effectively 0.0 (will be initialized above on next call)
    if any_has_position && trigger_price.abs() > f64::EPSILON {
        let new_trigger_price = f64::max(trigger_price, pool_price * (1. - config.trail_percent));

        if (new_trigger_price - trigger_price).abs() > f64::EPSILON {
            info!("trailing trigger price up to {}", new_trigger_price);
            kv::set(&trigger_price_key(&pool_ident), &new_trigger_price)?;
        }
    } else if !any_has_position && trigger_price.abs() > f64::EPSILON {
        // All positions have been exited; clear the stored trigger price so that
        // future entries can reinitialize the trailing stop from the new entry price.
        info!("no active positions; resetting trigger price to 0.0");
        kv::set(&trigger_price_key(&pool_ident), &0.0_f64)?;
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

#[balius_sdk::main]
fn main() -> Worker {
    balius_sdk::logging::init();

    Strategy::<StrategyConfig>::new()
        .on_new_pool_state(on_new_pool_state)
        .worker()
}
