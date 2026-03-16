//! # Symmetrical Grid Strategy
//!
//! This strategy provides symmetric liquidity around a fixed center price
//! using a static grid of limit-style orders.
//!
//! ## How It Works
//!
//! The strategy expects to receive a UTxO that contains both the strategy
//! and the base token with approximately equal value at initialization.
//!
//! Using the observed pool price at startup, the strategy computes a fixed
//! center price and derives a symmetric grid of price levels above and below
//! that center.
//!
//! The grid is static and does not move once initialized.
//!
//! - **Price moves up**: The strategy checks how many grid levels were crossed
//!   since the previous pool price and sells one fixed inventory slice of the
//!   strategy token per level crossed.
//!
//! - **Price moves down**: The strategy checks how many grid levels were crossed
//!   since the previous pool price and buys back one fixed inventory slice of the
//!   strategy token per level crossed using the base token.
//!
//! ## Example
//!
//! With `spacing_percent = 0.05` (5%), `levels_per_side = 3`:
//!
//! Initial balances:
//! - strategy_token: 100.00
//! - base_token:     100.00
//!
//! 1. Worker first observes pool price at 1 → calculates grid lines with 5% space 3 per side
//!    ------- 1.15 -------
//!    ------- 1.10 -------
//!    ------- 1.05 -------
//!    ------- 1.00 ------- <-- center price (reference only)
//!    ------- 0.95 -------
//!    ------- 0.90 -------
//!    ------- 0.85 -------
//!
//! 2. Price rises to 1.101:
//!    - Two grid levels (1.05 and 1.10) are crossed upward.
//!    - The strategy executes both grid fills in a single transaction,
//!      selling two fixed inventory slices of `strategy_token`.
//!    
//!    New balances:
//!    - strategy_token: 33.34
//!    - base_token:     171.66 (100 + 33.33 * 1.05 + 33.33 * 1.10)
//!
//!
//! 3. Price then drops to 1
//!    - Two previously filled grid levels (1.10, 1.05) are recrossed
//!      in the opposite direction.
//!    - The strategy executes two grid fills in a single transaction,
//!      buying back the previously sold `strategy_token`.
//!    - Crossing the center price does not trigger a grid fill.
//!
//!    New balances:
//!    - strategy_token: 95.38 (33.34 + 33.33 / 1.10 + 33.33 / 1.05)
//!    - base_token:     105.00 (171.66 - 33.33 * 2)
//!
//! 4. User closes grid strategy and receives the final balances.
//!
//! > **Note:** The center price is set from the pool price at the worker's
//! > first observation of the position, which may differ from the actual user entry
//! > price if there is a delay or rapid price movement between entry and observation.
//!
//! ## Configuration
//!
//! - `strategy_token`: The token traded by the grid. It is sold as price moves up and
//!   bought back as price moves down.
//! - `base_token`: The counter asset used to settle trades and hold proceeds between fills.
//! - `spacing_percent`: Percentage distance between adjacent grid levels (e.g. `0.05` = 5%).
//! - `levels_per_side`: Number of grid levels placed above and below the center price.

mod config;

use balius_sdk::{Ack, Config, WorkerResult};
use config::Config as StrategyConfig;
use sundae_strategies::{
    ManagedStrategy, PoolState, Strategy, kv,
    types::{Interval, Order, asset_amount},
};
use tracing::info;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GridState {
    center_price: f64,
    line_offset: i64,
    initial_strategy_amount: u64,
    initial_base_amount: u64,
}

impl GridState {
    fn new(strategy: &ManagedStrategy, pool_price: f64, config: &StrategyConfig) -> Self {
        Self {
            center_price: pool_price,
            line_offset: 0,
            initial_strategy_amount: asset_amount(&strategy.utxo, &config.strategy_token),
            initial_base_amount: asset_amount(&strategy.utxo, &config.base_token),
        }
    }
}

// GridState is keyed by a blake3 hash of the full strategy config.
//
// Design requirements:
// - The strategy config must be treated as immutable. Changing the config
//   creates a new key and discards all existing state (including `line_offset`),
//   which may lead to unexpected losses.
// - The frontend must prevent deploying multiple strategies with identical
//   configs, as they would share the same GridState.
//
// The center price is included to reduce collisions. Additional fields
// (e.g. `initial_strategy_amount`) can be added if stronger uniqueness
// guarantees are required.
fn grid_state_key(config: &StrategyConfig) -> Result<String, serde_json::Error> {
    let bytes = serde_json::to_vec(config)?;
    let hash = blake3::hash(&bytes);
    let key = hex::encode(hash.as_bytes());
    Ok(format!("grid_state:{key}"))
}

