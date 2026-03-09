pub mod keys;
pub mod kv;
pub mod types;

use balius_sdk::{
    _internal::Handler,
    Ack, Config, Error, Tx, Utxo, UtxoMatcher, Worker, WorkerResult,
    http::{HttpRequest, HttpResponse},
    wit,
};
use serde::{Deserialize, Serialize};
use tracing::{info, trace};
use url::Url;
use utxorpc_spec::utxorpc::v1alpha::cardano::TxOutput;

use crate::{
    keys::get_signer_key,
    types::{
        AssetId, Interval, Order, OrderDatum, OutputReference, PoolDatum, SignedStrategyExecution,
        StrategyAuthorization, StrategyExecution, SubmitSSE, TransactionId, serialize,
    },
};

/// Which network is this strategy running against?
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Preview,
    Mainnet,
}

impl Network {
    /// Get the UNIX time associated with a slot
    pub fn to_unix_time(&self, slot: u64) -> u64 {
        let offset = match self {
            Self::Preview => 1666656000,
            Self::Mainnet => 1591566291,
        };
        (slot + offset) * 1000
    }

    pub(crate) fn relay_url(&self) -> Url {
        let url = match self {
            Self::Preview => "http://sse-relay.preview.sundae.fi/publish",
            Self::Mainnet => "http://sse-relay.sundae.fi/publish",
        };
        Url::parse(url).unwrap()
    }
}

/// Information about a strategy order getting managed by this library.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ManagedStrategy {
    /// The slot in which we first saw the order.
    pub slot: u64,
    /// A reference to the UTXO which is holding the order.
    pub output: OutputReference,
    /// Contents of the UTXO which is holding the order.
    pub utxo: TxOutput,
    /// The parsed order.
    pub order: OrderDatum,
}

impl ManagedStrategy {
    pub fn submit_execution(
        &self,
        network: &Network,
        validity_range: Interval,
        details: Order,
    ) -> Result<HttpResponse, balius_sdk::Error> {
        submit_execution(network, &self.output, validity_range, details)
    }
}

/// Information about a Sundae pool
#[derive(Debug, Clone)]
pub struct PoolState {
    /// The slot in which we first saw the pool.
    pub slot: u64,
    /// A reference to the UTXO which is holding the pool.
    pub output: OutputReference,
    /// Contents of the UTXO which is holding the pool.
    pub utxo: TxOutput,
    /// The parsed pool datum.
    pub pool_datum: PoolDatum,
}

impl PoolState {
    /// Returns true if the pool is relevant to the provided order
    pub fn is_correct_pool(
        &self,
        order: &OrderDatum,
        token_a: &AssetId,
        token_b: &AssetId,
    ) -> bool {
        if let Some(specific_pool) = &order.pool_ident {
            self.pool_datum.identifier == *specific_pool
        } else {
            let (asset_a, asset_b) = &self.pool_datum.assets;
            (token_a == asset_a && token_b == asset_b) || (token_a == asset_b && token_b == asset_a)
        }
    }

    pub fn price(&self, token_a_decimals: u8, token_b_decimals: u8) -> f64 {
        let raw = self.pool_datum.raw_price(&self.utxo);
        raw * 10f64.powi(token_a_decimals as i32 - token_b_decimals as i32)
    }

    // Returns the validity range in milliseconds relative to the current slot
    pub fn get_validity_range(&self, network: &Network, seconds: u64) -> Interval {
        let now_ms = network.to_unix_time(self.slot);
        let delta_ms = seconds.saturating_mul(1000);

        let start = now_ms.saturating_sub(delta_ms);
        let end = now_ms.saturating_add(delta_ms);
        Interval::inclusive_range(start, end)
    }
}

pub type NewStrategyCallback<T> = fn(&Config<T>, &ManagedStrategy) -> WorkerResult<Ack>;
struct NewStrategyHandler<T>(Option<NewStrategyCallback<T>>);
impl<T> Clone for NewStrategyHandler<T> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

