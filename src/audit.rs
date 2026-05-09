use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::PaymentAttempt;
use crate::policy::PolicyDecision;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventKind {
    PaymentAttemptCreated,
    PolicyEvaluated,
    PaymentAllowed,
    PaymentBlocked,
    ApprovalRequired,
    PaymentSimulated,
    SettlementSucceeded,
    SettlementFailed,
    GatewayUnavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub kind: AuditEventKind,
    pub payment_attempt: PaymentAttempt,
    pub policy_version: Option<String>,
    pub decision: Option<PolicyDecision>,
    pub ap2_spec_version: Option<String>,
    pub raw_payload_ref: Option<String>,
    pub detail: Option<serde_json::Value>,
}

impl AuditEvent {
    pub fn new(kind: AuditEventKind, payment_attempt: PaymentAttempt) -> Self {
        Self {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            kind,
            payment_attempt,
            policy_version: None,
            decision: None,
            ap2_spec_version: None,
            raw_payload_ref: None,
            detail: None,
        }
    }

    pub fn kind_str(&self) -> &'static str {
        match self.kind {
            AuditEventKind::PaymentAttemptCreated => "payment_attempt_created",
            AuditEventKind::PolicyEvaluated => "policy_evaluated",
            AuditEventKind::PaymentAllowed => "payment_allowed",
            AuditEventKind::PaymentBlocked => "payment_blocked",
            AuditEventKind::ApprovalRequired => "approval_required",
            AuditEventKind::PaymentSimulated => "payment_simulated",
            AuditEventKind::SettlementSucceeded => "settlement_succeeded",
            AuditEventKind::SettlementFailed => "settlement_failed",
            AuditEventKind::GatewayUnavailable => "gateway_unavailable",
        }
    }
}

pub fn parse_kind(raw: &str) -> Option<AuditEventKind> {
    match raw {
        "payment_attempt_created" => Some(AuditEventKind::PaymentAttemptCreated),
        "policy_evaluated" => Some(AuditEventKind::PolicyEvaluated),
        "payment_allowed" => Some(AuditEventKind::PaymentAllowed),
        "payment_blocked" => Some(AuditEventKind::PaymentBlocked),
        "approval_required" => Some(AuditEventKind::ApprovalRequired),
        "payment_simulated" => Some(AuditEventKind::PaymentSimulated),
        "settlement_succeeded" => Some(AuditEventKind::SettlementSucceeded),
        "settlement_failed" => Some(AuditEventKind::SettlementFailed),
        "gateway_unavailable" => Some(AuditEventKind::GatewayUnavailable),
        _ => None,
    }
}
