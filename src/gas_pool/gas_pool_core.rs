// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::metrics::GasPoolCoreMetrics;
use crate::storage::Storage;
use crate::sui_client::SuiClient;
use crate::tx_signer::TxSigner;
use crate::types::{GasCoin, ReservationID};
use crate::{retry_forever, retry_with_max_attempts};
use anyhow::bail;
use std::sync::Arc;
use std::time::Duration;
use sui_json_rpc_types::{
    SuiTransactionBlockEffects, SuiTransactionBlockEffectsAPI, SuiTransactionBlockEvents,
};
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::gas_coin::MIST_PER_OCT;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::quorum_driver_types::ExecuteTransactionRequestType;
use sui_types::signature::GenericSignature;
use sui_types::transaction::{
    Argument, Command, Transaction, TransactionData, TransactionDataAPI, TransactionKind,
};
use tap::TapFallible;
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use super::gas_usage_cap::GasUsageCap;

const EXPIRATION_JOB_INTERVAL: Duration = Duration::from_secs(1);

pub struct GasPoolContainer {
    inner: Arc<GasPool>,
    _coin_unlocker_task: JoinHandle<()>,
    // This is always Some. It is None only after the drop method is called.
    cancel_sender: Option<tokio::sync::oneshot::Sender<()>>,
}

pub struct GasPool {
    signer: Arc<dyn TxSigner>,
    gas_pool_store: Arc<dyn Storage>,
    sui_client: SuiClient,
    metrics: Arc<GasPoolCoreMetrics>,
    gas_usage_cap: Arc<GasUsageCap>,
}

impl GasPool {
    pub async fn new(
        signer: Arc<dyn TxSigner>,
        gas_pool_store: Arc<dyn Storage>,
        sui_client: SuiClient,
        metrics: Arc<GasPoolCoreMetrics>,
        gas_usage_cap: Arc<GasUsageCap>,
    ) -> Arc<Self> {
        let pool = Self {
            signer,
            gas_pool_store,
            sui_client,
            metrics,
            gas_usage_cap,
        };
        Arc::new(pool)
    }

    pub async fn reserve_gas(
        &self,
        sponsor_address: Option<SuiAddress>,
        gas_budget: u64,
        duration: Duration,
    ) -> anyhow::Result<(SuiAddress, ReservationID, Vec<ObjectRef>)> {
        let sponsor_address = sponsor_address.unwrap_or(self.signer.get_addresses()[0]);
        let cur_time = std::time::Instant::now();
        self.gas_usage_cap.check_usage().await?;
        let (reservation_id, gas_coins) = self
            .gas_pool_store
            .reserve_gas_coins(sponsor_address, gas_budget, duration.as_millis() as u64)
            .await?;
        let elapsed = cur_time.elapsed().as_millis();
        self.metrics.reserve_gas_latency_ms.observe(elapsed as u64);
        self.metrics
            .reserved_gas_coin_count_per_request
            .observe(gas_coins.len() as u64);
        Ok((
            sponsor_address,
            reservation_id,
            gas_coins.into_iter().map(|c| c.object_ref).collect(),
        ))
    }

