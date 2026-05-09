use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use rust_decimal::Decimal;

use crate::approval::{ApprovalUi, stdin_approval};
use crate::audit::{AuditEvent, AuditEventKind};
use crate::merchant::{self, FakeMerchant};
use crate::model::{PaymentAttempt, Protocol};
use crate::policy::{self, Policy, PolicyDecision};
use crate::state::StateFile;
use crate::storage::AuditStore;
use crate::wallet::{AssetKey, PaymentRecord, WalletState};

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub policy_path: Option<PathBuf>,
    pub state_path: Option<PathBuf>,
    pub audit_url: String,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            policy_path: None,
            state_path: None,
            audit_url: "sqlite::memory:".to_string(),
        }
    }
}

pub struct SandboxRuntime {
    pub policy: Policy,
    pub policy_source: Option<PathBuf>,
    pub state_file: Option<StateFile>,
    pub wallet: WalletState,
    pub store: Box<dyn AuditStore>,
}

impl SandboxRuntime {
    pub async fn open(cfg: &SandboxConfig) -> Result<Self> {
        let resolved = policy::load_policy(cfg.policy_path.as_deref())?;
        let state_file = cfg.state_path.clone().map(StateFile::new);
        let wallet = match &state_file {
            Some(f) => f.load_or_default()?,
            None => WalletState::new(),
        };
        let store = crate::storage::open(&cfg.audit_url).await?;
        Ok(Self {
            policy: resolved.policy,
            policy_source: resolved.source,
            state_file,
            wallet,
            store,
        })
    }

    pub fn persist(&self) -> Result<()> {
        if let Some(file) = &self.state_file {
            file.save(&self.wallet)?;
        }
        Ok(())
    }

    pub async fn record(&self, event: &AuditEvent) -> Result<()> {
        self.store.write_event(event).await
    }

    pub fn show_status(&self) {
        if let Some(src) = &self.policy_source {
            println!("Policy source: {}", src.display());
        } else {
            println!("Policy source: (none — using defaults)");
        }
        if let Some(f) = &self.state_file {
            println!("State file: {}", f.path().display());
        } else {
            println!("State file: (in-memory only)");
        }
        println!("Reservations tracked: {}", self.wallet.reservations.len());
    }
}

#[derive(Debug, Clone)]
pub struct PayInput {
    pub merchant: String,
    pub amount: Option<Decimal>,
    pub currency: Option<String>,
    pub network: Option<String>,
    pub task_id: String,
    pub agent_id: String,
}

pub struct PaymentOutcome {
    pub attempt: PaymentAttempt,
    pub initial_decision: PolicyDecision,
    pub final_decision: PolicyDecision,
}

pub async fn run_pay(rt: &mut SandboxRuntime, input: PayInput) -> Result<PaymentOutcome> {
    let mut ui = stdin_approval();
    run_pay_with_ui(rt, input, &mut ui).await
}