fn compute_grid_prices(center_price: f64, spacing_percent: f64, levels_per_side: u64) -> Vec<f64> {
    let step = 1.0 + spacing_percent;

    let mut prices = Vec::with_capacity((levels_per_side * 2) as usize);

    // Below center
    for i in (1..=levels_per_side).rev() {
        prices.push(center_price / step.powi(i as i32));
    }

    // Above center
    for i in 1..=levels_per_side {
        prices.push(center_price * step.powi(i as i32));
    }

    prices
}

pub fn compute_crossed_prices(
    grid_prices: &[f64],
    previous_offset: i64,
    price: f64,
) -> (i64, Vec<f64>) {
    let levels_per_side = (grid_prices.len() / 2) as i64;

    let new_index = grid_prices.iter().take_while(|p| **p < price).count() as i64;
    let new_offset = new_index - levels_per_side;
    let delta = new_offset - previous_offset;

    let mut crossed = Vec::new();

    if delta > 0 {
        for step in 1..=delta {
            let idx = (levels_per_side + previous_offset + step - 1) as usize;
            crossed.push(grid_prices[idx]);
        }
    } else if delta < 0 {
        for step in 1..=(-delta) {
            let idx = (levels_per_side + previous_offset - step) as usize;
            crossed.push(grid_prices[idx]);
        }
    }

    (new_offset, crossed)
}

fn get_or_init_grid_state(
    key: &str,
    strategy: &ManagedStrategy,
    pool_price: f64,
    config: &StrategyConfig,
) -> Result<GridState, balius_sdk::Error> {
    match kv::get::<GridState>(key)? {
        Some(state) => Ok(state),
        None => {
            let state = GridState::new(strategy, pool_price, config);
            kv::set(key, &state)?;
            Ok(state)
        }
    }
}

