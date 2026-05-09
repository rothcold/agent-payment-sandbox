use anyhow::{Context, Result};
use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;
use uuid::Uuid;

use crate::audit::AuditEvent;
use crate::storage::AuditStore;

const SCHEMA: &str = include_str!("schema.sql");

pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    pub async fn connect(url: &str) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(url)
            .with_context(|| format!("invalid sqlite url: {url}"))?
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .context("connect sqlite")?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl AuditStore for SqliteStore {
    async fn migrate(&self) -> Result<()> {
        for stmt in SCHEMA.split(';') {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            sqlx::query(trimmed)
                .execute(&self.pool)
                .await
                .with_context(|| format!("apply migration: {trimmed}"))?;
        }
        Ok(())
    }

    async fn write_event(&self, event: &AuditEvent) -> Result<()> {
        let payment_attempt = serde_json::to_string(&event.payment_attempt)?;
        let decision = event
            .decision
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let detail = event
            .detail
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let kind = serde_json::to_value(event.kind)?
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        sqlx::query(
            "INSERT INTO payment_events
             (id, timestamp, kind, payment_attempt, policy_version, decision,
              ap2_spec_version, raw_payload_ref, detail)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(event.id.to_string())
        .bind(event.timestamp.to_rfc3339())
        .bind(kind)
        .bind(payment_attempt)
        .bind(&event.policy_version)
        .bind(decision)
        .bind(&event.ap2_spec_version)
        .bind(&event.raw_payload_ref)
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_event(&self, id: Uuid) -> Result<Option<AuditEvent>> {
        let row = sqlx::query(
            "SELECT id, timestamp, kind, payment_attempt, policy_version, decision,
                    ap2_spec_version, raw_payload_ref, detail
             FROM payment_events WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_event).transpose()
    }

    async fn list_events(&self, limit: i64) -> Result<Vec<AuditEvent>> {
        let rows = sqlx::query(
            "SELECT id, timestamp, kind, payment_attempt, policy_version, decision,
                    ap2_spec_version, raw_payload_ref, detail
             FROM payment_events ORDER BY timestamp DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_event).collect()
    }
}

fn row_to_event(row: sqlx::sqlite::SqliteRow) -> Result<AuditEvent> {
    let id: String = row.try_get("id")?;
    let timestamp: String = row.try_get("timestamp")?;
    let kind: String = row.try_get("kind")?;
    let payment_attempt: String = row.try_get("payment_attempt")?;
    let policy_version: Option<String> = row.try_get("policy_version")?;
    let decision: Option<String> = row.try_get("decision")?;
    let ap2_spec_version: Option<String> = row.try_get("ap2_spec_version")?;
    let raw_payload_ref: Option<String> = row.try_get("raw_payload_ref")?;
    let detail: Option<String> = row.try_get("detail")?;

    Ok(AuditEvent {
        id: Uuid::parse_str(&id)?,
        timestamp: chrono::DateTime::parse_from_rfc3339(&timestamp)?.with_timezone(&chrono::Utc),
        kind: serde_json::from_value(serde_json::Value::String(kind))?,
        payment_attempt: serde_json::from_str(&payment_attempt)?,
        policy_version,
        decision: decision.map(|s| serde_json::from_str(&s)).transpose()?,
        ap2_spec_version,
        raw_payload_ref,
        detail: detail.map(|s| serde_json::from_str(&s)).transpose()?,
    })
}
