mod config;

use balius_sdk::{Ack, Config, WorkerResult};
use config::StopLossConfig as StrategyConfig;
use sundae_strategies::{
    ManagedStrategy, PoolState, Strategy,
    types::{Interval, Order, asset_amount},
};
use tracing::info;

fn on_new_pool_state(
    config: &Config<StrategyConfig>,
    pool_state: &PoolState,
    strategies: &Vec<ManagedStrategy>,
) -> WorkerResult<Ack> {
    for strategy in strategies {
        //  Skip processing for state changes of unrelated pools
        if !pool_state.is_correct_pool(&strategy.order, &config.token_a, &config.token_b) {
            continue;
        }

        // Get pool price and scale for decimals
        let pool_price = pool_state.price(config.token_a_decimals, config.token_b_decimals);
        info!("pool update found, with price {}", pool_price);

        // Execute if pool_price is below execution price
        if pool_price < config.execution_price {
            info!(
                "price has fallen to {}, below SL price of {}. Triggering a sell order...",
                pool_price, config.execution_price
            );
            let validity_range = pool_state.get_validity_range(&config.network, 20);
            trigger_sell(config, validity_range, strategy)?;
        }
    }
    Ok(Ack)
}

fn trigger_sell(
    config: &StrategyConfig,
    validity_range: Interval,
    order: &ManagedStrategy,
) -> WorkerResult<Ack> {
    let give_amount = asset_amount(&order.utxo, &config.sell_token);

    // Get the buy asset and the minimum number of buy tokens per sell token based on config
    let (buy_policy, buy_name, price_ratio) = config.trade_direction();
    let receive_amount = (give_amount as f64 * price_ratio) as u64;

    let swap = Order::Swap {
        offer: (
            config.sell_token.policy_id.clone(),
            config.sell_token.asset_name.clone(),
            give_amount,
        ),
        min_received: (buy_policy, buy_name, receive_amount),
    };

    // Submit to relay and log
    order.submit_execution(&config.network, validity_range, swap)?;
    config.log_submission(give_amount, receive_amount);
    Ok(Ack)
}

#[balius_sdk::main]
fn main() -> Worker {
    balius_sdk::logging::init();

    Strategy::<StrategyConfig>::new()
        .on_new_pool_state(on_new_pool_state)
        .worker()
}