    pub async fn execute_transaction(
        &self,
        reservation_id: ReservationID,
        tx_data: TransactionData,
        request_type: Option<ExecuteTransactionRequestType>,
        user_sig: GenericSignature,
    ) -> anyhow::Result<(
        Option<u64>,
        SuiTransactionBlockEffects,
        Option<SuiTransactionBlockEvents>,
    )> {
        let sponsor = tx_data.gas_data().owner;
        if !self.signer.is_valid_address(&sponsor) {
            bail!("Sponsor {:?} is not registered", sponsor);
        };
        Self::check_transaction_validity(&tx_data)?;
        let payment: Vec<_> = tx_data
            .gas_data()
            .payment
            .iter()
            .map(|oref| oref.0)
            .collect();
        let payment_count = payment.len();
        debug!(
            ?reservation_id,
            "Payment coins in transaction: {:?}", payment
        );
        self.gas_pool_store
            .ready_for_execution(sponsor, reservation_id)
            .await?;
        debug!(?reservation_id, "Reservation is ready for execution");

        // To avoid read-after-write inconsistency, we apply a trick here to calculate the
        // new balance of the gas coin after the transaction.
        // We first query the total balance prior to transaction execution, then execute the
        // transaction, and finally derive the new gas coin balance using the gas usage from effects.
        let total_gas_coin_balance = self.get_total_gas_coin_balance(payment.clone()).await;
        debug!(
            ?reservation_id,
            "Total gas coin balance prior to execution: {}", total_gas_coin_balance,
        );
        let response = self
            .execute_transaction_impl(reservation_id, tx_data, request_type, user_sig)
            .await;
        let updated_coins = match &response {
            Ok((_, effects, _)) => {
                let new_gas_coin = effects.gas_object().reference.to_object_ref();
                let new_balance =
                    total_gas_coin_balance as i64 - effects.gas_cost_summary().net_gas_usage();
                debug!(
                    ?reservation_id,
                    "New gas coin balance after execution: {}", new_balance,
                );
                #[cfg(test)]
                {
                    self.sui_client.wait_for_object(new_gas_coin).await;
                    assert_eq!(
                        self.get_total_gas_coin_balance(payment).await,
                        new_balance as u64
                    );
                }
                vec![GasCoin {
                    owner: sponsor,
                    object_ref: new_gas_coin,
                    balance: new_balance as u64,
                }]
            }
            Err(_) => {
                debug!(
                    ?reservation_id,
                    "Querying latest gas state since transaction failed"
                );
                self.sui_client
                    .get_latest_gas_objects(payment)
                    .await
                    .into_values()
                    .flatten()
                    .collect()
            }
        };
        let smashed_coin_count = payment_count - updated_coins.len();
        // Regardless of whether the transaction succeeded, we need to release the coins.
        // Otherwise, we lose track of them. This is because `ready_for_execution` already takes
        // the coins out of the pool and will not be covered by the auto-release mechanism.
        self.release_gas_coins(updated_coins).await;
        if smashed_coin_count > 0 {
            info!(
                ?reservation_id,
                "Smashed {:?} coins after transaction execution", smashed_coin_count
            );
            self.metrics
                .num_smashed_gas_coins
                .with_label_values(&[&sponsor.to_string()])
                .inc_by(smashed_coin_count as u64);
        }
        info!(?reservation_id, "Transaction execution finished");

        response
    }

    async fn execute_transaction_impl(
        &self,
        reservation_id: ReservationID,
        tx_data: TransactionData,
        request_type: Option<ExecuteTransactionRequestType>,
        user_sig: GenericSignature,
    ) -> anyhow::Result<(
        Option<u64>,
        SuiTransactionBlockEffects,
        Option<SuiTransactionBlockEvents>,
    )> {
        let sponsor = tx_data.gas_data().owner;
        let cur_time = std::time::Instant::now();
        let sponsor_sig = retry_with_max_attempts!(
            async {
                self.signer
                    .sign_transaction(&tx_data)
                    .await
                    .tap_err(|err| error!("Failed to sign transaction: {:?}", err))
            },
            3
        )?;
        let elapsed = cur_time.elapsed().as_millis();
        self.metrics
            .transaction_signing_latency_ms
            .observe(elapsed as u64);
        debug!(?reservation_id, "Transaction signed by sponsor");

        let tx = Transaction::from_generic_sig_data(tx_data, vec![sponsor_sig, user_sig]);
        let cur_time = std::time::Instant::now();
        let response = self
            .sui_client
            .execute_transaction(tx, request_type, 3)
            .await?;
        debug!(?reservation_id, "Transaction executed");
        let elapsed = cur_time.elapsed().as_millis();
        self.metrics
            .transaction_execution_latency_ms
            .observe(elapsed as u64);
        let net_gas_usage = response.1.gas_cost_summary().net_gas_usage();
        let new_daily_usage = self.gas_usage_cap.update_usage(net_gas_usage).await;
        self.metrics
            .daily_gas_usage
            .with_label_values(&[&sponsor.to_string()])
            .set(new_daily_usage);
        Ok(response)
    }

    async fn get_total_gas_coin_balance(&self, gas_coins: Vec<ObjectID>) -> u64 {
        let latest = self.sui_client.get_latest_gas_objects(gas_coins).await;
        latest
            .into_values()
            .flatten()
            .map(|coin| coin.balance)
            .sum()
    }

    fn check_transaction_validity(tx_data: &TransactionData) -> anyhow::Result<()> {
        let mut all_args = vec![];
        for command in tx_data.kind().iter_commands() {
            match command {
                Command::MoveCall(call) => {
                    all_args.extend(call.arguments.iter());
                }
                Command::TransferObjects(args, _) => {
                    all_args.extend(args.iter());
                }
                Command::SplitCoins(arg, _) => {
                    all_args.push(arg);
                }
                Command::MergeCoins(arg, args) => {
                    all_args.push(arg);
                    all_args.extend(args.iter());
                }
                Command::Publish(_, _) => {}
                Command::MakeMoveVec(_, args) => {
                    all_args.extend(args.iter());
                }
                Command::Upgrade(_, _, _, _) => {}
            };
        }
        let uses_gas = all_args
            .into_iter()
            .any(|arg| matches!(*arg, Argument::GasCoin));
        if uses_gas {
            bail!("Gas coin can only be used to pay gas")
        };
        Ok(())
    }

