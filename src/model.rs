use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    X402,
    Ap2,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentAttempt {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub protocol: Protocol,
    pub agent_id: String,
    pub task_id: String,
    pub merchant: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    pub currency: String,
    pub network: Option<String>,
    pub resource: Option<String>,
    pub mandate: Option<serde_json::Value>,
    pub payment_requirement: Option<serde_json::Value>,
    pub wallet: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

impl PaymentAttempt {
    pub fn new(
        protocol: Protocol,
        agent_id: impl Into<String>,
        task_id: impl Into<String>,
        merchant: impl Into<String>,
        amount: Decimal,
        currency: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            protocol,
            agent_id: agent_id.into(),
            task_id: task_id.into(),
            merchant: merchant.into(),
            amount,
            currency: currency.into(),
            network: None,
            resource: None,
            mandate: None,
            payment_requirement: None,
            wallet: None,
            metadata: serde_json::Map::new(),
        }
    }
}
