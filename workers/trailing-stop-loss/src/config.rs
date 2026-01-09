use serde::Deserialize;
use sundae_strategies::{Network, types::AssetId};

#[derive(Deserialize)]
pub struct Config {
    pub network: Network,
    /// The token being protected by TSL (the position you're holding)
    pub position_token: AssetId,
    /// The token to swap into when TSL triggers (exit destination)
    pub exit_token: AssetId,
    /// How far below the peak price the stop triggers (e.g., 0.15 = 15%)
    pub trail_percent: f64,
}
