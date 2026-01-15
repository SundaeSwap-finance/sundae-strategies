use serde::Deserialize;
use sundae_strategies::{Network, types::AssetId};

#[derive(Deserialize)]
#[serde(try_from = "ConfigRaw")]
pub struct Config {
    pub network: Network,
    /// The token to buy or sell when a grid line is crossed
    pub strategy_token: AssetId,
    /// The token to trade against the strategy token
    pub base_token: AssetId,
    /// The percentage between each grid line
    pub spacing_percent: f64,
    /// The number of grid lines per side of the grid
    pub levels_per_side: u64,
}

/// Raw config for deserialization before validation
#[derive(Deserialize)]
struct ConfigRaw {
    network: Network,
    strategy_token: AssetId,
    base_token: AssetId,
    spacing_percent: f64,
    levels_per_side: u64,
}

impl TryFrom<ConfigRaw> for Config {
    type Error = String;

    fn try_from(raw: ConfigRaw) -> Result<Self, Self::Error> {
        if raw.levels_per_side == 0 {
            return Err("levels_per_side must be >= 1".to_string());
        }

        if raw.spacing_percent <= 0.0 {
            return Err(format!(
                "spacing_percent must be > 0, got {}",
                raw.spacing_percent
            ));
        }

        if raw.spacing_percent * raw.levels_per_side as f64 >= 1.0 {
            return Err(format!(
                "spacing_percent * levels_per_side must be < 1.0 (got {} * {} = {})",
                raw.spacing_percent,
                raw.levels_per_side,
                raw.spacing_percent * raw.levels_per_side as f64
            ));
        }

        Ok(Config {
            network: raw.network,
            strategy_token: raw.strategy_token,
            base_token: raw.base_token,
            spacing_percent: raw.spacing_percent,
            levels_per_side: raw.levels_per_side,
        })
    }
}
