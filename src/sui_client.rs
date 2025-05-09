// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::types::GasCoin;
use crate::{retry_forever, retry_with_max_attempts};
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use itertools::Itertools;
use std::collections::HashMap;
use std::time::Duration;
use sui_json_rpc_types::{
    SuiData, SuiObjectDataOptions, SuiObjectResponse, SuiTransactionBlockEffects,
    SuiTransactionBlockResponseOptions,
};
use sui_json_rpc_types::{SuiTransactionBlockEffectsAPI, SuiTransactionBlockEvents};
use sui_sdk::SuiClientBuilder;
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::coin::{PAY_MODULE_NAME, PAY_SPLIT_N_FUNC_NAME};
use sui_types::gas_coin::GAS;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::quorum_driver_types::ExecuteTransactionRequestType;
use sui_types::transaction::{
    Argument, ObjectArg, ProgrammableTransaction, Transaction, TransactionKind,
};
use sui_types::SUI_FRAMEWORK_PACKAGE_ID;
use tap::TapFallible;
use tracing::{debug, info};

#[derive(Clone)]
pub struct SuiClient {
    sui_client: sui_sdk::SuiClient,
}

impl SuiClient {
    pub async fn new(fullnode_url: &str, basic_auth: Option<(String, String)>) -> Self {
        let mut sui_client_builder = SuiClientBuilder::default().max_concurrent_requests(100000);
        if let Some((username, password)) = basic_auth {
            sui_client_builder = sui_client_builder.basic_auth(username, password);
        }
        let sui_client = sui_client_builder.build(fullnode_url).await.unwrap();
        Self { sui_client }
    }

    pub async fn get_all_owned_sui_coins_above_balance_threshold(
        &self,
        address: SuiAddress,
        balance_threshold: u64,
    ) -> Vec<GasCoin> {
        info!(
            "Querying all gas coins owned by sponsor address {} that has at least {} balance",
            address, balance_threshold
        );
        let mut cursor = None;
        let mut coins = Vec::new();
        loop {
            let page = retry_forever!(async {
                self.sui_client
                    .coin_read_api()
                    .get_coins(address, None, cursor, None)
                    .await
                    .tap_err(|err| debug!("Failed to get owned gas coins: {:?}", err))
            })
            .unwrap();
            for coin in page.data {
                if coin.balance >= balance_threshold {
                    coins.push(GasCoin {
                        owner: address,
                        object_ref: coin.object_ref(),
                        balance: coin.balance,
                    });
                }
            }
            if page.has_next_page {
                cursor = page.next_cursor;
            } else {
                break;
            }
        }
        coins
    }

    pub async fn get_reference_gas_price(&self) -> u64 {
        retry_forever!(async {
            self.sui_client
                .governance_api()
                .get_reference_gas_price()
                .await
                .tap_err(|err| debug!("Failed to get reference gas price: {:?}", err))
        })
        .unwrap()
    }

    pub async fn get_latest_gas_objects(
        &self,
        object_ids: impl IntoIterator<Item = ObjectID>,
    ) -> HashMap<ObjectID, Option<GasCoin>> {
        let tasks: FuturesUnordered<_> = object_ids
            .into_iter()
            .chunks(50)
            .into_iter()
            .map(|chunk| {
                let chunk: Vec<_> = chunk.collect();
                let sui_client = self.sui_client.clone();
                tokio::spawn(async move {
                    retry_forever!(async {
                        let chunk = chunk.clone();
                        let result = sui_client
                            .clone()
                            .read_api()
                            .multi_get_object_with_options(
                                chunk.clone(),
                                SuiObjectDataOptions::default().with_bcs().with_owner(),
                            )
                            .await
                            .map_err(anyhow::Error::from)?;
                        if result.len() != chunk.len() {
                            anyhow::bail!(
                                "Unable to get all gas coins, got {} out of {}",
                                result.len(),
                                chunk.len()
                            );
                        }
                        Ok(chunk.into_iter().zip(result).collect::<Vec<_>>())
                    })
                    .unwrap()
                })
            })
            .collect();
        let objects: Vec<_> = tasks.collect().await;
        let objects: Vec<_> = objects.into_iter().flat_map(|r| r.unwrap()).collect();
        objects
            .into_iter()
            .map(|(id, response)| {
                let object = match Self::try_get_sui_coin_balance(&response) {
                    Some(coin) => {
                        debug!("Got updated gas coin info: {:?}", coin);
                        Some(coin)
                    }
                    None => {
                        debug!("Object no longer exists: {:?}", id);
                        None
                    }
                };
                (id, object)
            })
            .collect()
    }

