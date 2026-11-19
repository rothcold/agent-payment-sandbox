use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use rust_decimal::Decimal;
use uuid::Uuid;

use agent_payment_sandbox::audit::parse_kind;
use agent_payment_sandbox::proxy::{self, ProxyConfig};
use agent_payment_sandbox::sandbox::{
    PayInput, SandboxConfig, SandboxRuntime, print_balances, run_pay,
};
use agent_payment_sandbox::wallet::AssetKey;

#[derive(Parser, Debug)]
#[command(name = "agent-payment-sandbox", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the local sandbox (fake merchants, fake wallet, fake settlement)
    Sandbox(SandboxArgs),
    /// Run the local HTTP/HTTPS proxy with x402 interception + policy enforcement
    Proxy(ProxyArgs),
    /// Inspect the audit log
    Audit(AuditArgs),
}

#[derive(Args, Debug)]
struct AuditArgs {
    /// Audit store URL (sqlite:..., postgres://...)
    #[arg(long, default_value = "sqlite:./agent-payment-audit.db?mode=rwc")]
    audit_db: String,

    #[command(subcommand)]
    action: AuditAction,
}

#[derive(Subcommand, Debug)]
enum AuditAction {
    /// List recent events.
    List {
        #[arg(long, default_value_t = 50)]
        limit: i64,
        /// Filter by event kind (e.g. payment_blocked, settlement_failed).
        #[arg(long)]
        kind: Option<String>,
    },
    /// Show a single event by ID.
    Show { id: String },
    /// Stream every event as JSONL on stdout (newest first).
    Export {
        #[arg(long, default_value_t = 1000)]
        limit: i64,
    },
}

#[derive(Args, Debug)]
struct ProxyArgs {
    /// Listen address. Default 127.0.0.1:7402.
    #[arg(long, default_value = "127.0.0.1:7402")]
    listen: SocketAddr,

    /// Path to the policy YAML.
    #[arg(long)]
    policy: Option<PathBuf>,

    /// Persist wallet/task state across runs.
    #[arg(long)]
    state_file: Option<PathBuf>,

    /// Audit store URL.
    #[arg(long, default_value = "sqlite::memory:")]
    audit_db: String,

    /// Directory for the local CA cert + key.
    /// Default: $XDG_CONFIG_HOME/agent-payment-sandbox/ca (or ~/.config/agent-payment-sandbox/ca).
    /// The CA private key never goes near the repo root.
    #[arg(long)]
    ca_dir: Option<PathBuf>,
}

fn default_ca_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("agent-payment-sandbox").join("ca");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("agent-payment-sandbox")
            .join("ca");
    }
    PathBuf::from("./ca")
}

#[derive(Args, Debug)]
struct SandboxArgs {
    /// Path to the policy YAML. Falls back to AGENT_PAYMENT_POLICY env, then ./agent-payment-policy.yaml.
    #[arg(long)]
    policy: Option<PathBuf>,

    /// Persist wallet/task state across runs.
    #[arg(long)]
    state_file: Option<PathBuf>,

    /// Audit store URL (sqlite::memory:, sqlite:./audit.db, postgres://...).
    #[arg(long, default_value = "sqlite::memory:")]
    audit_db: String,

    #[command(subcommand)]
    action: SandboxAction,
}

#[derive(Subcommand, Debug)]
enum SandboxAction {
    /// Show wallet balances and policy/state file in use.
    Status,
    /// Add available balance for an asset.
    Seed {
        #[arg(long)]
        currency: String,
        #[arg(long)]
        amount: String,
        #[arg(long)]
        network: Option<String>,
    },
    /// Trigger a fake payment flow.
    Pay {
        #[arg(long)]
        merchant: String,
        #[arg(long)]
        amount: Option<String>,
        #[arg(long)]
        currency: Option<String>,
        #[arg(long)]
        network: Option<String>,
        #[arg(long, default_value = "task-default")]
        task_id: String,
        #[arg(long, default_value = "agent-default")]
        agent_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Sandbox(args) => run_sandbox(args).await,
        Command::Proxy(args) => run_proxy(args).await,
        Command::Audit(args) => run_audit(args).await,
    }
}

