#![cfg(feature = "sqlite")]

use agent_payment_sandbox::audit::{AuditEvent, AuditEventKind};
use agent_payment_sandbox::model::{PaymentAttempt, Protocol};
use agent_payment_sandbox::policy::PolicyDecision;
use agent_payment_sandbox::storage;
use rust_decimal::Decimal;
use std::str::FromStr;

#[tokio::test]
async fn write_and_read_event_roundtrip() {
    let store = storage::open("sqlite::memory:").await.expect("open store");

    let mut attempt = PaymentAttempt::new(
        Protocol::X402,
        "agent-test",
        "task-1",
        "api.example.com",
        Decimal::from_str("0.10").unwrap(),
        "USDC",
    );
    attempt.network = Some("eip155:8453".to_string());

    let mut event = AuditEvent::new(AuditEventKind::PolicyEvaluated, attempt);
    event.policy_version = Some("v1".to_string());
    event.decision = Some(PolicyDecision::Allow);

    store.write_event(&event).await.expect("write");

    let fetched = store
        .get_event(event.id)
        .await
        .expect("get")
        .expect("event present");

    assert_eq!(fetched.id, event.id);
    assert_eq!(fetched.kind, AuditEventKind::PolicyEvaluated);
    assert_eq!(
        fetched.payment_attempt.amount,
        Decimal::from_str("0.10").unwrap()
    );
    assert_eq!(
        fetched.payment_attempt.network.as_deref(),
        Some("eip155:8453")
    );
    assert_eq!(fetched.decision, Some(PolicyDecision::Allow));

    let listed = store.list_events(10).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, event.id);
}
