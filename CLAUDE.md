# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`agent-payment-sandbox` — a Rust single-binary developer tool that sits between an agent runtime and AP2 / x402 agent-payment protocols. It runs a local sandbox, an HTTP/HTTPS proxy with x402 interception, a deterministic policy engine, and an audit log. Source of truth for product scope: `docs/agent-payment-policy-gateway-prd.md`. Living plan: `TODOS.md`.

The single-binary constraint is a product requirement, not an implementation detail — every subcommand must keep compiling into one executable. Heavy optional integrations (currently the live x402 buyer in `examples/x402_cdp.rs`) live behind cargo features so default builds stay lean.

## Build, test, run

```
cargo build                                   # default: sqlite feature
cargo build --no-default-features --features postgres
cargo build --features "sqlite postgres"
cargo build --release
cargo test  --features "sqlite postgres"      # 34 lib + 4 proxy integration + 1 storage roundtrip
cargo test  --test proxy_integration          # just the integration suite
cargo test  --features sqlite policy::tests   # filter to one module
cargo test  --features sqlite -- --nocapture frequency_limit_counts_recent_payments
cargo build --example x402_cdp --features example-cdp   # heavy: pulls alloy + reqwest
```

CI runs `ubuntu-latest` + `macos-latest` matrices in `.github/workflows/ci.yml`. Windows is intentionally deferred.

Run the binary:

```
./target/release/agent-payment-sandbox sandbox --policy ./policy.yaml --state-file ./state.json status
./target/release/agent-payment-sandbox sandbox ... seed --currency USDC --network eip155:8453 --amount 5.00
./target/release/agent-payment-sandbox sandbox ... pay  --merchant api.example.com --amount 0.10 --currency USDC --network eip155:8453 --task-id t1
./target/release/agent-payment-sandbox proxy   --listen 127.0.0.1:7402 --policy ./policy.yaml
./target/release/agent-payment-sandbox audit   --audit-db sqlite:./audit.db list --kind settlement_failed
```

The proxy auto-generates a local CA at `$XDG_CONFIG_HOME/agent-payment-sandbox/ca` (or `~/.config/...`) on first run; the path and `SSL_CERT_FILE=` hint print to stdout. Never commit anything from a `ca/` directory — `.gitignore` blocks the common patterns but check before staging.

## Architecture — the things that need reading multiple files

### One process, three entry points sharing one runtime

`src/main.rs` exposes `sandbox`, `proxy`, and `audit` subcommands. They share:

- The canonical `PaymentAttempt` (`src/model.rs`) and `PolicyDecision` (`src/policy.rs`) types
- The deterministic policy evaluator (`policy::evaluate(policy, attempt, wallet, now)`)
- The same audit-store trait (`storage::AuditStore`) backed by SQLite or PostgreSQL through `sqlx` (`src/storage/{mod,sqlite,postgres,schema.sql}.rs`) — the schema is one shared `payment_events` table; do not let the two backends diverge
- The same `WalletState` (`src/wallet.rs`) with `balances`, `task_spend`, `reservations`, and an unbounded `history: Vec<PaymentRecord>` used by frequency / per-day / duplicate rules
- Shared protocol adapters (`src/adapter/{mod,x402,ap2}.rs`)

Sandbox vs proxy diverge only in *how* they obtain a `PaymentAttempt`:

- **sandbox** (`src/sandbox.rs`): synthesizes from CLI args + fake merchant fixtures (`src/merchant.rs`). `require_approval` blocks on a synchronous CLI prompt (`src/approval.rs`). Always settles successfully (no real money), so `settlement_succeeded` and `wallet.record_payment` happen inline.
- **proxy** (`src/proxy/mod.rs` + `src/proxy/ca.rs`): MITMs HTTP and HTTPS via `hyper` + `tokio-rustls` + `rcgen`. CONNECT for HTTPS performs a TLS handshake using a leaf cert minted by the local CA (cached per hostname; SNI never includes port). `require_approval` returns a synthetic 402 with the reason — there is no CLI prompt in proxy mode (PRD §10).

