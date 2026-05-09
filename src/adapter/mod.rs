use anyhow::Result;

use crate::model::{PaymentAttempt, Protocol};

pub mod ap2;
pub mod x402;

#[derive(Debug, Clone, Default)]
pub struct ParseContext {
    pub agent_id: Option<String>,
    pub task_id: Option<String>,
    pub wallet: Option<String>,
}

pub trait ProtocolAdapter: Send + Sync {
    fn protocol(&self) -> Protocol;
    fn name(&self) -> &'static str;
    fn parse_value(&self, ctx: &ParseContext, value: &serde_json::Value) -> Result<PaymentAttempt>;
}