pub async fn run_pay_with_ui<U: ApprovalUi>(
    rt: &mut SandboxRuntime,
    input: PayInput,
    ui: &mut U,
) -> Result<PaymentOutcome> {
    let fixture: Option<FakeMerchant> = merchant::find_fixture(&input.merchant);
    let amount = input
        .amount
        .or_else(|| fixture.as_ref().map(|f| f.price))
        .context("no amount provided and no fake merchant fixture matched")?;
    let currency = input
        .currency
        .clone()
        .or_else(|| fixture.as_ref().map(|f| f.currency.clone()))
        .unwrap_or_else(|| "USDC".to_string());
    let network = input
        .network
        .clone()
        .or_else(|| fixture.as_ref().and_then(|f| f.network.clone()));

    let mut attempt = PaymentAttempt::new(
        Protocol::X402,
        input.agent_id,
        input.task_id,
        input.merchant.clone(),
        amount,
        currency.clone(),
    );
    attempt.network = network.clone();
    if let Some(f) = &fixture {
        attempt.payment_requirement = Some(f.fake_x402_payment_requirement());
        attempt.resource = Some(format!("https://{}{}", f.host, f.resource));
    }

    rt.record(&AuditEvent::new(
        AuditEventKind::PaymentAttemptCreated,
        attempt.clone(),
    ))
    .await?;

    let now = Utc::now();
    let decision = policy::evaluate(&rt.policy, &attempt, &rt.wallet, now);
    let mut policy_event = AuditEvent::new(AuditEventKind::PolicyEvaluated, attempt.clone());
    policy_event.policy_version = rt.policy.version.clone();
    policy_event.decision = Some(decision.clone());
    rt.record(&policy_event).await?;

    let final_decision = match &decision {
        PolicyDecision::RequireApproval { .. } => {
            let mut ev = AuditEvent::new(AuditEventKind::ApprovalRequired, attempt.clone());
            ev.decision = Some(decision.clone());
            rt.record(&ev).await?;
            if ui.confirm(&attempt, &decision) {
                PolicyDecision::Allow
            } else {
                PolicyDecision::Block {
                    reason: "operator rejected".to_string(),
                }
            }
        }
        other => other.clone(),
    };

    match &final_decision {
        PolicyDecision::Allow => {
            let asset = AssetKey::new(currency.clone(), network.clone());
            let reservation = rt
                .wallet
                .reserve(attempt.id, &attempt.task_id, asset, amount)?;
            let mut ev = AuditEvent::new(AuditEventKind::PaymentAllowed, attempt.clone());
            ev.detail = Some(serde_json::json!({"reservation_id": reservation.id}));
            rt.record(&ev).await?;

            // Sandbox always succeeds in v1. Real settlement happens in proxy mode + Phase 5/6.
            rt.wallet.settle(reservation.id)?;
            rt.wallet.record_payment(PaymentRecord {
                timestamp: now,
                agent_id: attempt.agent_id.clone(),
                task_id: attempt.task_id.clone(),
                merchant: attempt.merchant.clone(),
                resource: attempt.resource.clone(),
                amount: attempt.amount,
                currency: attempt.currency.clone(),
            });
            let ev = AuditEvent::new(AuditEventKind::SettlementSucceeded, attempt.clone());
            rt.record(&ev).await?;
        }
        PolicyDecision::Block { .. } => {
            let mut ev = AuditEvent::new(AuditEventKind::PaymentBlocked, attempt.clone());
            ev.decision = Some(final_decision.clone());
            rt.record(&ev).await?;
        }
        PolicyDecision::SimulateOnly { .. } => {
            let mut ev = AuditEvent::new(AuditEventKind::PaymentSimulated, attempt.clone());
            ev.decision = Some(final_decision.clone());
            rt.record(&ev).await?;
        }
        PolicyDecision::RequireApproval { .. } => unreachable!("approval already resolved"),
    }

    rt.persist()?;
    Ok(PaymentOutcome {
        attempt,
        initial_decision: decision,
        final_decision,
    })
}

pub fn print_balances(wallet: &WalletState) {
    if wallet.balances.is_empty() {
        println!("(no balances — use `seed` to fund the wallet)");
        return;
    }
    println!("Balances:");
    for (key, bal) in &wallet.balances {
        println!(
            "  {key}: available={} reserved={}",
            bal.available, bal.reserved
        );
    }
    if !wallet.task_spend.is_empty() {
        println!("Task spend totals:");
        for (task, amt) in &wallet.task_spend {
            println!("  {task}: {amt}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PolicyDecision;
    use std::str::FromStr;

    struct FixedUi(bool);
    impl ApprovalUi for FixedUi {
        fn confirm(&mut self, _: &PaymentAttempt, _: &PolicyDecision) -> bool {
            self.0
        }
    }

    fn input(task: &str, merchant: &str) -> PayInput {
        PayInput {
            merchant: merchant.to_string(),
            amount: None,
            currency: None,
            network: None,
            task_id: task.to_string(),
            agent_id: "agent-test".to_string(),
        }
    }

    #[tokio::test]
    async fn pay_uses_fake_merchant_fixture_and_settles() {
        let mut rt = SandboxRuntime::open(&SandboxConfig::default())
            .await
            .unwrap();
        rt.wallet.seed(
            &AssetKey::new("USDC", Some("eip155:8453".to_string())),
            Decimal::from_str("1.00").unwrap(),
        );
        let mut ui = FixedUi(true);
        let outcome = run_pay_with_ui(&mut rt, input("t1", "api.example.com"), &mut ui)
            .await
            .unwrap();
        let attempt = outcome.attempt;
        assert_eq!(outcome.final_decision, PolicyDecision::Allow);
        assert_eq!(attempt.amount, Decimal::from_str("0.10").unwrap());
        assert_eq!(attempt.currency, "USDC");
        assert_eq!(attempt.network.as_deref(), Some("eip155:8453"));
        let bal = rt
            .wallet
            .balance(&AssetKey::new("USDC", Some("eip155:8453".to_string())));
        assert_eq!(bal.available, Decimal::from_str("0.90").unwrap());
        assert_eq!(
            rt.wallet.task_total("t1"),
            Decimal::from_str("0.10").unwrap()
        );

        let events = rt.store.list_events(20).await.unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.kind == AuditEventKind::SettlementSucceeded)
        );
    }
}