fn on_new_pool_state(
    config: &Config<StrategyConfig>,
    pool_state: &PoolState,
    strategies: &Vec<ManagedStrategy>,
) -> WorkerResult<Ack> {
    let pool_price = pool_state.pool_datum.raw_price(&pool_state.utxo);

    tracing::info!("Found new pool price: {pool_price}");

    for s in strategies {
        // Filter for active strategies
        if pool_state.is_correct_pool(&s.order, &config.strategy_token, &config.base_token) {
            tracing::info!("Strategy found with the correct pool");
            // Get current UTxO balance for `strategy_token` and `base_token`
            let strategy_amt = asset_amount(&s.utxo, &config.strategy_token);
            tracing::info!("Strategy amount: {strategy_amt}");
            let base_amt = asset_amount(&s.utxo, &config.base_token);
            tracing::info!("Base amount: {base_amt}");
            if strategy_amt == 0 && base_amt == 0 {
                continue;
            }

            // Get center price and current line offset
            let key = grid_state_key(config)?;
            let mut grid_state = get_or_init_grid_state(&key, s, pool_price, config)?;
            tracing::info!("Grid state: {:?}", grid_state);

            // Compute grid lines
            let grid_prices = compute_grid_prices(
                grid_state.center_price,
                config.spacing_percent,
                config.levels_per_side,
            );

            tracing::info!("Computed grid lines: {:?}", grid_prices);

            // Check which grid lines (if any) were crossed
            let (new_offset, crossed_prices) =
                compute_crossed_prices(&grid_prices, grid_state.line_offset, pool_price);

            tracing::info!("Crossed grids: {:?}", crossed_prices);

            // Execute buy or sell depending on direction of the new offset
            if !crossed_prices.is_empty() {
                tracing::info!("Crossed {} grid lines", crossed_prices.len());
                let validity_range = pool_state.get_validity_range(&config.network, 20);
                if new_offset > grid_state.line_offset {
                    // Compute `strategy_token` to sell per grid line
                    let sell_per_grid = grid_state.initial_strategy_amount / config.levels_per_side;
                    if sell_per_grid == 0 {
                        continue;
                    }
                    tracing::info!(
                        "Selling {sell_per_grid} {} per crossed grid",
                        config.strategy_token.name_to_string()
                    );

                    // Compute max fillable grid lines based on current UTxO balance
                    let max_fillable = strategy_amt / sell_per_grid;
                    if max_fillable == 0 {
                        continue;
                    }
                    tracing::info!("Number of fillable grids: {max_fillable}");

                    // Reduce crossed prices to only the prices that can be filled
                    let grids_to_fill = crossed_prices.len().min(max_fillable as usize);
                    let prices_to_fill = &crossed_prices[..grids_to_fill];

                    tracing::info!(
                        "{grids_to_fill} grids will be filled at the following prices: {:?}",
                        prices_to_fill
                    );

                    // Calculate buy and sell amounts
                    let sell_amt = sell_per_grid * grids_to_fill as u64;
                    let buy_amt: u64 = prices_to_fill
                        .iter()
                        .map(|price| sell_per_grid as f64 * price)
                        .sum::<f64>()
                        .floor() as u64;

                    tracing::info!(
                        "Selling {sell_amt} {} for {buy_amt} {}",
                        config.strategy_token.name_to_string(),
                        config.base_token.name_to_string()
                    );
                    trigger_sell_strategy(config, validity_range, s, sell_amt, buy_amt)?;

                    // Update offset
                    grid_state.line_offset += grids_to_fill as i64;
                    kv::set(&key, &grid_state)?;
                } else {
                    // Compute `base_token` to sell per grid line
                    let sell_per_grid = grid_state.initial_base_amount / config.levels_per_side;
                    if sell_per_grid == 0 {
                        continue;
                    }
                    tracing::info!(
                        "Selling {sell_per_grid} {} per crossed grid",
                        config.base_token.name_to_string()
                    );

                    // Compute max fillable grid lines based on current UTxO balance
                    let max_fillable = base_amt / sell_per_grid;
                    if max_fillable == 0 {
                        continue;
                    }
                    tracing::info!("Number of fillable grids: {max_fillable}");

                    // Reduce crossed prices to only the prices that can be filled
                    let grids_to_fill = crossed_prices.len().min(max_fillable as usize);
                    let prices_to_fill = &crossed_prices[..grids_to_fill];

                    tracing::info!(
                        "{grids_to_fill} grids will be filled at the following prices: {:?}",
                        prices_to_fill
                    );

                    // Calculate buy and sell amounts
                    let sell_amt = sell_per_grid * grids_to_fill as u64;
                    let buy_amt: u64 = prices_to_fill
                        .iter()
                        .map(|price| sell_per_grid as f64 / price)
                        .sum::<f64>()
                        .floor() as u64;
                    tracing::info!(
                        "Selling {sell_amt} {} for {buy_amt} {}",
                        config.base_token.name_to_string(),
                        config.strategy_token.name_to_string(),
                    );

                    trigger_buy_strategy(config, validity_range, s, sell_amt, buy_amt)?;

                    // Update offset
                    grid_state.line_offset -= grids_to_fill as i64;
                    kv::set(&key, &grid_state)?
                }
            }
        }
    }

    Ok(Ack)
}

/// Buy Strategy: Swap `base_token` for `strategy_token` when grid line crossed going up
fn trigger_buy_strategy(
    config: &Config<StrategyConfig>,
    validity_range: Interval,
    strategy: &ManagedStrategy,
    sell_amt: u64,
    buy_amt: u64,
) -> WorkerResult<Ack> {
    let swap = Order::Swap {
        offer: (
            config.base_token.policy_id.clone(),
            config.base_token.asset_name.clone(),
            sell_amt,
        ),
        min_received: (
            config.strategy_token.policy_id.clone(),
            config.strategy_token.asset_name.clone(),
            buy_amt,
        ),
    };

    sundae_strategies::submit_execution(&config.network, &strategy.output, validity_range, swap)?;
    info!("sell base asset order submitted");
    Ok(Ack)
}

/// Sell Strategy: Swap `strategy_token` for `base_token` when grid line crossed going down
fn trigger_sell_strategy(
    config: &Config<StrategyConfig>,
    validity_range: Interval,
    strategy: &ManagedStrategy,
    sell_amt: u64,
    buy_amt: u64,
) -> WorkerResult<Ack> {
    let swap = Order::Swap {
        offer: (
            config.strategy_token.policy_id.clone(),
            config.strategy_token.asset_name.clone(),
            sell_amt,
        ),
        min_received: (
            config.base_token.policy_id.clone(),
            config.base_token.asset_name.clone(),
            buy_amt,
        ),
    };

    sundae_strategies::submit_execution(&config.network, &strategy.output, validity_range, swap)?;
    info!("sell base asset order submitted");
    Ok(Ack)
}

