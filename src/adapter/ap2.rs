use anyhow::{Context, Result, anyhow};
use chrono::DateTime;
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;

use crate::adapter::{ParseContext, ProtocolAdapter};
use crate::model::{PaymentAttempt, Protocol};

/// AP2 spec snapshot this adapter was built against. Update together with the changelog
/// in `docs/ap2-adapter-changelog.md` whenever fields here are revised.
pub const AP2_SPEC_SNAPSHOT: &str = "2026-05-02";

pub struct Ap2Adapter;

impl ProtocolAdapter for Ap2Adapter {
    fn protocol(&self) -> Protocol {
        Protocol::Ap2
    }
    fn name(&self) -> &'static str {
        "ap2"
    }
    fn parse_value(&self, ctx: &ParseContext, value: &Value) -> Result<PaymentAttempt> {
        let parsed = ParsedMandate::extract(value)?;

        let agent_id = ctx
            .agent_id
            .clone()
            .or_else(|| parsed.agent_id.clone())
            .unwrap_or_else(|| "agent".to_string());
        let task_id = ctx
            .task_id
            .clone()
            .or_else(|| parsed.task_id.clone())
            .unwrap_or_else(|| "task".to_string());

        let mut attempt = PaymentAttempt::new(
            Protocol::Ap2,
            agent_id,
            task_id,
            parsed.merchant.clone().unwrap_or_default(),
            parsed.amount.unwrap_or(Decimal::ZERO),
            parsed.currency.clone().unwrap_or_default(),
        );
        attempt.network = parsed.network.clone();
        attempt.wallet = ctx.wallet.clone();
        attempt.mandate = Some(value.clone());

        attempt.metadata.insert(
            "ap2_spec_snapshot".to_string(),
            Value::from(AP2_SPEC_SNAPSHOT),
        );
        if let Some(t) = &parsed.mandate_type {
            attempt
                .metadata
                .insert("ap2_mandate_type".to_string(), Value::from(t.clone()));
        }
        if let Some(e) = &parsed.expires_at {
            attempt
                .metadata
                .insert("ap2_expires_at".to_string(), Value::from(e.clone()));
        }
        if !parsed.merchant_allow_list.is_empty() {
            attempt.metadata.insert(
                "ap2_merchant_allow".to_string(),
                Value::from(parsed.merchant_allow_list.clone()),
            );
        }
        if let Some(limit) = &parsed.spending_limit {
            attempt.metadata.insert(
                "ap2_spending_limit".to_string(),
                serde_json::json!({
                    "amount": limit.amount.to_string(),
                    "currency": limit.currency,
                    "period": limit.period,
                }),
            );
        }

        Ok(attempt)
    }
}

#[derive(Debug, Default)]
struct ParsedMandate {
    mandate_type: Option<String>,
    merchant: Option<String>,
    amount: Option<Decimal>,
    currency: Option<String>,
    network: Option<String>,
    expires_at: Option<String>,
    agent_id: Option<String>,
    task_id: Option<String>,
    merchant_allow_list: Vec<String>,
    spending_limit: Option<SpendingLimit>,
}

#[derive(Debug)]
struct SpendingLimit {
    amount: Decimal,
    currency: String,
    period: Option<String>,
}

impl ParsedMandate {
    fn extract(value: &Value) -> Result<Self> {
        let subject = pick_subject(value).context("AP2 mandate has no credentialSubject")?;

        let mandate_type = type_array(value)
            .into_iter()
            .find(|t| t.ends_with("Mandate"))
            .or_else(|| {
                value
                    .get("mandateType")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            });

        let intent = subject
            .get("intent")
            .or_else(|| subject.get("payment"))
            .or_else(|| subject.get("paymentIntent"))
            .or_else(|| subject.get("transaction"));

        let amount_obj = intent
            .and_then(|i| i.get("amount"))
            .or_else(|| subject.get("amount"));
        let (amount, currency) = match amount_obj {
            Some(obj) => parse_amount(obj)?,
            None => (None, None),
        };

        let merchant = intent
            .and_then(|i| {
                i.get("merchant")
                    .and_then(|m| m.get("id").or_else(|| m.get("name")).or(Some(m)))
                    .and_then(|m| m.as_str())
                    .map(String::from)
                    .or_else(|| i.get("payee").and_then(|v| v.as_str()).map(String::from))
            })
            .or_else(|| {
                subject
                    .get("merchant")
                    .and_then(|v| v.get("id").or_else(|| v.get("name")).or(Some(v)))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            });

        let network = intent
            .and_then(|i| i.get("network"))
            .or_else(|| subject.get("network"))
            .and_then(|v| v.as_str())
            .map(String::from);

        let expires_at = value
            .get("expirationDate")
            .or_else(|| value.get("validUntil"))
            .or_else(|| subject.get("expiresAt"))
            .and_then(|v| v.as_str())
            .map(String::from);

        if let Some(s) = &expires_at {
            DateTime::parse_from_rfc3339(s)
                .map_err(|_| anyhow!("ap2 expiry not a valid RFC3339 timestamp: {s}"))?;
        }

        let agent_id = subject
            .get("agentId")
            .or_else(|| subject.get("agent").and_then(|v| v.get("id")))
            .and_then(|v| v.as_str())
            .map(String::from);

        let task_id = subject
            .get("taskId")
            .or_else(|| intent.and_then(|i| i.get("taskId")))
            .and_then(|v| v.as_str())
            .map(String::from);

        let constraints = subject
            .get("constraints")
            .or_else(|| intent.and_then(|i| i.get("constraints")));

        let merchant_allow_list = constraints
            .and_then(|c| {
                c.get("merchantAllowList")
                    .or_else(|| c.get("allowedMerchants"))
            })
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let spending_limit = constraints
            .and_then(|c| {
                c.get("spendingLimit")
                    .or_else(|| c.get("spendingLimits"))
                    .or_else(|| c.get("maxAmount"))
            })
            .and_then(|v| {
                if v.is_array() {
                    v.as_array().and_then(|a| a.first()).cloned()
                } else {
                    Some(v.clone())
                }
            })
            .map(parse_spending_limit)
            .transpose()?
            .flatten();

        Ok(Self {
            mandate_type,
            merchant,
            amount,
            currency,
            network,
            expires_at,
            agent_id,
            task_id,
            merchant_allow_list,
            spending_limit,
        })
    }
}