### Proxy invariants — keep these intact

These came from a code review (see `done/20260505-170013-claude-code.md`); regressions are easy to introduce.

1. **Spend is recorded only on real settlement.** `evaluate_payment_request` returns `PreForwardOutcome::Allow(attempt)`. `handle_proxied` forwards upstream first; only on `2xx` does it write `settlement_succeeded` + `wallet.record_payment` + `persist`. Non-2xx → `settlement_failed`, no spend recorded. Network error → `settlement_failed` (and `gateway_unavailable` if no Allow attempt was held). Putting `record_payment` inside the Allow branch would let upstream failures inflate the per-task / per-day / frequency / duplicate counters. Tested by `allow_then_upstream_500_does_not_credit_spend`.
2. **Every malformed X-PAYMENT writes an audit event.** `reject_malformed_payment` covers utf-8 / base64 / JSON / missing `payload.authorization` / adapter-normalize failures and produces `payment_blocked` with structured `detail`. Do not silently bubble these into the outer fail-closed 502. Tested by `malformed_x_payment_audits_block`.
3. **Cache key includes scheme + method + authority (with port) + path + query.** `format!("{} {}://{}{}", method, scheme, authority, path_q)`. Same host on different ports must not collide; same path with different methods must not collide. Tested by `cache_key_separates_custom_ports` and unit tests in `proxy::unit`.
4. **Fail-closed is the only gateway mode.** No fail-open flag exists or should be added (PRD §10.5). Internal errors return 502 + `gateway_unavailable` audit event.

### Policy engine — replayability

`policy::evaluate` takes `now: DateTime<Utc>` as a parameter (never calls `Utc::now()` itself) and reads only from `WalletState` (a deterministic snapshot). The 9 rules from PRD §7.2 evaluate in fixed order with documented precedence (Block > RequireApproval > Allow). Network names get normalized to CAIP-2 on both sides of the comparison so policy YAML can use either friendly names (`base`) or CAIP-2 (`eip155:8453`). Currency cross-FX is not performed — amounts compared numerically across `USD`/`USDC`/etc. is a v1 simplification. The `evaluator_is_deterministic` test guards this; do not introduce HashMap iteration into the rule path.

### AP2 spec discipline

The AP2 spec is tracked continuously, not pinned. `src/adapter/ap2.rs` carries an `AP2_SPEC_SNAPSHOT` constant (currently `"2026-05-02"`); `docs/ap2-adapter-changelog.md` lists the assumed field paths and known gaps. **When you change anything about AP2 parsing, bump the snapshot constant and add a dated entry to the changelog in the same commit.** Audit events for AP2 attempts include the snapshot in metadata so old records stay re-parseable.

### Examples are gated

`examples/x402_cdp.rs` is the live x402 buyer (Coinbase CDP, Base mainnet, USDC) — it pulls `alloy-*` + `rand` and only compiles with `--features example-cdp`. Default `cargo build` and CI matrix do not touch it. It has not been exercised end-to-end against a live target — verify with a tiny amount before relying on it.

## Conventions worth knowing

- All amounts use `rust_decimal::Decimal` end-to-end. Never introduce `f64` for money; the `serde-with-str` feature is what keeps round-trips lossless through JSON / SQL.
- Subcommands use `clap` derive. Adding a new subcommand: add a `#[derive(Subcommand)]` variant in `main.rs` + an async handler.
- `Mutex` choice matters: `tokio::sync::Mutex` for `WalletState` (held across `await`s when persisting); `std::sync::Mutex` for the `challenge_cache` (short critical sections, simpler). Mixing them up panics — `blocking_lock` from inside async will crash the runtime, learned the hard way.
- The PRD is the ground truth for product decisions; `TODOS.md` tracks execution. When PRD and code disagree, treat the PRD as load-bearing and update code (or open a question to the user before deviating).
- `done/` work logs are append-only and per-task; each meaningful change writes a fresh `YYYYMMDD-HHMMSS-claude-code.md` describing intent, files, commands, result, and notes.
