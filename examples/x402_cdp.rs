//! End-to-end x402 buyer client targeting an x402-protected URL.
//!
//! Pays USDC on Base. The target API server is responsible for verifying and
//! settling via Coinbase CDP (or any compatible facilitator); this example
//! signs the EIP-3009 `transferWithAuthorization` payload and replays the
//! request with the `X-PAYMENT` header.
//!
//! Build:
//!   cargo build --release --example x402_cdp --features example-cdp
//!
//! Run:
//!   X402_TARGET_URL=https://your-x402-api/example \
//!   X402_PRIVATE_KEY=0x<hex private key for a wallet funded with USDC on Base> \
//!   ./target/release/examples/x402_cdp
//!
//! Optional env vars:
//!   X402_AGENT_ID, X402_TASK_ID — labels recorded in the parsed PaymentAttempt.
//!   X402_VERBOSE=1 — print the full challenge and signed payload before sending.

use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256, FixedBytes, U256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolStruct, eip712_domain, sol};
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rand::RngCore;

use agent_payment_sandbox::adapter::ParseContext;
use agent_payment_sandbox::adapter::ProtocolAdapter;
use agent_payment_sandbox::adapter::x402::{Requirement, X402Adapter};

sol! {
    struct TransferWithAuthorization {
        address from;
        address to;
        uint256 value;
        uint256 validAfter;
        uint256 validBefore;
        bytes32 nonce;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let target_url = env::var("X402_TARGET_URL").context("set X402_TARGET_URL")?;
    let pk_hex = env::var("X402_PRIVATE_KEY").context("set X402_PRIVATE_KEY")?;
    let verbose = env::var("X402_VERBOSE").map(|v| v == "1").unwrap_or(false);

    let signer: PrivateKeySigner = pk_hex.trim_start_matches("0x").parse()?;
    let from = signer.address();
    println!("Wallet:        {from}");
    println!("Target URL:    {target_url}");

    let client = reqwest::Client::builder().build()?;

    // 1. Initial request → expect 402 with payment challenge.
    let resp = client.get(&target_url).send().await?;
    let status = resp.status();
    if status.as_u16() != 402 {
        anyhow::bail!("expected 402 from {target_url}, got {status}");
    }
    let challenge: serde_json::Value = resp.json().await?;
    if verbose {
        println!("Challenge:\n{}", serde_json::to_string_pretty(&challenge)?);
    }

    // 2. Parse via the in-tree adapter.
    let ctx = ParseContext {
        agent_id: env::var("X402_AGENT_ID").ok(),
        task_id: env::var("X402_TASK_ID").ok(),
        wallet: Some(format!("{from:?}")),
    };
    let attempt = X402Adapter.parse_value(&ctx, &challenge)?;
    let req: Requirement = serde_json::from_value(
        attempt
            .payment_requirement
            .clone()
            .ok_or_else(|| anyhow!("adapter returned no payment_requirement"))?,
    )?;

    println!(
        "Will pay:      {} {} (atomic {}) on {} → {}",
        attempt.amount, attempt.currency, req.max_amount_required, req.network, req.pay_to
    );

    // 3. Build EIP-3009 authorization.
    let to: Address = req.pay_to.parse().context("invalid payTo address")?;
    let asset: Address = req.asset.parse().context("invalid asset address")?;
    let value: U256 = req
        .max_amount_required
        .parse()
        .context("invalid maxAmountRequired")?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let valid_before = now + req.max_timeout_seconds.unwrap_or(300);

    let mut nonce_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = FixedBytes::<32>::from(nonce_bytes);

    let typed = TransferWithAuthorization {
        from,
        to,
        value,
        validAfter: U256::ZERO,
        validBefore: U256::from(valid_before),
        nonce,
    };

    // 4. EIP-712 sign.
    let chain_id = chain_id_for(&req.network)?;
    let token_name = req
        .extra
        .as_ref()
        .and_then(|e| e.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("USD Coin")
        .to_string();
    let token_version = req
        .extra
        .as_ref()
        .and_then(|e| e.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("2")
        .to_string();

    let domain = eip712_domain! {
        name: token_name,
        version: token_version,
        chain_id: chain_id,
        verifying_contract: asset,
    };
    let signing_hash: B256 = typed.eip712_signing_hash(&domain);
    let sig = signer.sign_hash(&signing_hash).await?;
    let sig_bytes: [u8; 65] = sig.as_bytes();

    // 5. X-PAYMENT header (base64 of the JSON payload).
    let payload = serde_json::json!({
        "x402Version": 1,
        "scheme": req.scheme,
        "network": req.network,
        "payload": {
            "signature": format!("0x{}", hex::encode(sig_bytes)),
            "authorization": {
                "from": format!("{from:?}"),
                "to": format!("{to:?}"),
                "value": req.max_amount_required,
                "validAfter": "0",
                "validBefore": valid_before.to_string(),
                "nonce": format!("0x{}", hex::encode(nonce_bytes)),
            }
        }
    });
    if verbose {
        println!(
            "X-PAYMENT JSON:\n{}",
            serde_json::to_string_pretty(&payload)?
        );
    }
    let header = B64.encode(payload.to_string());

    // 6. Replay original request with the signed payment header.
    let resp = client
        .get(&target_url)
        .header("X-PAYMENT", header)
        .send()
        .await?;
    let status = resp.status();
    println!("Final status:  {status}");
    if let Some(pr) = resp.headers().get("X-PAYMENT-RESPONSE") {
        if let Ok(val) = pr.to_str() {
            if let Ok(decoded) = B64.decode(val) {
                if let Ok(s) = std::str::from_utf8(&decoded) {
                    println!("X-PAYMENT-RESPONSE (decoded): {s}");
                }
            }
        }
    }
    let body = resp.text().await.unwrap_or_default();
    println!("Body:\n{body}");
    Ok(())
}

fn chain_id_for(network: &str) -> Result<u64> {
    Ok(match network {
        "base" => 8453,
        "base-sepolia" => 84532,
        "ethereum" | "mainnet" => 1,
        "polygon" => 137,
        "arbitrum" => 42161,
        "optimism" => 10,
        other => anyhow::bail!("unsupported network for EIP-712 chain id: {other}"),
    })
}
