// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

mod script_manager;

use crate::metrics::StorageMetrics;
use crate::storage::redis::script_manager::ScriptManager;
use crate::storage::Storage;
use crate::types::{GasCoin, ReservationID};
use chrono::Utc;
use redis::aio::ConnectionManager;
use std::collections::HashMap;
use std::ops::Add;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use sui_types::base_types::{ObjectDigest, ObjectID, SequenceNumber, SuiAddress};
use tracing::{debug, info};

pub struct RedisStorage {
    conn_manager: ConnectionManager,
    // String format of the sponsor address to avoid converting it to string multiple times.
    sponsor_vec: Vec<String>,
    metrics: Arc<StorageMetrics>,
}

impl RedisStorage {
    pub async fn new(
        redis_url: &str,
        sponsor_vec: Vec<SuiAddress>,
        metrics: Arc<StorageMetrics>,
    ) -> Self {
        let client = redis::Client::open(redis_url).unwrap();
        let conn_manager = ConnectionManager::new(client).await.unwrap();
        Self {
            conn_manager,
            sponsor_vec: sponsor_vec.into_iter().map(|s| s.to_string()).collect(),
            metrics,
        }
    }
}

#[async_trait::async_trait]
impl Storage for RedisStorage {
    async fn reserve_gas_coins(
        &self,
        sponsor: SuiAddress,
        target_budget: u64,
        reserved_duration_ms: u64,
    ) -> anyhow::Result<(ReservationID, Vec<GasCoin>)> {
        self.metrics.num_reserve_gas_coins_requests.inc();
        let sponsor_str = sponsor.to_string();

        let expiration_time = Utc::now()
            .add(Duration::from_millis(reserved_duration_ms))
            .timestamp_millis() as u64;
        let mut conn = self.conn_manager.clone();
        let (reservation_id, coins, new_total_balance, new_coin_count): (
            ReservationID,
            Vec<String>,
            i64,
            i64,
        ) = ScriptManager::reserve_gas_coins_script()
            .arg(&sponsor_str)
            .arg(target_budget)
            .arg(expiration_time)
            .invoke_async(&mut conn)
            .await?;
        // The script returns (0, []) if it is unable to find enough coins to reserve.
        // We choose to handle the error here instead of inside the script so that we could
        // provide a more readable error message.
        if coins.is_empty() {
            return Err(anyhow::anyhow!(
                "Unable to reserve gas coins for the given budget."
            ));
        }
        let gas_coins: Vec<_> = coins
            .into_iter()
            .map(|s| {
                // Each coin is in the form of: balance,object_id,version,digest
                let mut splits = s.split(',');
                let balance = splits.next().unwrap().parse::<u64>().unwrap();
                let object_id = ObjectID::from_str(splits.next().unwrap()).unwrap();
                let version = SequenceNumber::from(splits.next().unwrap().parse::<u64>().unwrap());
                let digest = ObjectDigest::from_str(splits.next().unwrap()).unwrap();
                GasCoin {
                    owner: sponsor,
                    balance,
                    object_ref: (object_id, version, digest),
                }
            })
            .collect();

        self.metrics
            .gas_pool_available_gas_coin_count
            .with_label_values(&[&sponsor_str])
            .set(new_coin_count);
        self.metrics
            .gas_pool_available_gas_total_balance
            .with_label_values(&[&sponsor_str])
            .set(new_total_balance);
        self.metrics.num_successful_reserve_gas_coins_requests.inc();
        Ok((reservation_id, gas_coins))
    }

    async fn ready_for_execution(
        &self,
        sponsor: SuiAddress,
        reservation_id: ReservationID,
    ) -> anyhow::Result<()> {
        self.metrics.num_ready_for_execution_requests.inc();

        let mut conn = self.conn_manager.clone();
        ScriptManager::ready_for_execution_script()
            .arg(sponsor.to_string())
            .arg(reservation_id)
            .invoke_async::<_, ()>(&mut conn)
            .await?;

        self.metrics
            .num_successful_ready_for_execution_requests
            .inc();
        Ok(())
    }

