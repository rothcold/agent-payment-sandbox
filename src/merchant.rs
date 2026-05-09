use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeMerchant {
    pub host: String,
    pub resource: String,
    pub price: Decimal,
    pub currency: String,
    pub network: Option<String>,
}

impl FakeMerchant {
    pub fn fake_x402_payment_requirement(&self) -> serde_json::Value {
        serde_json::json!({
            "scheme": "exact",
            "network": self.network,
            "asset": self.currency,
            "maxAmountRequired": self.price.to_string(),
            "resource": format!("https://{}{}", self.host, self.resource),
            "description": format!("Sandbox fake payment for {}", self.host),
            "mimeType": "application/json"
        })
    }
}

pub fn default_fixtures() -> Vec<FakeMerchant> {
    use std::str::FromStr;
    vec![
        FakeMerchant {
            host: "api.example.com".to_string(),
            resource: "/v1/data".to_string(),
            price: Decimal::from_str("0.10").unwrap(),
            currency: "USDC".to_string(),
            network: Some("eip155:8453".to_string()),
        },
        FakeMerchant {
            host: "data.vendor.com".to_string(),
            resource: "/v1/feed".to_string(),
            price: Decimal::from_str("0.25").unwrap(),
            currency: "USDC".to_string(),
            network: Some("eip155:8453".to_string()),
        },
    ]
}

pub fn find_fixture(host: &str) -> Option<FakeMerchant> {
    default_fixtures().into_iter().find(|m| m.host == host)
}
