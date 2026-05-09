use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use crate::audit::AuditEvent;

#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "sqlite")]
pub mod sqlite;

#[async_trait]
pub trait AuditStore: Send + Sync {
    async fn write_event(&self, event: &AuditEvent) -> Result<()>;
    async fn get_event(&self, id: Uuid) -> Result<Option<AuditEvent>>;
    async fn list_events(&self, limit: i64) -> Result<Vec<AuditEvent>>;
    async fn migrate(&self) -> Result<()>;
}

pub enum StoreUrl {
    Sqlite(String),
    Postgres(String),
}

impl StoreUrl {
    pub fn parse(url: &str) -> anyhow::Result<Self> {
        if url.starts_with("sqlite:") {
            Ok(Self::Sqlite(url.to_string()))
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            Ok(Self::Postgres(url.to_string()))
        } else {
            anyhow::bail!("unsupported audit store URL: {url}")
        }
    }
}

pub async fn open(url: &str) -> Result<Box<dyn AuditStore>> {
    match StoreUrl::parse(url)? {
        #[cfg(feature = "sqlite")]
        StoreUrl::Sqlite(u) => {
            let store = sqlite::SqliteStore::connect(&u).await?;
            store.migrate().await?;
            Ok(Box::new(store))
        }
        #[cfg(not(feature = "sqlite"))]
        StoreUrl::Sqlite(_) => anyhow::bail!("sqlite feature not enabled"),
        #[cfg(feature = "postgres")]
        StoreUrl::Postgres(u) => {
            let store = postgres::PostgresStore::connect(&u).await?;
            store.migrate().await?;
            Ok(Box::new(store))
        }
        #[cfg(not(feature = "postgres"))]
        StoreUrl::Postgres(_) => anyhow::bail!("postgres feature not enabled"),
    }
}
