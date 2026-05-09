use std::collections::HashMap;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AssetKey {
    pub currency: String,
    pub network: Option<String>,
}

impl AssetKey {
    pub fn new(currency: impl Into<String>, network: Option<String>) -> Self {
        Self {
            currency: currency.into(),
            network,
        }
    }

    pub fn as_string_key(&self) -> String {
        match &self.network {
            Some(n) => format!("{}|{}", self.currency, n),
            None => format!("{}|", self.currency),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AssetBalance {
    #[serde(with = "rust_decimal::serde::str")]
    pub available: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub reserved: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementState {
    Reserved,
    Pending,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reservation {
    pub id: Uuid,
    pub attempt_id: Uuid,
    pub task_id: String,
    pub asset: AssetKey,
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    pub state: SettlementState,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentRecord {
    pub timestamp: DateTime<Utc>,
    pub agent_id: String,
    pub task_id: String,
    pub merchant: String,
    pub resource: Option<String>,
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    pub currency: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WalletState {
    pub balances: HashMap<String, AssetBalance>,
    pub task_spend: HashMap<String, Decimal>,
    pub reservations: HashMap<Uuid, Reservation>,
    #[serde(default)]
    pub history: Vec<PaymentRecord>,
}

impl WalletState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed(&mut self, asset: &AssetKey, amount: Decimal) {
        let entry = self.balances.entry(asset.as_string_key()).or_default();
        entry.available += amount;
    }

    pub fn balance(&self, asset: &AssetKey) -> AssetBalance {
        self.balances
            .get(&asset.as_string_key())
            .cloned()
            .unwrap_or_default()
    }

    pub fn task_total(&self, task_id: &str) -> Decimal {
        self.task_spend
            .get(task_id)
            .copied()
            .unwrap_or(Decimal::ZERO)
    }

    pub fn record_payment(&mut self, rec: PaymentRecord) {
        self.history.push(rec);
    }

    pub fn agent_total_since(&self, agent_id: &str, since: DateTime<Utc>) -> Decimal {
        self.history
            .iter()
            .filter(|r| r.agent_id == agent_id && r.timestamp >= since)
            .map(|r| r.amount)
            .sum()
    }

    pub fn count_since(&self, since: DateTime<Utc>) -> usize {
        self.history.iter().filter(|r| r.timestamp >= since).count()
    }

    pub fn has_duplicate_within(
        &self,
        merchant: &str,
        resource: Option<&str>,
        amount: Decimal,
        currency: &str,
        since: DateTime<Utc>,
    ) -> bool {
        self.history.iter().any(|r| {
            r.timestamp >= since
                && r.merchant == merchant
                && r.amount == amount
                && r.currency == currency
                && r.resource.as_deref() == resource
        })
    }

    pub fn reserve(
        &mut self,
        attempt_id: Uuid,
        task_id: &str,
        asset: AssetKey,
        amount: Decimal,
    ) -> Result<Reservation> {
        let key = asset.as_string_key();
        let bal = self
            .balances
            .get_mut(&key)
            .ok_or_else(|| anyhow!("asset {key} not seeded"))?;
        if bal.available < amount {
            return Err(anyhow!(
                "insufficient funds for {key}: available={} need={}",
                bal.available,
                amount
            ));
        }
        bal.available -= amount;
        bal.reserved += amount;

        let reservation = Reservation {
            id: Uuid::new_v4(),
            attempt_id,
            task_id: task_id.to_string(),
            asset,
            amount,
            state: SettlementState::Reserved,
            created_at: Utc::now(),
        };
        self.reservations
            .insert(reservation.id, reservation.clone());
        Ok(reservation)
    }

    pub fn settle(&mut self, reservation_id: Uuid) -> Result<Reservation> {
        let res = self
            .reservations
            .get_mut(&reservation_id)
            .ok_or_else(|| anyhow!("reservation {reservation_id} not found"))?;
        if !matches!(
            res.state,
            SettlementState::Reserved | SettlementState::Pending
        ) {
            return Err(anyhow!("reservation already finalized"));
        }
        res.state = SettlementState::Succeeded;

        let asset_key = res.asset.as_string_key();
        let amount = res.amount;
        let task_id = res.task_id.clone();
        let result = res.clone();

        if let Some(bal) = self.balances.get_mut(&asset_key) {
            bal.reserved -= amount;
        }
        *self.task_spend.entry(task_id).or_insert(Decimal::ZERO) += amount;
        Ok(result)
    }

    pub fn fail(&mut self, reservation_id: Uuid) -> Result<Reservation> {
        let res = self
            .reservations
            .get_mut(&reservation_id)
            .ok_or_else(|| anyhow!("reservation {reservation_id} not found"))?;
        if !matches!(
            res.state,
            SettlementState::Reserved | SettlementState::Pending
        ) {
            return Err(anyhow!("reservation already finalized"));
        }
        res.state = SettlementState::Failed;

        let asset_key = res.asset.as_string_key();
        let amount = res.amount;
        let result = res.clone();

        if let Some(bal) = self.balances.get_mut(&asset_key) {
            bal.reserved -= amount;
            bal.available += amount;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn usdc_base() -> AssetKey {
        AssetKey::new("USDC", Some("eip155:8453".to_string()))
    }

    #[test]
    fn seed_and_balance() {
        let mut w = WalletState::new();
        w.seed(&usdc_base(), Decimal::from_str("10.0").unwrap());
        assert_eq!(
            w.balance(&usdc_base()).available,
            Decimal::from_str("10.0").unwrap()
        );
    }

    #[test]
    fn reserve_and_settle_updates_balances_and_task_total() {
        let mut w = WalletState::new();
        w.seed(&usdc_base(), Decimal::from_str("10.0").unwrap());
        let res = w
            .reserve(
                Uuid::new_v4(),
                "task-1",
                usdc_base(),
                Decimal::from_str("0.25").unwrap(),
            )
            .unwrap();
        let bal = w.balance(&usdc_base());
        assert_eq!(bal.available, Decimal::from_str("9.75").unwrap());
        assert_eq!(bal.reserved, Decimal::from_str("0.25").unwrap());

        w.settle(res.id).unwrap();
        let bal = w.balance(&usdc_base());
        assert_eq!(bal.available, Decimal::from_str("9.75").unwrap());
        assert_eq!(bal.reserved, Decimal::ZERO);
        assert_eq!(w.task_total("task-1"), Decimal::from_str("0.25").unwrap());
    }

    #[test]
    fn fail_returns_funds() {
        let mut w = WalletState::new();
        w.seed(&usdc_base(), Decimal::from_str("1.0").unwrap());
        let res = w
            .reserve(
                Uuid::new_v4(),
                "task-1",
                usdc_base(),
                Decimal::from_str("0.5").unwrap(),
            )
            .unwrap();
        w.fail(res.id).unwrap();
        let bal = w.balance(&usdc_base());
        assert_eq!(bal.available, Decimal::from_str("1.0").unwrap());
        assert_eq!(bal.reserved, Decimal::ZERO);
        assert_eq!(w.task_total("task-1"), Decimal::ZERO);
    }

    #[test]
    fn insufficient_funds_errors() {
        let mut w = WalletState::new();
        w.seed(&usdc_base(), Decimal::from_str("0.10").unwrap());
        let err = w.reserve(
            Uuid::new_v4(),
            "task-1",
            usdc_base(),
            Decimal::from_str("0.20").unwrap(),
        );
        assert!(err.is_err());
    }
}