    async fn add_new_coins(&self, new_coins: Vec<GasCoin>) -> anyhow::Result<()> {
        if new_coins.is_empty() {
            return Ok(());
        }
        self.metrics.num_add_new_coins_requests.inc();
        let mut formatted_coin_maps = HashMap::new();
        for c in new_coins {
            formatted_coin_maps
                .entry(c.owner)
                .or_insert_with(Vec::new)
                .push(format!(
                    "{},{},{},{}",
                    c.balance,
                    c.object_ref.0,
                    c.object_ref.1.value(),
                    c.object_ref.2
                ))
        }
        let mut conn = self.conn_manager.clone();
        let results: String = ScriptManager::add_new_coins_script()
            .arg(serde_json::to_string(&formatted_coin_maps)?)
            .invoke_async(&mut conn)
            .await?;

        let results = serde_json::from_str::<Vec<(String, i64, i64)>>(&results)?;

        for (sponsor, new_total_balance, new_coin_count) in results {
            debug!(
                "After add_new_coins. New total balance: {}, new coin count: {}",
                new_total_balance, new_coin_count
            );
            self.metrics
                .gas_pool_available_gas_coin_count
                .with_label_values(&[&sponsor])
                .set(new_coin_count as i64);
            self.metrics
                .gas_pool_available_gas_total_balance
                .with_label_values(&[&sponsor])
                .set(new_total_balance as i64);
            self.metrics.num_successful_add_new_coins_requests.inc();
        }
        Ok(())
    }

    async fn expire_coins(&self) -> anyhow::Result<Vec<ObjectID>> {
        self.metrics.num_expire_coins_requests.inc();

        let now = Utc::now().timestamp_millis() as u64;
        let mut conn = self.conn_manager.clone();
        let expired_coin_strings: Vec<String> = ScriptManager::expire_coins_script()
            .arg(serde_json::to_string(&self.sponsor_vec)?)
            .arg(now)
            .invoke_async(&mut conn)
            .await?;
        // The script returns a list of comma separated coin ids.
        let expired_coin_ids = expired_coin_strings
            .iter()
            .flat_map(|s| s.split(',').map(|id| ObjectID::from_str(id).unwrap()))
            .collect();

        self.metrics.num_successful_expire_coins_requests.inc();
        Ok(expired_coin_ids)
    }

    async fn init_coin_stats_at_startup(&self) -> anyhow::Result<Vec<(String, i64, i64)>> {
        let mut conn = self.conn_manager.clone();
        let results: String = ScriptManager::init_coin_stats_at_startup_script()
            .arg(serde_json::to_string(&self.sponsor_vec)?)
            .invoke_async(&mut conn)
            .await?;

        let results = serde_json::from_str::<Vec<(String, i64, i64)>>(&results)?;

        for (sponsor, available_coin_count, available_coin_total_balance) in results.clone() {
            info!(
                sponsor_address=?sponsor,
                "Number of available gas coins in the pool: {}, total balance: {}",
                available_coin_count,
                available_coin_total_balance
            );
            self.metrics
                .gas_pool_available_gas_coin_count
                .with_label_values(&[&sponsor])
                .set(available_coin_count);
            self.metrics
                .gas_pool_available_gas_total_balance
                .with_label_values(&[&sponsor])
                .set(available_coin_total_balance);
        }
        Ok(results)
    }

    async fn is_initialized(&self) -> anyhow::Result<bool> {
        let mut conn = self.conn_manager.clone();
        let result = ScriptManager::get_is_initialized_script()
            .arg(serde_json::to_string(&self.sponsor_vec)?)
            .invoke_async::<_, bool>(&mut conn)
            .await?;
        Ok(result)
    }

    async fn acquire_init_lock(
        &self,
        lock_duration_sec: u64,
    ) -> anyhow::Result<Vec<(String, bool)>> {
        let mut conn = self.conn_manager.clone();
        let cur_timestamp = Utc::now().timestamp() as u64;
        debug!(
            "Acquiring init lock at {} for {} seconds",
            cur_timestamp, lock_duration_sec
        );
        let results: String = ScriptManager::acquire_init_lock_script()
            .arg(serde_json::to_string(&self.sponsor_vec)?)
            .arg(cur_timestamp)
            .arg(lock_duration_sec)
            .invoke_async(&mut conn)
            .await?;

        let results = serde_json::from_str::<Vec<(String, bool)>>(&results)?;

        Ok(results)
    }