    /// Release gas coins back to the gas pool, by adding them to the storage.
    async fn release_gas_coins(&self, gas_coins: Vec<GasCoin>) {
        debug!("Trying to release gas coins: {:?}", gas_coins);
        retry_forever!(async {
            self.gas_pool_store
                .add_new_coins(gas_coins.clone())
                .await
                .tap_err(|err| error!("Failed to call update_gas_coins on storage: {:?}", err))
        })
        .unwrap();
    }

    /// Performs an end-to-end flow of reserving gas, signing a transaction, and releasing the gas coins.
    pub async fn debug_check_health(&self) -> anyhow::Result<()> {
        let gas_budget = MIST_PER_OCT / 10;
        let sponsor = self.signer.get_addresses()[0];
        let (_address, _reservation_id, gas_coins) = self
            .reserve_gas(Some(sponsor), gas_budget, Duration::from_secs(3))
            .await?;
        let tx_kind = TransactionKind::ProgrammableTransaction(
            ProgrammableTransactionBuilder::new().finish(),
        );
        // Since we just want to check the health of the signer, we don't need to actually execute the transaction.
        let tx_data = TransactionData::new_with_gas_coins(
            tx_kind,
            SuiAddress::default(),
            gas_coins,
            gas_budget,
            0,
        );
        self.signer.sign_transaction(&tx_data).await?;
        Ok(())
    }

    async fn start_coin_unlock_task(
        self: Arc<Self>,
        mut cancel_receiver: tokio::sync::oneshot::Receiver<()>,
    ) -> JoinHandle<()> {
        tokio::task::spawn(async move {
            loop {
                let expire_results = self.gas_pool_store.expire_coins().await;
                let unlocked_coins = expire_results.unwrap_or_else(|err| {
                    error!("Failed to call expire_coins to the storage: {:?}", err);
                    vec![]
                });
                if !unlocked_coins.is_empty() {
                    debug!("Coins that are expired: {:?}", unlocked_coins);
                    let latest_coins: Vec<_> = self
                        .sui_client
                        .get_latest_gas_objects(unlocked_coins.clone())
                        .await
                        .into_values()
                        .flatten()
                        .collect();
                    let count = latest_coins.len();
                    self.release_gas_coins(latest_coins).await;
                    info!("Released {:?} coins after expiration", count);
                }
                tokio::select! {
                    _ = tokio::time::sleep(EXPIRATION_JOB_INTERVAL) => {}
                    _ = &mut cancel_receiver => {
                        info!("Coin unlocker task is cancelled");
                        break;
                    }
                }
            }
        })
    }

    pub async fn query_pool_available_coin_count(&self, sponsor: SuiAddress) -> usize {
        self.gas_pool_store
            .get_available_coin_count(sponsor)
            .await
            .unwrap()
    }

    pub fn support_address(&self) -> Vec<SuiAddress> {
        self.signer.get_addresses()
    }
}

impl GasPoolContainer {
    pub async fn new(
        signer: Arc<dyn TxSigner>,
        gas_pool_store: Arc<dyn Storage>,
        sui_client: SuiClient,
        gas_usage_daily_cap: u64,
        metrics: Arc<GasPoolCoreMetrics>,
    ) -> Self {
        let inner = GasPool::new(
            signer,
            gas_pool_store,
            sui_client,
            metrics,
            Arc::new(GasUsageCap::new(gas_usage_daily_cap)),
        )
        .await;
        let (cancel_sender, cancel_receiver) = tokio::sync::oneshot::channel();
        let _coin_unlocker_task = inner.clone().start_coin_unlock_task(cancel_receiver).await;

        Self {
            inner,
            _coin_unlocker_task,
            cancel_sender: Some(cancel_sender),
        }
    }

    pub fn get_gas_pool_arc(&self) -> Arc<GasPool> {
        self.inner.clone()
    }
}

impl Drop for GasPoolContainer {
    fn drop(&mut self) {
        self.cancel_sender.take().unwrap().send(()).unwrap();
    }
}
