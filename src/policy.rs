use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::model::PaymentAttempt;
use crate::wallet::WalletState;

const POLICY_ENV: &str = "AGENT_PAYMENT_POLICY";
const DEFAULT_POLICY_FILE: &str = "agent-payment-policy.yaml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Block { reason: String },
    RequireApproval { reason: String },
    SimulateOnly { reason: String },
}

impl PolicyDecision {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Block { .. } => "block",
            Self::RequireApproval { .. } => "require_approval",
            Self::SimulateOnly { .. } => "simulate_only",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Allow => None,
            Self::Block { reason }
            | Self::RequireApproval { reason }
            | Self::SimulateOnly { reason } => Some(reason),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    pub agent: Option<String>,
    pub mode: Option<String>,
    pub limits: Option<Limits>,
    pub allow: Option<AllowList>,
    pub block: Option<BlockList>,
    pub approval: Option<Approval>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Limits {
    pub max_per_payment: Option<MoneyLimit>,
    pub max_per_task: Option<MoneyLimit>,
    pub max_per_agent_per_day: Option<MoneyLimit>,
    pub max_payments_per_minute: Option<u32>,
    pub duplicate_window_seconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoneyLimit {
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    pub currency: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AllowList {
    #[serde(default)]
    pub merchants: Vec<String>,
    #[serde(default)]
    pub currencies: Vec<String>,
    #[serde(default)]
    pub networks: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockList {
    #[serde(default)]
    pub merchants: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Approval {
    pub required_above: Option<MoneyLimit>,
}

pub struct ResolvedPolicy {
    pub policy: Policy,
    pub source: Option<PathBuf>,
}

pub fn load_policy(cli_path: Option<&Path>) -> Result<ResolvedPolicy> {
    if let Some(p) = cli_path {
        return load_from_path(p).map(|policy| ResolvedPolicy {
            policy,
            source: Some(p.to_path_buf()),
        });
    }
    if let Ok(env_path) = std::env::var(POLICY_ENV) {
        let p = PathBuf::from(env_path);
        return load_from_path(&p).map(|policy| ResolvedPolicy {
            policy,
            source: Some(p),
        });
    }
    let default = PathBuf::from(DEFAULT_POLICY_FILE);
    if default.exists() {
        return load_from_path(&default).map(|policy| ResolvedPolicy {
            policy,
            source: Some(default),
        });
    }
    Ok(ResolvedPolicy {
        policy: Policy::default(),
        source: None,
    })
}

fn load_from_path(path: &Path) -> Result<Policy> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read policy file {}", path.display()))?;
    let policy: Policy = serde_yaml::from_str(&raw)
        .with_context(|| format!("parse policy file {}", path.display()))?;
    Ok(policy)
}

/// Deterministic policy evaluator. Given identical inputs (policy, attempt, wallet
/// history, now), this returns the same decision — required for replay/audit.
///
/// Rule order (first match wins, Block > RequireApproval > Allow):
/// 1. block.merchants
/// 2. allow.merchants (when set, attempt must match)
/// 3. allow.currencies
/// 4. allow.networks
/// 5. limits.max_per_payment
/// 6. limits.max_per_task
/// 7. limits.max_per_agent_per_day
/// 8. limits.max_payments_per_minute
/// 9. limits.duplicate_window_seconds
/// 10. approval.required_above
pub fn evaluate(
    policy: &Policy,
    attempt: &PaymentAttempt,
    wallet: &WalletState,
    now: DateTime<Utc>,
) -> PolicyDecision {
    // 1. Block list (merchants).
    if let Some(b) = &policy.block {
        if b.merchants
            .iter()
            .any(|m| m.eq_ignore_ascii_case(&attempt.merchant))
        {
            return block(format!("merchant '{}' is in blocklist", attempt.merchant));
        }
    }

    // 2-4. Allow list checks (only enforced if the corresponding list is non-empty).
    if let Some(a) = &policy.allow {
        if !a.merchants.is_empty()
            && !a
                .merchants
                .iter()
                .any(|m| m.eq_ignore_ascii_case(&attempt.merchant))
        {
            return block(format!("merchant '{}' not in allowlist", attempt.merchant));
        }
        if !a.currencies.is_empty()
            && !a
                .currencies
                .iter()
                .any(|c| c.eq_ignore_ascii_case(&attempt.currency))
        {
            return block(format!("currency '{}' not in allowlist", attempt.currency));
        }
        if !a.networks.is_empty() {
            let attempt_net = attempt.network.as_deref().unwrap_or("");
            if !a.networks.iter().any(|n| network_matches(n, attempt_net)) {
                return block(format!("network '{attempt_net}' not in allowlist"));
            }
        }
    }

    // 5-9. Limits.
    if let Some(l) = &policy.limits {
        if let Some(mp) = &l.max_per_payment {
            if attempt.amount > mp.amount {
                return block(format!(
                    "amount {} exceeds max_per_payment {} {}",
                    attempt.amount, mp.amount, mp.currency
                ));
            }
        }
        if let Some(mt) = &l.max_per_task {
            let total = wallet.task_total(&attempt.task_id) + attempt.amount;
            if total > mt.amount {
                return block(format!(
                    "task '{}' total {} would exceed max_per_task {} {}",
                    attempt.task_id, total, mt.amount, mt.currency
                ));
            }
        }
        if let Some(md) = &l.max_per_agent_per_day {
            let day_start = now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .expect("00:00:00 valid")
                .and_utc();
            let agent_total =
                wallet.agent_total_since(&attempt.agent_id, day_start) + attempt.amount;
            if agent_total > md.amount {
                return block(format!(
                    "agent '{}' daily total {} would exceed max_per_agent_per_day {} {}",
                    attempt.agent_id, agent_total, md.amount, md.currency
                ));
            }
        }
        if let Some(freq) = l.max_payments_per_minute {
            let since = now - Duration::seconds(60);
            let count = wallet.count_since(since) + 1;
            if count > freq as usize {
                return block(format!(
                    "frequency {count} would exceed max_payments_per_minute {freq}"
                ));
            }
        }
        if let Some(window) = l.duplicate_window_seconds {
            let since = now - Duration::seconds(window as i64);
            if wallet.has_duplicate_within(
                &attempt.merchant,
                attempt.resource.as_deref(),
                attempt.amount,
                &attempt.currency,
                since,
            ) {
                return block(format!(
                    "duplicate payment within {window}s window: merchant={} amount={} {}",
                    attempt.merchant, attempt.amount, attempt.currency
                ));
            }
        }
    }

    // 10. Approval threshold.
    if let Some(a) = &policy.approval {
        if let Some(threshold) = &a.required_above {
            if attempt.amount > threshold.amount {
                return PolicyDecision::RequireApproval {
                    reason: format!(
                        "amount {} exceeds approval threshold {} {}",
                        attempt.amount, threshold.amount, threshold.currency
                    ),
                };
            }
        }
    }

    PolicyDecision::Allow
}

fn block(reason: String) -> PolicyDecision {
    PolicyDecision::Block { reason }
}

fn network_matches(rule: &str, attempt: &str) -> bool {
    if rule.eq_ignore_ascii_case(attempt) {
        return true;
    }
    normalize_network(rule).eq_ignore_ascii_case(&normalize_network(attempt))
}

fn normalize_network(s: &str) -> String {
    match s.to_lowercase().as_str() {
        "base" => "eip155:8453".into(),
        "base-sepolia" => "eip155:84532".into(),
        "ethereum" | "mainnet" => "eip155:1".into(),
        "polygon" => "eip155:137".into(),
        "arbitrum" => "eip155:42161".into(),
        "optimism" => "eip155:10".into(),
        "avalanche" => "eip155:43114".into(),
        "avalanche-fuji" => "eip155:43113".into(),
        other => other.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use tempfile::tempdir;

    #[test]
    fn parses_example_policy() {
        let yaml = r#"
agent: coding-agent-local
mode: sandbox
limits:
  max_per_payment:
    amount: "0.25"
    currency: USD
  max_per_task:
    amount: "5.00"
    currency: USD
  max_payments_per_minute: 10
allow:
  merchants:
    - api.example.com
  currencies:
    - USDC
  networks:
    - base
block:
  merchants:
    - unknown
approval:
  required_above:
    amount: "1.00"
    currency: USD
"#;
        let p: Policy = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(p.agent.as_deref(), Some("coding-agent-local"));
        assert_eq!(
            p.limits
                .as_ref()
                .unwrap()
                .max_per_payment
                .as_ref()
                .unwrap()
                .amount,
            Decimal::from_str("0.25").unwrap()
        );
        assert_eq!(p.limits.as_ref().unwrap().max_payments_per_minute, Some(10));
    }

    #[test]
    fn loads_from_explicit_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.yaml");
        std::fs::write(&path, "agent: a1\n").unwrap();
        let resolved = load_policy(Some(&path)).unwrap();
        assert_eq!(resolved.policy.agent.as_deref(), Some("a1"));
        assert_eq!(resolved.source, Some(path));
    }

    // ----- Evaluator tests -----

    use crate::model::{PaymentAttempt, Protocol};
    use crate::wallet::{PaymentRecord, WalletState};
    use chrono::TimeZone;

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 2, 12, 0, 0).unwrap()
    }

    fn attempt(
        amount: &str,
        merchant: &str,
        currency: &str,
        network: Option<&str>,
    ) -> PaymentAttempt {
        let mut a = PaymentAttempt::new(
            Protocol::X402,
            "agent-1",
            "task-1",
            merchant,
            Decimal::from_str(amount).unwrap(),
            currency,
        );
        a.network = network.map(String::from);
        a
    }

    fn record(
        when: DateTime<Utc>,
        agent: &str,
        task: &str,
        merchant: &str,
        amount: &str,
        currency: &str,
        resource: Option<&str>,
    ) -> PaymentRecord {
        PaymentRecord {
            timestamp: when,
            agent_id: agent.into(),
            task_id: task.into(),
            merchant: merchant.into(),
            resource: resource.map(String::from),
            amount: Decimal::from_str(amount).unwrap(),
            currency: currency.into(),
        }
    }

    #[test]
    fn empty_policy_allows() {
        let p = Policy::default();
        let w = WalletState::new();
        assert_eq!(
            evaluate(
                &p,
                &attempt("1.00", "api.example.com", "USDC", Some("eip155:8453")),
                &w,
                fixed_now()
            ),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn block_merchant_blocks() {
        let mut p = Policy::default();
        p.block = Some(BlockList {
            merchants: vec!["evil.com".into()],
        });
        let w = WalletState::new();
        let d = evaluate(
            &p,
            &attempt("1.00", "evil.com", "USDC", None),
            &w,
            fixed_now(),
        );
        assert_eq!(d.kind(), "block");
        assert!(d.reason().unwrap().contains("blocklist"));
    }

    #[test]
    fn allow_merchant_filters() {
        let mut p = Policy::default();
        p.allow = Some(AllowList {
            merchants: vec!["api.example.com".into()],
            ..Default::default()
        });
        let w = WalletState::new();
        assert_eq!(
            evaluate(
                &p,
                &attempt("1.00", "api.example.com", "USDC", None),
                &w,
                fixed_now()
            ),
            PolicyDecision::Allow
        );
        assert_eq!(
            evaluate(
                &p,
                &attempt("1.00", "other.com", "USDC", None),
                &w,
                fixed_now()
            )
            .kind(),
            "block"
        );
    }

    #[test]
    fn allow_currency_filters() {
        let mut p = Policy::default();
        p.allow = Some(AllowList {
            currencies: vec!["USDC".into()],
            ..Default::default()
        });
        let w = WalletState::new();
        assert_eq!(
            evaluate(&p, &attempt("1.00", "x.com", "USDC", None), &w, fixed_now()),
            PolicyDecision::Allow
        );
        assert_eq!(
            evaluate(&p, &attempt("1.00", "x.com", "EUR", None), &w, fixed_now()).kind(),
            "block"
        );
    }

    #[test]
    fn allow_network_normalizes_friendly_to_caip2() {
        let mut p = Policy::default();
        p.allow = Some(AllowList {
            networks: vec!["base".into()],
            ..Default::default()
        });
        let w = WalletState::new();
        assert_eq!(
            evaluate(
                &p,
                &attempt("1.00", "x.com", "USDC", Some("eip155:8453")),
                &w,
                fixed_now()
            ),
            PolicyDecision::Allow
        );
        assert_eq!(
            evaluate(
                &p,
                &attempt("1.00", "x.com", "USDC", Some("eip155:1")),
                &w,
                fixed_now()
            )
            .kind(),
            "block"
        );
    }

    #[test]
    fn max_per_payment_blocks_above() {
        let mut p = Policy::default();
        p.limits = Some(Limits {
            max_per_payment: Some(MoneyLimit {
                amount: Decimal::from_str("0.25").unwrap(),
                currency: "USD".into(),
            }),
            ..Default::default()
        });
        let w = WalletState::new();
        assert_eq!(
            evaluate(&p, &attempt("0.25", "x.com", "USDC", None), &w, fixed_now()),
            PolicyDecision::Allow
        );
        assert_eq!(
            evaluate(&p, &attempt("0.26", "x.com", "USDC", None), &w, fixed_now()).kind(),
            "block"
        );
    }

    #[test]
    fn max_per_task_uses_wallet_task_total() {
        let mut p = Policy::default();
        p.limits = Some(Limits {
            max_per_task: Some(MoneyLimit {
                amount: Decimal::from_str("1.00").unwrap(),
                currency: "USD".into(),
            }),
            ..Default::default()
        });
        let mut w = WalletState::new();
        w.task_spend
            .insert("task-1".into(), Decimal::from_str("0.80").unwrap());
        assert_eq!(
            evaluate(&p, &attempt("0.20", "x.com", "USDC", None), &w, fixed_now()),
            PolicyDecision::Allow
        );
        assert_eq!(
            evaluate(&p, &attempt("0.21", "x.com", "USDC", None), &w, fixed_now()).kind(),
            "block"
        );
    }

    #[test]
    fn max_per_agent_per_day_sums_history_for_today() {
        let mut p = Policy::default();
        p.limits = Some(Limits {
            max_per_agent_per_day: Some(MoneyLimit {
                amount: Decimal::from_str("1.00").unwrap(),
                currency: "USD".into(),
            }),
            ..Default::default()
        });
        let mut w = WalletState::new();
        // Earlier today (UTC) — counts.
        w.history.push(record(
            fixed_now() - Duration::hours(2),
            "agent-1",
            "task-x",
            "y.com",
            "0.60",
            "USDC",
            None,
        ));
        // Yesterday — does not count.
        w.history.push(record(
            fixed_now() - Duration::hours(36),
            "agent-1",
            "task-x",
            "y.com",
            "5.00",
            "USDC",
            None,
        ));
        assert_eq!(
            evaluate(&p, &attempt("0.40", "x.com", "USDC", None), &w, fixed_now()),
            PolicyDecision::Allow
        );
        assert_eq!(
            evaluate(&p, &attempt("0.41", "x.com", "USDC", None), &w, fixed_now()).kind(),
            "block"
        );
    }

    #[test]
    fn frequency_limit_counts_recent_payments() {
        let mut p = Policy::default();
        p.limits = Some(Limits {
            max_payments_per_minute: Some(2),
            ..Default::default()
        });
        let mut w = WalletState::new();
        w.history.push(record(
            fixed_now() - Duration::seconds(10),
            "agent-1",
            "task-1",
            "x.com",
            "0.10",
            "USDC",
            None,
        ));
        // 1 prior + 1 attempt = 2 → allowed.
        assert_eq!(
            evaluate(&p, &attempt("0.10", "x.com", "USDC", None), &w, fixed_now()),
            PolicyDecision::Allow
        );
        w.history.push(record(
            fixed_now() - Duration::seconds(5),
            "agent-1",
            "task-1",
            "x.com",
            "0.10",
            "USDC",
            None,
        ));
        // 2 prior + 1 attempt = 3 → blocked.
        assert_eq!(
            evaluate(&p, &attempt("0.10", "x.com", "USDC", None), &w, fixed_now()).kind(),
            "block"
        );
    }

    #[test]
    fn duplicate_window_blocks_same_resource_amount() {
        let mut p = Policy::default();
        p.limits = Some(Limits {
            duplicate_window_seconds: Some(60),
            ..Default::default()
        });
        let mut w = WalletState::new();
        w.history.push(record(
            fixed_now() - Duration::seconds(30),
            "agent-1",
            "task-1",
            "api.example.com",
            "0.10",
            "USDC",
            Some("/v1/data"),
        ));
        let mut a = attempt("0.10", "api.example.com", "USDC", None);
        a.resource = Some("/v1/data".into());
        let d = evaluate(&p, &a, &w, fixed_now());
        assert_eq!(d.kind(), "block");
        assert!(d.reason().unwrap().contains("duplicate"));
    }

    #[test]
    fn approval_threshold_returns_require_approval() {
        let mut p = Policy::default();
        p.approval = Some(Approval {
            required_above: Some(MoneyLimit {
                amount: Decimal::from_str("1.00").unwrap(),
                currency: "USD".into(),
            }),
        });
        let w = WalletState::new();
        assert_eq!(
            evaluate(&p, &attempt("1.00", "x.com", "USDC", None), &w, fixed_now()),
            PolicyDecision::Allow
        );
        let d = evaluate(&p, &attempt("1.01", "x.com", "USDC", None), &w, fixed_now());
        assert_eq!(d.kind(), "require_approval");
    }

    #[test]
    fn block_beats_approval() {
        let mut p = Policy::default();
        p.block = Some(BlockList {
            merchants: vec!["x.com".into()],
        });
        p.approval = Some(Approval {
            required_above: Some(MoneyLimit {
                amount: Decimal::from_str("0.10").unwrap(),
                currency: "USD".into(),
            }),
        });
        let w = WalletState::new();
        let d = evaluate(&p, &attempt("1.00", "x.com", "USDC", None), &w, fixed_now());
        assert_eq!(d.kind(), "block");
    }

    #[test]
    fn evaluator_is_deterministic() {
        let mut p = Policy::default();
        p.limits = Some(Limits {
            max_per_payment: Some(MoneyLimit {
                amount: Decimal::from_str("0.25").unwrap(),
                currency: "USD".into(),
            }),
            ..Default::default()
        });
        let w = WalletState::new();
        let a = attempt("0.30", "x.com", "USDC", None);
        let now = fixed_now();
        let d1 = evaluate(&p, &a, &w, now);
        let d2 = evaluate(&p, &a, &w, now);
        let d3 = evaluate(&p, &a, &w, now);
        assert_eq!(d1, d2);
        assert_eq!(d2, d3);
    }
}
