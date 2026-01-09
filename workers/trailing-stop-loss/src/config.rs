use serde::Deserialize;
use sundae_strategies::{Network, types::AssetId};

#[derive(Deserialize)]
#[serde(try_from = "ConfigRaw")]
pub struct Config {
    pub network: Network,
    /// The token being protected by TSL (the position you're holding)
    pub position_token: AssetId,
    /// The token to swap into when TSL triggers (exit destination)
    pub exit_token: AssetId,
    /// How far below the peak price the stop triggers (e.g., 0.15 = 15%)
    /// Must be in range (0.0, 1.0) - e.g., 0.15 means trigger at 15% below peak
    pub trail_percent: f64,
}

/// Raw config for deserialization before validation
#[derive(Deserialize)]
struct ConfigRaw {
    network: Network,
    position_token: AssetId,
    exit_token: AssetId,
    trail_percent: f64,
}

impl TryFrom<ConfigRaw> for Config {
    type Error = String;

    fn try_from(raw: ConfigRaw) -> Result<Self, Self::Error> {
        // Validate trail_percent is in valid range
        if raw.trail_percent <= 0.0 {
            return Err(format!(
                "trail_percent must be > 0.0, got {}",
                raw.trail_percent
            ));
        }
        if raw.trail_percent >= 1.0 {
            return Err(format!(
                "trail_percent must be < 1.0, got {}",
                raw.trail_percent
            ));
        }

        Ok(Config {
            network: raw.network,
            position_token: raw.position_token,
            exit_token: raw.exit_token,
            trail_percent: raw.trail_percent,
        })
    }
}