pub type NewPoolStateCallback<T> =
    fn(&Config<T>, &PoolState, &Vec<ManagedStrategy>) -> WorkerResult<Ack>;
struct NewPoolStateHandler<T>(Option<NewPoolStateCallback<T>>);
impl<T> Clone for NewPoolStateHandler<T> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

pub type EachTxCallback<T> = fn(&Config<T>, &Tx, &Vec<ManagedStrategy>) -> WorkerResult<Ack>;
struct EachTxHandler<T>(Option<EachTxCallback<T>>);
impl<T> Clone for EachTxHandler<T> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

/// The entry point to a Sundae strategy worker. Use this to register handlers for interesting events.
pub struct Strategy<T> {
    new_strategy_callback: NewStrategyHandler<T>,
    new_pool_state_callback: NewPoolStateHandler<T>,
    each_tx_callback: EachTxHandler<T>,
}
impl<T> Clone for Strategy<T> {
    fn clone(&self) -> Self {
        Self {
            each_tx_callback: self.each_tx_callback.clone(),
            new_pool_state_callback: self.new_pool_state_callback.clone(),
            new_strategy_callback: self.new_strategy_callback.clone(),
        }
    }
}

impl<T: Send + Sync + 'static> Strategy<T>
where
    Config<T>: TryFrom<Vec<u8>, Error = balius_sdk::Error>,
{
    pub fn new() -> Self {
        Strategy {
            new_strategy_callback: NewStrategyHandler(None),
            new_pool_state_callback: NewPoolStateHandler(None),
            each_tx_callback: EachTxHandler(None),
        }
    }

    /// Register a callback to run when a new strategy is seen.
    pub fn on_new_strategy(mut self, f: NewStrategyCallback<T>) -> Self {
        self.new_strategy_callback = NewStrategyHandler(Some(f));
        self
    }
    /// Register a callback to run when a Sundae pool has been updated.
    pub fn on_new_pool_state(mut self, f: NewPoolStateCallback<T>) -> Self {
        self.new_pool_state_callback = NewPoolStateHandler(Some(f));
        self
    }
    /// Register a callback to run for every transaction.
    pub fn on_each_tx(mut self, f: EachTxCallback<T>) -> Self {
        self.each_tx_callback = EachTxHandler(Some(f));
        self
    }

    /// Finish building this strategy handler and construct a Balius worker.
    pub fn worker(self) -> Worker {
        self.worker_with(|w| w)
    }

    /// Finish building this strategy handler with additional customization.
    ///
    /// The provided function receives the partially-built Worker and can add
    /// custom request handlers before the Worker is finalized.
    ///
    /// # Example
    /// ```ignore
    /// Strategy::<MyConfig>::new()
    ///     .on_new_pool_state(handler)
    ///     .worker_with(|w| w.with_request_handler("my-handler", MyHandler))
    /// ```
    pub fn worker_with<F>(self, customize: F) -> Worker
    where
        F: FnOnce(Worker) -> Worker,
    {
        let worker = Worker::new()
            .with_request_handler("get-signer-key", self.clone())
            .with_utxo_handler(UtxoMatcher::all(), self.clone())
            .with_tx_handler(UtxoMatcher::all(), self.clone())
            .with_signer(STRATEGY_KEY, "ed25519");
        customize(worker)
    }
}

impl<T: Send + Sync + 'static> Default for Strategy<T>
where
    Config<T>: TryFrom<Vec<u8>, Error = balius_sdk::Error>,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + Sync + 'static> Handler for Strategy<T>
where
    Config<T>: TryFrom<Vec<u8>, Error = balius_sdk::Error>,
{
    fn handle(
        &self,
        config: wit::Config,
        event: wit::Event,
    ) -> Result<wit::Response, wit::HandleError> {
        let config: Config<T> = config.try_into()?;

        let result = if let Ok(tx) = event.clone().try_into() {
            self.handle_tx(config, tx)
        } else if let Ok(utxo) = event.clone().try_into() {
            self.handle_utxo(config, utxo)
        } else if let Ok(params) = event.clone().try_into() {
            let result = get_signer_key(config, params)?;
            return Ok(result.try_into()?);
        } else {
            Ok(Ack)
        };

        Ok(result?.try_into()?)
    }
}