async fn run_audit(args: AuditArgs) -> Result<()> {
    let store = agent_payment_sandbox::storage::open(&args.audit_db).await?;
    match args.action {
        AuditAction::List { limit, kind } => {
            let want_kind = kind
                .as_deref()
                .map(|k| parse_kind(k).ok_or_else(|| anyhow!("unknown event kind: {k}")))
                .transpose()?;
            // Pull a generous slice so kind-filtering doesn't return empty too easily.
            let pull = if want_kind.is_some() {
                limit.saturating_mul(8)
            } else {
                limit
            };
            let events = store.list_events(pull).await?;
            let mut shown = 0;
            for ev in events {
                if let Some(k) = want_kind {
                    if ev.kind != k {
                        continue;
                    }
                }
                println!(
                    "{} {:>22} {:<28} amount={} {} reason={}",
                    ev.timestamp.format("%Y-%m-%dT%H:%M:%SZ"),
                    ev.kind_str(),
                    ev.payment_attempt.merchant,
                    ev.payment_attempt.amount,
                    ev.payment_attempt.currency,
                    ev.decision
                        .as_ref()
                        .and_then(|d| d.reason().map(String::from))
                        .unwrap_or_default(),
                );
                shown += 1;
                if shown >= limit {
                    break;
                }
            }
        }
        AuditAction::Show { id } => {
            let uuid = Uuid::parse_str(&id).context("not a UUID")?;
            match store.get_event(uuid).await? {
                Some(ev) => println!("{}", serde_json::to_string_pretty(&ev)?),
                None => return Err(anyhow!("event {id} not found")),
            }
        }
        AuditAction::Export { limit } => {
            let events = store.list_events(limit).await?;
            for ev in events {
                println!("{}", serde_json::to_string(&ev)?);
            }
        }
    }
    Ok(())
}

async fn run_proxy(args: ProxyArgs) -> Result<()> {
    let cfg = ProxyConfig {
        addr: args.listen,
        sandbox: SandboxConfig {
            policy_path: args.policy,
            state_path: args.state_file,
            audit_url: args.audit_db,
        },
        ca_dir: args.ca_dir.unwrap_or_else(default_ca_dir),
    };
    proxy::run(cfg).await
}

async fn run_sandbox(args: SandboxArgs) -> Result<()> {
    let cfg = SandboxConfig {
        policy_path: args.policy,
        state_path: args.state_file,
        audit_url: args.audit_db,
    };
    let mut rt = SandboxRuntime::open(&cfg).await?;

    match args.action {
        SandboxAction::Status => {
            rt.show_status();
            print_balances(&rt.wallet);
        }
        SandboxAction::Seed {
            currency,
            amount,
            network,
        } => {
            let amt = Decimal::from_str(&amount)?;
            let asset = AssetKey::new(currency, network);
            rt.wallet.seed(&asset, amt);
            rt.persist()?;
            println!("Seeded {} {}.", amt, asset.as_string_key());
        }
        SandboxAction::Pay {
            merchant,
            amount,
            currency,
            network,
            task_id,
            agent_id,
        } => {
            let input = PayInput {
                merchant,
                amount: amount.as_deref().map(Decimal::from_str).transpose()?,
                currency,
                network,
                task_id,
                agent_id,
            };
            let outcome = run_pay(&mut rt, input).await?;
            let decision = outcome.final_decision.kind();
            let reason = outcome
                .final_decision
                .reason()
                .map(|r| format!(" — {r}"))
                .unwrap_or_default();
            println!(
                "Payment attempt {} → {decision}{reason}",
                outcome.attempt.id
            );
            print_balances(&rt.wallet);
        }
    }
    Ok(())
}