#[balius_sdk::main]
fn main() -> Worker {
    balius_sdk::logging::init();

    Strategy::<StrategyConfig>::new()
        .on_new_pool_state(on_new_pool_state)
        .worker()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_expected_grid_prices() {
        let center = 1.0;
        let spacing = 0.05;
        let levels = 3;

        let grid = compute_grid_prices(center, spacing, levels);

        let step = 1.0 + spacing;

        let expected = [
            1.0 / step.powi(3),
            1.0 / step.powi(2),
            1.0 / step.powi(1),
            1.0 * step.powi(1),
            1.0 * step.powi(2),
            1.0 * step.powi(3),
        ];

        assert_eq!(grid.len(), expected.len());

        for (actual, expected) in grid.iter().zip(expected.iter()) {
            assert!(
                (actual - expected).abs() < 1e-10,
                "expected {}, got {}",
                expected,
                actual
            );
        }
    }

    #[test]
    fn detects_upward_crossing() {
        let center = 1.0;
        let spacing = 0.05;
        let levels = 3;

        let grid = compute_grid_prices(center, spacing, levels);

        let previous_offset = 0;

        let new_price = 1.12;

        let (new_offset, crossed) = compute_crossed_prices(&grid, previous_offset, new_price);

        let step = 1.0 + spacing;

        let expected_crossed = [center * step.powi(1), center * step.powi(2)];

        assert_eq!(crossed.len(), expected_crossed.len());

        for (actual, expected) in crossed.iter().zip(expected_crossed.iter()) {
            assert!((actual - expected).abs() < 1e-10);
        }

        assert_eq!(new_offset, previous_offset + 2);
    }

    #[test]
    fn detects_downward_crossing() {
        let center = 1.0;
        let spacing = 0.05;
        let levels = 3;

        let grid = compute_grid_prices(center, spacing, levels);

        let previous_offset = 0;

        let new_price = 0.89;

        let (new_offset, crossed) = compute_crossed_prices(&grid, previous_offset, new_price);

        let step = 1.0 + spacing;

        let expected = [center / step.powi(1), center / step.powi(2)];

        assert_eq!(crossed.len(), expected.len());

        for (actual, expected) in crossed.iter().zip(expected.iter()) {
            assert!((actual - expected).abs() < 1e-10);
        }

        assert_eq!(new_offset, -2);
    }

    #[test]
    fn no_grid_crossed_when_price_moves_within_band() {
        let grid = compute_grid_prices(1.0, 0.05, 3);

        let previous_offset = 0;

        let new_price = 1.04;

        let (new_offset, crossed) = compute_crossed_prices(&grid, previous_offset, new_price);

        assert!(crossed.is_empty());
        assert_eq!(new_offset, previous_offset);
    }

    #[test]
    fn crossing_center_does_not_trigger_fill() {
        let grid = compute_grid_prices(1.0, 0.05, 3);

        let previous_offset = 0;

        let new_price = 0.999;

        let (new_offset, crossed) = compute_crossed_prices(&grid, previous_offset, new_price);

        assert!(crossed.is_empty());
        assert_eq!(new_offset, 0);
    }

    #[test]
    fn continues_from_existing_offset() {
        let grid = compute_grid_prices(1.0, 0.05, 3);

        let previous_offset = 1;

        let new_price = 1.16;

        let (new_offset, crossed) = compute_crossed_prices(&grid, previous_offset, new_price);

        assert_eq!(crossed.len(), 2);
        assert_eq!(new_offset, 3);
    }

    #[test]
    fn price_exactly_on_grid_line_does_not_fill() {
        let center = 1.0;
        let spacing = 0.05;

        let grid = compute_grid_prices(center, spacing, 3);

        let previous_offset = 0;

        let new_price = grid[3];

        let (new_offset, crossed) = compute_crossed_prices(&grid, previous_offset, new_price);

        assert!(crossed.is_empty());
        assert_eq!(new_offset, 0);
    }
}
