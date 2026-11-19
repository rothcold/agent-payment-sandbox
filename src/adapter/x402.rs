use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapter::{ParseContext, ProtocolAdapter};
use crate::model::{PaymentAttempt, Protocol};

pub struct X402Adapter;

impl ProtocolAdapter for X402Adapter {
    fn protocol(&self) -> Protocol {
        Protocol::X402
    }
    fn name(&self) -> &'static str {
        "x402"
    }
    fn parse_value(&self, ctx: &ParseContext, value: &Value) -> Result<PaymentAttempt> {
        let challenge: Challenge =
            serde_json::from_value(value.clone()).context("parse x402 challenge body")?;
        let requirement = challenge
            .accepts
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("x402 challenge has no accepts entry"))?;
        requirement_to_attempt(ctx, requirement, value)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Challenge {
    #[serde(rename = "x402Version")]
    pub x402_version: u32,
    pub accepts: Vec<Requirement>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    pub scheme: String,
    pub network: String,
    #[serde(rename = "maxAmountRequired")]
    pub max_amount_required: String,
    pub resource: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
    #[serde(rename = "payTo")]
    pub pay_to: String,
    pub asset: String,
    #[serde(rename = "maxTimeoutSeconds", default)]
    pub max_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub extra: Option<Value>,
}

fn requirement_to_attempt(
    ctx: &ParseContext,
    req: Requirement,
    raw_challenge: &Value,
) -> Result<PaymentAttempt> {
    let merchant = host_from_resource(&req.resource).unwrap_or_else(|| req.pay_to.clone());
    let token_name = req
        .extra
        .as_ref()
        .and_then(|v| v.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let currency = token_name
        .clone()
        .or_else(|| known_asset_symbol(&req.asset))
        .unwrap_or_else(|| req.asset.clone());
    let decimals = decimals_for(&currency);
    let amount = atomic_to_decimal(&req.max_amount_required, decimals)?;
    let network = network_to_caip2(&req.network).unwrap_or_else(|| req.network.clone());

    let mut attempt = PaymentAttempt::new(
        Protocol::X402,
        ctx.agent_id.clone().unwrap_or_else(|| "agent".to_string()),
        ctx.task_id.clone().unwrap_or_else(|| "task".to_string()),
        merchant,
        amount,
        currency,
    );
    attempt.network = Some(network);
    attempt.resource = Some(req.resource.clone());
    attempt.payment_requirement = Some(serde_json::to_value(&req)?);
    attempt.wallet = ctx.wallet.clone();
    attempt.metadata.insert(
        "x402_version".to_string(),
        serde_json::Value::from(
            raw_challenge
                .get("x402Version")
                .cloned()
                .unwrap_or(Value::Null),
        ),
    );
    attempt.metadata.insert(
        "asset_decimals".to_string(),
        serde_json::Value::from(decimals),
    );
    attempt.metadata.insert(
        "atomic_amount".to_string(),
        serde_json::Value::from(req.max_amount_required.clone()),
    );
    Ok(attempt)
}

fn host_from_resource(resource: &str) -> Option<String> {
    let after_scheme = resource.split("://").nth(1)?;
    let host = after_scheme.split('/').next()?;
    Some(host.to_string())
}

fn known_asset_symbol(asset: &str) -> Option<String> {
    match asset.to_lowercase().as_str() {
        "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913" => Some("USDC".to_string()),
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48" => Some("USDC".to_string()),
        _ => None,
    }
}

fn decimals_for(currency: &str) -> u32 {
    match currency.to_uppercase().as_str() {
        "USDC" | "USDT" => 6,
        "ETH" | "WETH" | "DAI" => 18,
        _ => 6,
    }
}

fn atomic_to_decimal(atomic: &str, decimals: u32) -> Result<Decimal> {
    let mut raw = Decimal::from_str(atomic).context("atomic amount is not a number")?;
    raw.set_scale(decimals)
        .map_err(|e| anyhow!(e.to_string()))?;
    Ok(raw)
}

fn network_to_caip2(name: &str) -> Option<String> {
    Some(
        match name {
            "base" => "eip155:8453",
            "base-sepolia" => "eip155:84532",
            "ethereum" | "mainnet" => "eip155:1",
            "polygon" => "eip155:137",
            "arbitrum" => "eip155:42161",
            "optimism" => "eip155:10",
            "avalanche" => "eip155:43114",
            "avalanche-fuji" => "eip155:43113",
            _ => return None,
        }
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn sample_challenge() -> Value {
        serde_json::json!({
            "x402Version": 1,
            "accepts": [
                {
                    "scheme": "exact",
                    "network": "base",
                    "maxAmountRequired": "10000",
                    "resource": "https://api.example.com/v1/data",
                    "description": "Sample API",
                    "mimeType": "application/json",
                    "payTo": "0x000000000000000000000000000000000000dEaD",
                    "asset": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
                    "maxTimeoutSeconds": 60,
                    "extra": { "name": "USDC", "version": "2" }
                }
            ]
        })
    }

    #[test]
    fn parses_full_challenge_to_attempt() {
        let adapter = X402Adapter;
        let ctx = ParseContext {
            agent_id: Some("agent-1".to_string()),
            task_id: Some("task-1".to_string()),
            wallet: Some("0xabc".to_string()),
        };
        let attempt = adapter.parse_value(&ctx, &sample_challenge()).unwrap();
        assert_eq!(attempt.merchant, "api.example.com");
        assert_eq!(attempt.amount, Decimal::from_str("0.01").unwrap());
        assert_eq!(attempt.currency, "USDC");
        assert_eq!(attempt.network.as_deref(), Some("eip155:8453"));
        assert_eq!(
            attempt.resource.as_deref(),
            Some("https://api.example.com/v1/data")
        );
        assert_eq!(attempt.wallet.as_deref(), Some("0xabc"));
        assert_eq!(
            attempt
                .metadata
                .get("atomic_amount")
                .and_then(|v| v.as_str()),
            Some("10000")
        );
        assert_eq!(
            attempt
                .metadata
                .get("asset_decimals")
                .and_then(|v| v.as_u64()),
            Some(6)
        );
    }

    #[test]
    fn unknown_network_passes_through() {
        let mut c = sample_challenge();
        c["accepts"][0]["network"] = serde_json::json!("solana");
        let attempt = X402Adapter
            .parse_value(&ParseContext::default(), &c)
            .unwrap();
        assert_eq!(attempt.network.as_deref(), Some("solana"));
    }

    #[test]
    fn falls_back_to_asset_address_when_no_extra_name() {
        let mut c = sample_challenge();
        c["accepts"][0]["extra"] = Value::Null;
        c["accepts"][0]["asset"] = serde_json::json!("0xdeadbeef00000000000000000000000000000000");
        let attempt = X402Adapter
            .parse_value(&ParseContext::default(), &c)
            .unwrap();
        // Unknown asset → currency falls back to the asset address.
        assert!(attempt.currency.starts_with("0x"));
    }

    #[test]
    fn empty_accepts_errors() {
        let value = serde_json::json!({"x402Version": 1, "accepts": []});
        let err = X402Adapter
            .parse_value(&ParseContext::default(), &value)
            .unwrap_err();
        assert!(err.to_string().contains("no accepts"));
    }
}
