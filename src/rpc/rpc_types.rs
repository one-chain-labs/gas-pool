// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::types::ReservationID;
use fastcrypto::encoding::Base64;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sui_json_rpc_types::{SuiObjectRef, SuiTransactionBlockEffects, SuiTransactionBlockEvents};
use sui_types::base_types::{ObjectRef, SuiAddress};
use sui_types::quorum_driver_types::ExecuteTransactionRequestType;

// 2 SUI.
pub const MAX_BUDGET: u64 = 2_000_000_000;

// 10 mins.
pub const MAX_DURATION_S: u64 = 10 * 60;

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
pub struct ReserveGasRequest {
    pub sponsor_address: Option<SuiAddress>,
    pub gas_budget: u64,
    pub reserve_duration_secs: u64,
}

impl ReserveGasRequest {
    pub fn check_validity(&self) -> anyhow::Result<()> {
        if self.gas_budget == 0 {
            anyhow::bail!("Gas budget must be positive");
        }
        if self.gas_budget > MAX_BUDGET {
            anyhow::bail!("Gas budget must be less than {}", MAX_BUDGET);
        }
        if self.reserve_duration_secs == 0 {
            anyhow::bail!("Reserve duration must be positive");
        }
        if self.reserve_duration_secs > MAX_DURATION_S {
            anyhow::bail!(
                "Reserve duration must be less than {} seconds",
                MAX_DURATION_S
            );
        }
        Ok(())
    }
}

#[derive(Debug, JsonSchema, Serialize, Deserialize)]
pub struct ReserveGasResponse {
    pub result: Option<ReserveGasResult>,
    pub error: Option<String>,
}

#[derive(Debug, JsonSchema, Serialize, Deserialize)]
pub struct ReserveGasResult {
    pub sponsor_address: SuiAddress,
    pub reservation_id: ReservationID,
    pub gas_coins: Vec<SuiObjectRef>,
}

impl ReserveGasResponse {
    pub fn new_ok(
        sponsor_address: SuiAddress,
        reservation_id: ReservationID,
        gas_coins: Vec<ObjectRef>,
    ) -> Self {
        Self {
            result: Some(ReserveGasResult {
                sponsor_address,
                reservation_id,
                gas_coins: gas_coins.into_iter().map(|c| c.into()).collect(),
            }),
            error: None,
        }
    }

    pub fn new_err(error: anyhow::Error) -> Self {
        Self {
            result: None,
            error: Some(error.to_string()),
        }
    }
}

#[derive(Debug, JsonSchema, Serialize, Deserialize)]
pub struct ExecuteTxRequest {
    pub reservation_id: ReservationID,
    pub tx_bytes: Base64,
    pub request_type: Option<ExecuteTransactionRequestType>,
    pub user_sig: Base64,
}

#[derive(Debug, JsonSchema, Serialize, Deserialize)]
pub struct ExecuteTxResponse {
    pub timestamp_ms: Option<u64>,
    pub effects: Option<SuiTransactionBlockEffects>,
    pub events: Option<SuiTransactionBlockEvents>,
    pub error: Option<String>,
}

impl ExecuteTxResponse {
    pub fn new_ok(
        timestamp_ms: Option<u64>,
        effects: SuiTransactionBlockEffects,
        events: Option<SuiTransactionBlockEvents>,
    ) -> Self {
        Self {
            timestamp_ms,
            effects: Some(effects),
            events,
            error: None,
        }
    }

    pub fn new_err(error: anyhow::Error) -> Self {
        Self {
            timestamp_ms: None,
            effects: None,
            events: None,
            error: Some(error.to_string()),
        }
    }
}

#[derive(Debug, JsonSchema, Serialize, Deserialize)]
pub struct SupportAddressResponse {
    pub sponsor_addresses: Option<Vec<SuiAddress>>,
    pub error: Option<String>,
}

impl SupportAddressResponse {
    pub fn new_ok(sponsor_addresses: Vec<SuiAddress>) -> Self {
        Self {
            sponsor_addresses: Some(sponsor_addresses),
            error: None,
        }
    }

    pub fn new_err(error: anyhow::Error) -> Self {
        Self {
            sponsor_addresses: None,
            error: Some(error.to_string()),
        }
    }
}