    pub fn construct_coin_split_pt(
        gas_coin: Argument,
        split_count: u64,
    ) -> ProgrammableTransaction {
        let mut pt_builder = ProgrammableTransactionBuilder::new();
        let pure_arg = pt_builder.pure(split_count).unwrap();
        pt_builder.programmable_move_call(
            SUI_FRAMEWORK_PACKAGE_ID,
            PAY_MODULE_NAME.into(),
            PAY_SPLIT_N_FUNC_NAME.into(),
            vec![GAS::type_tag()],
            vec![gas_coin, pure_arg],
        );
        pt_builder.finish()
    }

    pub async fn calibrate_gas_cost_per_object(
        &self,
        sponsor_address: SuiAddress,
        gas_coin: &GasCoin,
    ) -> u64 {
        const SPLIT_COUNT: u64 = 500;
        let mut pt_builder = ProgrammableTransactionBuilder::new();
        let object_arg = pt_builder
            .obj(ObjectArg::ImmOrOwnedObject(gas_coin.object_ref))
            .unwrap();
        let pure_arg = pt_builder.pure(SPLIT_COUNT).unwrap();
        pt_builder.programmable_move_call(
            SUI_FRAMEWORK_PACKAGE_ID,
            PAY_MODULE_NAME.into(),
            PAY_SPLIT_N_FUNC_NAME.into(),
            vec![GAS::type_tag()],
            vec![object_arg, pure_arg],
        );
        let pt = pt_builder.finish();
        let response = retry_forever!(async {
            self.sui_client
                .read_api()
                .dev_inspect_transaction_block(
                    sponsor_address,
                    TransactionKind::ProgrammableTransaction(pt.clone()),
                    None,
                    None,
                    None,
                )
                .await
        })
        .unwrap();
        let gas_used = response.effects.gas_cost_summary().gas_used();
        // Multiply by 2 to be conservative and resilient to precision loss.
        gas_used / SPLIT_COUNT * 2
    }

    pub async fn execute_transaction(
        &self,
        tx: Transaction,
        request_type: Option<ExecuteTransactionRequestType>,
        max_attempts: usize,
    ) -> anyhow::Result<(
        Option<u64>,
        SuiTransactionBlockEffects,
        Option<SuiTransactionBlockEvents>,
    )> {
        let digest = *tx.digest();
        let request_type = request_type.or(Some(ExecuteTransactionRequestType::WaitForEffectsCert));
        debug!(?digest, "Executing transaction: {:?}", tx);
        let response = retry_with_max_attempts!(
            async {
                self.sui_client
                    .quorum_driver_api()
                    .execute_transaction_block(
                        tx.clone(),
                        SuiTransactionBlockResponseOptions::new()
                            .with_effects()
                            .with_events(),
                        request_type.clone(),
                    )
                    .await
                    .tap_err(|err| debug!(?digest, "execute_transaction error: {:?}", err))
                    .map_err(anyhow::Error::from)
            },
            max_attempts
        )?;
        let effects = response.effects.ok_or(anyhow::anyhow!("No effects"))?;
        debug!(?digest, "Transaction execution effects: {:?}", effects);
        Ok((response.timestamp_ms, effects, response.events))
    }

    /// Wait for a known valid object version to be available on the fullnode.
    pub async fn wait_for_object(&self, obj_ref: ObjectRef) {
        loop {
            let response = self
                .sui_client
                .read_api()
                .get_object_with_options(obj_ref.0, SuiObjectDataOptions::default())
                .await;
            if let Ok(SuiObjectResponse {
                data: Some(data), ..
            }) = response
            {
                if data.version == obj_ref.1 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    fn try_get_sui_coin_balance(object: &SuiObjectResponse) -> Option<GasCoin> {
        let data = object.data.as_ref()?;
        let owner = data.owner.clone()?.get_address_owner_address().ok()?;
        let object_ref = data.object_ref();
        let move_obj = data.bcs.as_ref()?.try_as_move()?;
        if move_obj.type_ != sui_types::gas_coin::GasCoin::type_() {
            return None;
        }
        let gas_coin: sui_types::gas_coin::GasCoin = bcs::from_bytes(&move_obj.bcs_bytes).ok()?;
        Some(GasCoin {
            owner,
            object_ref,
            balance: gas_coin.value(),
        })
    }
}
