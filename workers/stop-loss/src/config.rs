use serde::Deserialize;
use sundae_strategies::{Network, types::AssetId};
use tracing::info;

#[derive(Deserialize)]
pub struct StopLossConfig {
    pub network: Network,
    // Tokens must be in alphanumeric order with token_a < token_b when sorted
    pub token_a: AssetId,
    pub token_a_decimals: u8,
    pub token_b: AssetId,
    pub token_b_decimals: u8,
    pub sell_token: AssetId,
    pub execution_price: f64,
}

impl StopLossConfig {
    pub fn trade_direction(&self) -> (Vec<u8>, Vec<u8>, f64) {
        if self.sell_token == self.token_a {
            (
                self.token_b.policy_id.clone(),
                self.token_b.asset_name.clone(),
                1.0 / self.execution_price,
            )
        } else {
            (
                self.token_a.policy_id.clone(),
                self.token_a.asset_name.clone(),
                self.execution_price,
            )
        }
    }

    pub fn log_submission(&self, give_amount: u64, receive_amount: u64) {
        info!(
            "executing limit order to sell {give_amount} {} for {receive_amount} {}",
            self.sell_token.name_to_string(),
            if self.token_a == self.sell_token {
                self.token_b.name_to_string()
            } else {
                self.token_a.name_to_string()
            }
        );
    }
}