impl<T: Send + Sync + 'static> Strategy<T> {
    fn handle_strategy_order(&self, config: &Config<T>, utxo: &Utxo<()>) -> WorkerResult<Ack> {
        // Check if it's a sundae v3 order datum?
        let Some(datum) = utxo
            .utxo
            .datum
            .clone()
            .and_then(|d| types::try_parse::<OrderDatum>(&d.original_cbor))
        else {
            return Ok(Ack);
        };

        trace!(
            slot = utxo.block_slot,
            tx_ref = format!("{}#{}", hex::encode(&utxo.tx_hash), utxo.index),
            "transaction output is a sundae v3 order",
        );

        let Some(key) = balius_sdk::get_public_keys().remove(STRATEGY_KEY) else {
            return Err(Error::Internal("key not found".into()));
        };

        // Check if it's *our* order
        use StrategyAuthorization::Signature;
        match &datum.details {
            Order::Strategy {
                auth: Signature { signer },
            } => {
                if signer == &key {
                    info!(
                        "transaction output is a strategy order owned by us ({})",
                        hex::encode(signer)
                    );
                } else {
                    info!(
                        "transaction output is a strategy order not owned by ({}), not us ({})",
                        hex::encode(signer),
                        hex::encode(&key)
                    );
                    return Ok(Ack);
                }
            }
            _ => return Ok(Ack),
        }

        info!(
            slot = utxo.block_slot,
            tx_ref = format!("{}#{}", hex::encode(&utxo.tx_hash), utxo.index),
            "owned strategy order observed",
        );

        // Save this order in our key-value store
        let seen = ManagedStrategy {
            slot: utxo.block_slot,
            output: OutputReference {
                transaction_id: TransactionId(utxo.tx_hash.clone()),
                output_index: utxo.index,
            },
            utxo: utxo.utxo.clone(),
            order: datum,
        };

        // This is an order "under our custody", so we hold onto it
        let mut all_seen: Vec<ManagedStrategy> = kv::get(KV_MANAGED_ORDERS)?.unwrap_or_default();

        all_seen.push(seen.clone());
        kv::set(KV_MANAGED_ORDERS, &all_seen)?;

        info!("now tracking {} orders", all_seen.len());

        if let NewStrategyHandler(Some(callback)) = self.new_strategy_callback {
            callback(config, &seen)
        } else {
            Ok(Ack)
        }
    }
    fn handle_pool_state(&self, config: &Config<T>, utxo: &Utxo<()>) -> WorkerResult<Ack> {
        // Check if it's a sundae v3 order datum?
        let Some(datum) = utxo
            .utxo
            .datum
            .clone()
            .and_then(|d| types::try_parse::<PoolDatum>(&d.original_cbor))
        else {
            return Ok(Ack);
        };

        trace!(
            slot = utxo.block_slot,
            tx_ref = format!("{}#{}", hex::encode(&utxo.tx_hash), utxo.index),
            "transaction output is a new sundae v3 pool state",
        );

        let pool_state = PoolState {
            slot: utxo.block_slot,
            output: OutputReference {
                transaction_id: TransactionId(utxo.tx_hash.clone()),
                output_index: utxo.index,
            },
            utxo: utxo.utxo.clone(),
            pool_datum: datum,
        };

        let all_seen: Vec<ManagedStrategy> = kv::get(KV_MANAGED_ORDERS)?.unwrap_or_default();

        if let NewPoolStateHandler(Some(callback)) = self.new_pool_state_callback {
            callback(config, &pool_state, &all_seen)
        } else {
            Ok(Ack)
        }
    }

    fn handle_utxo(&self, config: Config<T>, utxo: Utxo<()>) -> WorkerResult<Ack> {
        trace!(
            slot = utxo.block_slot,
            tx_ref = format!("{}#{}", hex::encode(&utxo.tx_hash), utxo.index),
            "transaction output observed",
        );

        self.handle_pool_state(&config, &utxo)?;
        self.handle_strategy_order(&config, &utxo)?;
        Ok(Ack)
    }

    fn handle_tx(&self, config: Config<T>, tx: Tx) -> WorkerResult<Ack> {
        trace!(
            slot = tx.block_slot,
            tx_hash = hex::encode(&tx.hash),
            "transaction observed",
        );
        let spent_inputs = tx
            .tx
            .inputs
            .iter()
            .map(|input| (input.tx_hash.to_vec(), input.output_index as u64))
            .collect::<Vec<_>>();

        trace!("Marking orders as spent, if any...");
        let mut seen_orders: Vec<ManagedStrategy> = kv::get(KV_MANAGED_ORDERS)?.unwrap_or_default();

        seen_orders.retain(|spent| {
            let (spent_hash, spent_index) =
                (&spent.output.transaction_id.0, &spent.output.output_index);
            !spent_inputs
                .iter()
                .any(|(hash, index)| spent_hash == hash && spent_index == index)
        });

        kv::set(KV_MANAGED_ORDERS, &seen_orders)?;

        trace!("remaining orders: {:?}", seen_orders);

        if let EachTxHandler(Some(callback)) = self.each_tx_callback {
            callback(&config, &tx, &seen_orders)
        } else {
            Ok(Ack)
        }
    }
}

