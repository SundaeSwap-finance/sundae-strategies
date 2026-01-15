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
    types::{Interval, Order, StrategyAuthorization, asset_amount},
};
use tracing::info;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GridState {
    center_price: f64,
    line_offset: i64,
    init_strat: u64,
    init_base: u64,
}

impl GridState {
    fn new(strategy: &ManagedStrategy, pool_price: f64, config: &StrategyConfig) -> Self {
        Self {
            center_price: pool_price,
            line_offset: 0,
            init_strat: asset_amount(&strategy.utxo, &config.strategy_token),
            init_base: asset_amount(&strategy.utxo, &config.base_token),
        }
    }
}

fn grid_state_key(auth: &StrategyAuthorization) -> String {
    format!("grid_state:{auth}")
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

fn compute_crossed_prices(
    grid_prices: &[f64],
    previous_offset: i64,
    price: f64,
) -> (i64, Vec<f64>) {
    let new_offset = grid_prices.iter().take_while(|p| **p < price).count() as i64;

    let prices = if new_offset > previous_offset {
        grid_prices[previous_offset as usize..new_offset as usize].to_vec()
    } else if new_offset < previous_offset {
        grid_prices[new_offset as usize..previous_offset as usize]
            .iter()
            .rev()
            .copied()
            .collect()
    } else {
        Vec::new()
    };

    (new_offset, prices)
}

fn on_new_pool_state(
    config: &Config<StrategyConfig>,
    pool_state: &PoolState,
    strategies: &Vec<ManagedStrategy>,
) -> WorkerResult<Ack> {
    let pool_price = pool_state.pool_datum.raw_price(&pool_state.utxo);

    for s in strategies {
        // Filter for active strategies
        if pool_state.is_correct_pool(&s.order, &config.strategy_token, &config.base_token) {
            let auth = match &s.order.details {
                Order::Strategy { auth } => auth,
                Order::Swap {
                    offer: _,
                    min_received: _,
                } => continue,
            };

            // Get current UTxO balance for `strategy_token` and `base_token`
            let strategy_amt = asset_amount(&s.utxo, &config.strategy_token);
            let base_amt = asset_amount(&s.utxo, &config.base_token);
            if strategy_amt == 0 && base_amt == 0 {
                continue;
            }

            // Get center price and current line offset
            let key = grid_state_key(auth);
            let mut grid_state = kv::get::<GridState>(&key)?
                .unwrap_or_else(|| GridState::new(s, pool_price, config));

            // Compute grid lines
            let grid_prices = compute_grid_prices(
                grid_state.center_price,
                config.spacing_percent,
                config.levels_per_side,
            );

            // Check which grid lines (if any) were crossed
            let (new_offset, crossed_prices) =
                compute_crossed_prices(&grid_prices, grid_state.line_offset, pool_price);

            // Execute buy or sell depending on direction of the new offset
            if !crossed_prices.is_empty() {
                let validity_range = pool_state.get_validity_range(&config.network, 20);
                if new_offset > grid_state.line_offset {
                    // Compute `strategy_token` to sell per grid line
                    let sell_per_grid = grid_state.init_strat / config.levels_per_side;
                    if sell_per_grid == 0 {
                        continue;
                    }

                    // Compute max fillable grid lines based on current UTxO balance
                    let max_fillable = strategy_amt / sell_per_grid;
                    if max_fillable == 0 {
                        continue;
                    }

                    // Reduce crossed prices to only the prices that can be filled
                    let grids_to_fill = crossed_prices.len().min(max_fillable as usize);
                    let prices_to_fill = &crossed_prices[..grids_to_fill];

                    // Calculate buy and sell amounts
                    let sell_amt = sell_per_grid * grids_to_fill as u64;
                    let buy_amt: u64 = prices_to_fill
                        .iter()
                        .map(|price| sell_per_grid as f64 * price)
                        .sum::<f64>()
                        .floor() as u64;

                    trigger_sell_strategy(config, validity_range, s, sell_amt, buy_amt)?;

                    // Update offset
                    grid_state.line_offset += grids_to_fill as i64;
                    kv::set(&key, &grid_state)?;
                } else {
                    // Compute `base_token` to sell per grid line
                    let sell_per_grid = grid_state.init_base / config.levels_per_side;
                    if sell_per_grid == 0 {
                        continue;
                    }

                    // Compute max fillable grid lines based on current UTxO balance
                    let max_fillable = base_amt / sell_per_grid;
                    if max_fillable == 0 {
                        continue;
                    }

                    // Reduce crossed prices to only the prices that can be filled
                    let grids_to_fill = crossed_prices.len().min(max_fillable as usize);
                    let prices_to_fill = &crossed_prices[..grids_to_fill];

                    // Calculate buy and sell amounts
                    let sell_amt = sell_per_grid * grids_to_fill as u64;
                    let buy_amt: u64 = prices_to_fill
                        .iter()
                        .map(|price| sell_per_grid as f64 / price)
                        .sum::<f64>()
                        .floor() as u64;

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
