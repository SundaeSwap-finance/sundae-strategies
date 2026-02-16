# Getting Started

Learn how to set up your development environment and create your first Sundae Strategy.

## Prerequisites

Before you begin, make sure you have the following tools installed:

- **Rust** (latest stable version)
- **Cargo** (comes with Rust)
- **Bun** (JavaScript runtime and package manager)


## Required Tools

Install the additional tools needed for Sundae Strategies development:

```bash
# Install cargo-generate for creating new projects
cargo install cargo-generate

# Install just (task runner)
cargo install just

# Install the Balius runtime
cargo install baliusd

# Install the Balius worker builder
cargo install --git https://github.com/SundaeSwap-finance/sundae-strategies balius-worker-builder
```

## Creating Your First Strategy

Use the official template to create a new strategy project:

```bash
cargo generate SundaeSwap-finance/sundae-strategy-template
```

Enter a project name when prompted. This will generate a new project from our strategy template.

The new project includes a pre-implemented [trailing stop-loss](https://therobusttrader.com/trailing-stop-loss/) strategy. You'll want to replace that with your own logic, but it serves as a good example for how to structure a strategy. Let's examine the key parts:

```rust
// src/config.rs
use serde::Deserialize;
use sundae_strategies::{Network, types::AssetId};

#[derive(Deserialize)]
pub struct Config {
    pub network: Network,
    pub give_token: AssetId,
    pub receive_token: AssetId,
    pub trail_percent: f64,
}
```

The config file includes... configuration! This is where you add any settings your particular strategy needs to run. In this example,
 - `network` is the Cardano network this strategy is running against. Every strategy needs this.
 - `give_token` and `receive_token` are the assets being swapped (this particular strategy swaps `give_token` for `receive_token`).
 - `trail_percent` controls how much risk this particular strategy will take.

Next, look at the strategy itself. At the bottom of `src/lib.rs`, we can see:
```rust
#[balius_sdk::main]
// The config we defined earlier
use config::Config as StrategyConfig;

// ...

#[balius_sdk::main]
fn main() -> Worker {
    balius_sdk::logging::init();

    Strategy::<StrategyConfig>::new()
        .on_new_pool_state(on_new_pool_state)
        .worker()
}

```

This is the entry point to the strategy. We've set up a basic Balius worker, passing it an `on_new_pool_state` callback. That callback gets run whenever a pool is updated, which lets the strategy react to price changes.

Let's look at that `on_new_pool_state` callback itself to see the strategy logic itself. The strategy logic is a bit involved, so we'll break it down.

```rust
pub const BASE_PRICE_PREFIX: &str = "base_price:";
fn base_price_key(pool_ident: &String) -> String {
    format!("{BASE_PRICE_PREFIX}{pool_ident}")
}

fn on_new_pool_state(
    config: &Config<StrategyConfig>,
    pool_state: &PoolState,
    strategies: &Vec<ManagedStrategy>,
) -> WorkerResult<Ack> {
    let pool_price = pool_state.pool_datum.raw_price(&pool_state.utxo);
    let pool_ident = hex::encode(&pool_state.pool_datum.identifier);

    let base_price = kv::get::<f64>(base_price_key(&pool_ident).as_str())?.unwrap_or(0.0);

    info!(
        "pool update found, with price {} against base price {}",
        pool_price, base_price
    );
    // ...
```

The above code reads `pool_price` (the current price for that pool) from the pool state. It also reads `base_price` (the price this strategy will sell below) from the key-value store. 

```rust
    // ...

    if pool_price < base_price {
        info!(
            "price has fallen to {}, below the base price of {}. Triggering a sell order...",
            pool_price, base_price,
        );
        for strategy in strategies {
            trigger_sell(
                config,
                config.network.to_unix_time(pool_state.slot),
                strategy,
            )?;
        }
    }

    // ...
```

If the `pool_price` has dropped below the `base_price`, this logic says to sell. Note that we're triggering a sell across multiple strategies; more complex logic could read the `order` datum from each strategy to decide how to handle them.

```rust
    // ...

    let new_base_price: f64 = f64::max(base_price, pool_price * (1. - config.trail_percent));
    if new_base_price != base_price {
        info!("updating new base price to {}", new_base_price);
        kv::set(base_price_key(&pool_ident).as_str(), &new_base_price)?;
    }

    Ok(Ack)
}

```

Finally, we update our base price and put that in the key-value store.


## Testing your strategy

From your project directory, start a balius worker by running:

```bash
just start
```

Now the worker is running, and ready to react to onchain strategy orders.

To place a strategy order, you need the public key associated with this strategy. You can get it by running this command:

```bash
just get-key
```

Once you have that key, use the Sundae SDK CLI to place your first strategy order:

```bash
bunx @sundaeswap/cli
```

Follow the interactive prompts to create a strategy order, providing that public key when asked. Once the order is on-chain, your worker should see it and start running!

If you'd like, you can also run the worker in "debug" mode. This stores most state in memory, so that it gets cleared when the worker is stopped. That's useful when developing new strategies, because you can reuse the same strategy order.

```bash
just debug
```