pub(crate) const KV_MANAGED_ORDERS: &str = "managed_orders";
pub(crate) const STRATEGY_KEY: &str = "default";

/// Submit a strategy execution.
///
/// # Examples
/// ```
/// # use std::time::Duration;
/// # use sundae_strategies::{*, types::*};
/// # use balius_sdk::*;
/// #
/// # struct MyConfig {
/// #     network: Network,
/// #     offer_token: AssetId,
/// #     offer_amount: u64,
/// #     receive_token: AssetId,
/// #     receive_amount_min: u64,
/// # }
/// #
/// fn trigger_buy(config: &MyConfig, now: u64, order: &ManagedStrategy) -> WorkerResult<()> {
///     let valid_for = Duration::from_secs_f64(20. * 60.);
///     let validity_range = Interval::inclusive_range(
///         now - valid_for.as_millis() as u64,
///         now + valid_for.as_millis() as u64,
///     );
///
///     let swap = Order::Swap {
///         offer: (
///             config.offer_token.policy_id.clone(),
///             config.offer_token.asset_name.clone(),
///             config.offer_amount,
///         ),
///         min_received: (
///             config.receive_token.policy_id.clone(),
///             config.receive_token.asset_name.clone(),
///             config.receive_amount_min,
///         ),
///     };
///
///     sundae_strategies::submit_execution(&config.network, &order.output, validity_range, swap)?;
///
///     Ok(())
/// }
/// ```
pub fn submit_execution(
    network: &Network,
    utxo: &OutputReference,
    validity_range: Interval,
    details: Order,
) -> Result<HttpResponse, Error> {
    let execution = StrategyExecution {
        tx_ref: utxo.clone(),
        validity_range,
        details,
        extensions: vec![],
    };

    let bytes = serialize(execution.clone());

    let signature = balius_sdk::wit::balius::app::sign::sign_payload("default", &bytes)?;

    let sse = SignedStrategyExecution {
        execution,
        signature: Some(signature),
    };
    let sse_bytes = serialize(sse);

    let submit_sse = SubmitSSE {
        tx_hash: hex::encode(&utxo.transaction_id.0),
        tx_index: utxo.output_index,
        data: hex::encode(&sse_bytes),
    };

    info!(
        "posting to {}: {} / {} / {}",
        network.relay_url(),
        hex::encode(&utxo.transaction_id.0),
        utxo.output_index,
        hex::encode(&sse_bytes)
    );

    Ok(HttpRequest::post(network.relay_url())
        .json(&submit_sse)?
        .send()?)
}