fn pick_subject(value: &Value) -> Option<&Value> {
    value
        .get("credentialSubject")
        .or_else(|| value.get("subject"))
        .or(Some(value))
}

fn type_array(value: &Value) -> Vec<String> {
    match value.get("type") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => vec![],
    }
}

fn parse_amount(obj: &Value) -> Result<(Option<Decimal>, Option<String>)> {
    if let Some(s) = obj.as_str() {
        return Ok((Decimal::from_str(s).ok(), None));
    }
    let value = obj.get("value").or_else(|| obj.get("amount"));
    let currency = obj
        .get("currency")
        .or_else(|| obj.get("currencyCode"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let amount = match value {
        Some(Value::String(s)) => Decimal::from_str(s).ok(),
        Some(Value::Number(n)) => n
            .as_f64()
            .and_then(|f| Decimal::from_str(&f.to_string()).ok()),
        _ => None,
    };
    Ok((amount, currency))
}

fn parse_spending_limit(value: Value) -> Result<Option<SpendingLimit>> {
    let max = value
        .get("max")
        .or_else(|| value.get("amount"))
        .or(Some(&value));
    let max = match max {
        Some(v) => v,
        None => return Ok(None),
    };
    let (amount, currency) = parse_amount(max)?;
    let period = value
        .get("period")
        .or_else(|| value.get("window"))
        .and_then(|v| v.as_str())
        .map(String::from);
    match (amount, currency) {
        (Some(a), Some(c)) => Ok(Some(SpendingLimit {
            amount: a,
            currency: c,
            period,
        })),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payment_mandate() -> Value {
        serde_json::json!({
            "@context": ["https://www.w3.org/ns/credentials/v2", "https://ap2-protocol.org/contexts/v1"],
            "type": ["VerifiableCredential", "PaymentMandate"],
            "issuanceDate": "2026-05-01T00:00:00Z",
            "expirationDate": "2026-05-08T00:00:00Z",
            "credentialSubject": {
                "id": "did:example:user-123",
                "agentId": "agent-7",
                "intent": {
                    "merchant": { "id": "api.example.com", "name": "Example API" },
                    "amount": { "value": "0.10", "currency": "USDC" },
                    "network": "eip155:8453",
                    "taskId": "task-42"
                },
                "constraints": {
                    "merchantAllowList": ["api.example.com", "data.vendor.com"],
                    "spendingLimit": {
                        "max": { "value": "5.00", "currency": "USDC" },
                        "period": "PT24H"
                    }
                }
            }
        })
    }

    #[test]
    fn parses_payment_mandate() {
        let attempt = Ap2Adapter
            .parse_value(&ParseContext::default(), &payment_mandate())
            .unwrap();
        assert_eq!(attempt.merchant, "api.example.com");
        assert_eq!(attempt.amount, Decimal::from_str("0.10").unwrap());
        assert_eq!(attempt.currency, "USDC");
        assert_eq!(attempt.network.as_deref(), Some("eip155:8453"));
        assert_eq!(attempt.agent_id, "agent-7");
        assert_eq!(attempt.task_id, "task-42");
        assert!(attempt.mandate.is_some());

        let allow = attempt.metadata.get("ap2_merchant_allow").unwrap();
        assert!(
            allow
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "data.vendor.com")
        );

        let limit = attempt.metadata.get("ap2_spending_limit").unwrap();
        assert_eq!(limit.get("currency").unwrap(), "USDC");
        assert_eq!(limit.get("amount").unwrap(), "5.00");
        assert_eq!(
            attempt.metadata.get("ap2_spec_snapshot").unwrap(),
            AP2_SPEC_SNAPSHOT
        );
    }

    #[test]
    fn parser_context_overrides_mandate_ids() {
        let ctx = ParseContext {
            agent_id: Some("override-agent".into()),
            task_id: Some("override-task".into()),
            wallet: Some("0xwallet".into()),
        };
        let attempt = Ap2Adapter.parse_value(&ctx, &payment_mandate()).unwrap();
        assert_eq!(attempt.agent_id, "override-agent");
        assert_eq!(attempt.task_id, "override-task");
        assert_eq!(attempt.wallet.as_deref(), Some("0xwallet"));
    }

    #[test]
    fn invalid_expiry_errors() {
        let mut m = payment_mandate();
        m["expirationDate"] = serde_json::json!("not-a-date");
        let err = Ap2Adapter
            .parse_value(&ParseContext::default(), &m)
            .unwrap_err();
        assert!(err.to_string().contains("RFC3339"));
    }

    #[test]
    fn flattens_array_spending_limit() {
        let mut m = payment_mandate();
        m["credentialSubject"]["constraints"]["spendingLimit"] =
            serde_json::json!([{ "max": { "value": "2.50", "currency": "USDC" } }]);
        let attempt = Ap2Adapter
            .parse_value(&ParseContext::default(), &m)
            .unwrap();
        let limit = attempt.metadata.get("ap2_spending_limit").unwrap();
        assert_eq!(limit.get("amount").unwrap(), "2.50");
    }
}