    async fn release_init_lock(&self) -> anyhow::Result<()> {
        debug!("Releasing the init lock.");
        let mut conn = self.conn_manager.clone();
        ScriptManager::release_init_lock_script()
            .arg(serde_json::to_string(&self.sponsor_vec)?)
            .invoke_async::<_, ()>(&mut conn)
            .await?;
        Ok(())
    }

    async fn check_health(&self) -> anyhow::Result<()> {
        let mut conn = self.conn_manager.clone();
        redis::cmd("PING").query_async(&mut conn).await?;
        Ok(())
    }

    #[cfg(test)]
    async fn flush_db(&self) {
        let mut conn = self.conn_manager.clone();
        redis::cmd("FLUSHDB")
            .query_async::<_, String>(&mut conn)
            .await
            .unwrap();
    }

    async fn get_available_coin_count(&self, sponsor: SuiAddress) -> anyhow::Result<usize> {
        let mut conn = self.conn_manager.clone();
        let count = ScriptManager::get_available_coin_count_script()
            .arg(sponsor.to_string())
            .invoke_async::<_, usize>(&mut conn)
            .await?;
        Ok(count)
    }

    async fn get_available_coin_total_balance(&self, sponsor: SuiAddress) -> u64 {
        let mut conn = self.conn_manager.clone();
        ScriptManager::get_available_coin_total_balance_script()
            .arg(sponsor.to_string())
            .invoke_async::<_, u64>(&mut conn)
            .await
            .unwrap()
    }

    #[cfg(test)]
    async fn get_reserved_coin_count(&self, sponsor: SuiAddress) -> usize {
        let mut conn = self.conn_manager.clone();
        ScriptManager::get_reserved_coin_count_script()
            .arg(sponsor.to_string())
            .invoke_async::<_, usize>(&mut conn)
            .await
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use sui_types::base_types::{random_object_ref, SuiAddress};

    use crate::{
        metrics::StorageMetrics,
        storage::{redis::RedisStorage, Storage},
        types::GasCoin,
    };

    #[tokio::test]
    async fn test_init_coin_stats_at_startup() {
        let storage = setup_storage().await;
        storage
            .add_new_coins(vec![
                GasCoin {
                    owner: SuiAddress::ZERO,
                    balance: 100,
                    object_ref: random_object_ref(),
                },
                GasCoin {
                    owner: SuiAddress::ZERO,
                    balance: 200,
                    object_ref: random_object_ref(),
                },
            ])
            .await
            .unwrap();
        let results = storage.init_coin_stats_at_startup().await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, 2);
        assert_eq!(results[0].2, 300);
    }

    #[tokio::test]
    async fn test_add_new_coins() {
        let storage = setup_storage().await;
        let sponsor = SuiAddress::ZERO;
        storage
            .add_new_coins(vec![
                GasCoin {
                    owner: sponsor,
                    balance: 100,
                    object_ref: random_object_ref(),
                },
                GasCoin {
                    owner: sponsor,
                    balance: 200,
                    object_ref: random_object_ref(),
                },
            ])
            .await
            .unwrap();
        let coin_count = storage.get_available_coin_count(sponsor).await.unwrap();
        assert_eq!(coin_count, 2);
        let total_balance = storage.get_available_coin_total_balance(sponsor).await;
        assert_eq!(total_balance, 300);
        storage
            .add_new_coins(vec![
                GasCoin {
                    owner: sponsor,
                    balance: 300,
                    object_ref: random_object_ref(),
                },
                GasCoin {
                    owner: sponsor,
                    balance: 400,
                    object_ref: random_object_ref(),
                },
            ])
            .await
            .unwrap();
        let coin_count = storage.get_available_coin_count(sponsor).await.unwrap();
        assert_eq!(coin_count, 4);
        let total_balance = storage.get_available_coin_total_balance(sponsor).await;
        assert_eq!(total_balance, 1000);
    }

    async fn setup_storage() -> RedisStorage {
        let storage = RedisStorage::new(
            "redis://127.0.0.1:6379",
            vec![SuiAddress::ZERO],
            StorageMetrics::new_for_testing(),
        )
        .await;
        storage.flush_db().await;
        let results = storage.init_coin_stats_at_startup().await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, 0);
        assert_eq!(results[0].2, 0);
        storage
    }
}
