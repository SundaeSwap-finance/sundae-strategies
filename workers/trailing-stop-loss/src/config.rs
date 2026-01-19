use serde::Deserialize;
use sundae_strategies::{Network, types::AssetId};

/// Default slippage tolerance (3%)
/// This allows the exit order to fill even if price drops slightly between
/// trigger and execution, while still protecting against catastrophic fills.
const DEFAULT_SLIPPAGE_TOLERANCE: f64 = 0.03;

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
    /// Maximum acceptable slippage when executing the exit order (e.g., 0.03 = 3%)
    /// Must be in range (0.0, 1.0). Defaults to 3% if not specified.
    /// This determines the minimum amount of exit_token accepted in the swap.
    pub slippage_tolerance: f64,
    /// Optional initial peak price for the strategy.
    /// If provided, this value is used as the initial peak price instead of
    /// discovering it from the current pool price. This is useful when modifying
    /// an existing position (cancel + recreate) to preserve the previous peak.
    /// Not displayed on frontend - populated automatically during position modify.
    pub entry_price: Option<f64>,
}

/// Raw config for deserialization before validation
#[derive(Deserialize)]
struct ConfigRaw {
    network: Network,
    position_token: AssetId,
    exit_token: AssetId,
    trail_percent: f64,
    slippage_tolerance: Option<f64>,
    entry_price: Option<f64>,
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

        // Use default slippage tolerance if not provided
        let slippage_tolerance = raw.slippage_tolerance.unwrap_or(DEFAULT_SLIPPAGE_TOLERANCE);

        // Validate slippage_tolerance is in valid range
        if slippage_tolerance <= 0.0 {
            return Err(format!(
                "slippage_tolerance must be > 0.0, got {}",
                slippage_tolerance
            ));
        }
        if slippage_tolerance >= 1.0 {
            return Err(format!(
                "slippage_tolerance must be < 1.0, got {}",
                slippage_tolerance
            ));
        }

        // Validate entry_price if provided
        if let Some(price) = raw.entry_price
            && price <= 0.0
        {
            return Err(format!("entry_price must be > 0.0, got {}", price));
        }

        // Validate position_token and exit_token are different
        if raw.position_token == raw.exit_token {
            return Err(
                "position_token and exit_token must be different tokens".to_string()
            );
        }

        Ok(Config {
            network: raw.network,
            position_token: raw.position_token,
            exit_token: raw.exit_token,
            trail_percent: raw.trail_percent,
            slippage_tolerance,
            entry_price: raw.entry_price,
        })
    }
}
