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
//! 1. Worker first observes pool price at 100 → initial trigger price = 85
//! 2. Price rises to 120 → trigger price trails up to 102
//! 3. Price rises to 150 → trigger price trails up to 127.5
//! 4. Price drops to 125 → below trigger price 127.5? Stop loss triggered! Exit position.
//!
//! The strategy captured gains from 100→125 instead of riding it back down.
//!
//! > **Note:** The initial trigger price is set from the pool price at the worker's
//! > first observation of the position, which may differ from the actual user entry
//! > price if there is a delay or rapid price movement between entry and observation.
//!
//! ## Configuration
//!
//! - `position_token`: The token being protected (what you're holding)
//! - `exit_token`: The token to swap into when TSL triggers
//! - `trail_percent`: How far below the peak the stop triggers (0.15 = 15%). The
//!   initial trigger is computed from the first observed pool price, not the exact
//!   entry price, so fast price moves between entry and observation can result in
//!   earlier-than-expected exits or weaker protection than anticipated.

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
    let key = trigger_price_key(&pool_ident);
    let now = config.network.to_unix_time(pool_state.slot);

    // Filter to strategies with positions in this pool
    let active: Vec<_> = strategies
        .iter()
        .filter(|s| pool_state.is_correct_pool(&s.order, &config.position_token, &config.exit_token))
        .filter(|s| asset_amount(&s.utxo, &config.position_token) > 0)
        .collect();

    let trigger_price = kv::get::<f64>(&key)?.unwrap_or(0.0);
    info!("pool update: price={pool_price}, trigger_price={trigger_price}");

    // No active positions - reset trigger price if it was set
    if active.is_empty() {
        if trigger_price > 0.0 {
            info!("no active positions; resetting trigger price");
            kv::set(&key, &0.0_f64)?;
        }
        return Ok(Ack);
    }

    // Initialize or trail trigger price upward
    let new_trigger = pool_price * (1.0 - config.trail_percent);
    let trigger_price = if trigger_price == 0.0 {
        info!(
            "initializing trigger price to {new_trigger} ({}% below {pool_price})",
            config.trail_percent * 100.0
        );
        kv::set(&key, &new_trigger)?;
        new_trigger
    } else if new_trigger > trigger_price {
        info!("trailing trigger price up to {new_trigger}");
        kv::set(&key, &new_trigger)?;
        new_trigger
    } else {
        trigger_price
    };

    // Check if TSL should trigger for each active strategy
    if pool_price < trigger_price {
        info!("TSL triggered: price {pool_price} < trigger {trigger_price}");
        for strategy in active {
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

#[balius_sdk::main]
fn main() -> Worker {
    balius_sdk::logging::init();

    Strategy::<StrategyConfig>::new()
        .on_new_pool_state(on_new_pool_state)
        .worker()
}
