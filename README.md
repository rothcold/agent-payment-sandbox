# agent-payment-sandbox

A protocol-agnostic policy gateway for autonomous-agent payments. It sits between an
agent runtime and the emerging agent-payment protocols (AP2 and x402), letting
developers **simulate**, **approve**, **block**, and **audit** payment attempts before
real money moves.

This is a **developer tool**, not a production spend-management product. AP2 / x402
production usage is still small; the gateway is positioned around safe demos, test
harnesses, and protocol debugging.

Source of product truth: [`docs/agent-payment-policy-gateway-prd.md`](docs/agent-payment-policy-gateway-prd.md).
Living implementation plan: [`TODOS.md`](TODOS.md).

## What you get in one binary

```
agent-payment-sandbox
├── sandbox    — fake merchants, fake wallet, deterministic policy engine, CLI approval prompt
├── proxy      — local HTTP/HTTPS MITM proxy, x402 interception, fail-closed gateway
└── audit      — query the immutable event log (sqlite or postgres)
```

The single-binary constraint is a product requirement: every subcommand must keep
compiling into one executable. Heavy optional crates (currently the live Coinbase CDP
buyer in `examples/x402_cdp.rs`) sit behind cargo features so default builds stay lean.

## Install

Requires Rust (stable). macOS and Linux are supported; Windows is intentionally
deferred.

```
git clone <this repo> && cd agent-payment-sandbox
cargo build --release
# Binary at ./target/release/agent-payment-sandbox
```

Feature matrix:

```
cargo build                                        # default: sqlite
cargo build --no-default-features --features postgres
cargo build --features "sqlite postgres"           # both backends
cargo build --release --example x402_cdp --features example-cdp   # heavy
```

CI runs `ubuntu-latest` + `macos-latest` matrices in
[`.github/workflows/ci.yml`](.github/workflows/ci.yml).

## Quick start — sandbox

```yaml
# policy.yaml
agent: my-agent
mode: sandbox
version: v1
limits:
  max_per_payment: { amount: "0.25", currency: USD }
  max_per_task:    { amount: "5.00", currency: USD }
  max_payments_per_minute: 10
  duplicate_window_seconds: 30
allow:
  merchants:  [api.example.com, data.vendor.com]
  currencies: [USDC]
  networks:   [base]
block:
  merchants:  [evil.com]
approval:
  required_above: { amount: "1.00", currency: USD }
```

```bash
APS=./target/release/agent-payment-sandbox
DB="sqlite:./audit.db?mode=rwc"

$APS sandbox --policy policy.yaml --state-file state.json --audit-db "$DB" \
  seed --currency USDC --network eip155:8453 --amount 5.00

$APS sandbox --policy policy.yaml --state-file state.json --audit-db "$DB" \
  pay --merchant api.example.com --amount 0.10 --currency USDC --network eip155:8453 --task-id t1
# → allow

$APS sandbox --policy policy.yaml --state-file state.json --audit-db "$DB" \
  pay --merchant evil.com --amount 0.05 --currency USDC --network eip155:8453 --task-id t1
# → block — merchant 'evil.com' is in blocklist

$APS sandbox --policy policy.yaml --state-file state.json --audit-db "$DB" \
  pay --merchant api.example.com --amount 1.50 --currency USDC --network eip155:8453 --task-id t1
# Approve? [y/N]:   ← synchronous CLI prompt
```

## Quick start — proxy

```bash
$APS proxy --listen 127.0.0.1:7402 \
  --policy policy.yaml --state-file state.json --audit-db "$DB"
# Prints the local CA path on first run, e.g.
#   CA cert path: /home/you/.config/agent-payment-sandbox/ca/root.pem
#   Trust this CA in your client (e.g. SSL_CERT_FILE=...)
```

Point any agent at `HTTP_PROXY=http://127.0.0.1:7402 HTTPS_PROXY=http://127.0.0.1:7402`.
The proxy:

- Forwards plain requests, intercepts upstream `402 Payment Required`, caches the
  challenge, and writes a `payment_attempt_created` audit event.
- On a follow-up request with `X-PAYMENT`, decodes the header, runs the policy, and
  either forwards (allow) or returns a synthetic `402` with the decision reason
  (block / approval-required / simulate).
- For HTTPS, performs full TLS MITM — generates a leaf cert per host signed by the
  local CA, decrypts traffic, applies the same intercept logic, then re-encrypts.
- **Fail-closed**: any internal error returns `502` and writes a
  `gateway_unavailable` event. There is no fail-open mode.
- **Spend is recorded only on real settlement**: a `payment_allowed` decision lets
  the request proceed, but `wallet.record_payment` runs only after the upstream
  returns 2xx (and writes `settlement_succeeded`). Upstream failures write
  `settlement_failed` and never inflate the per-task / per-day / frequency counters.

## Audit

```bash
$APS audit --audit-db "$DB" list --limit 20
$APS audit --audit-db "$DB" list --kind payment_blocked
$APS audit --audit-db "$DB" show <uuid>
$APS audit --audit-db "$DB" export --limit 1000 > audit.jsonl
```

Same store layer powers SQLite (`sqlite:./audit.db?mode=rwc`) and PostgreSQL
(`postgres://user:pass@host/db`). One shared schema (`src/storage/schema.sql`).

## Live x402 buyer (optional)

`examples/x402_cdp.rs` signs an EIP-3009 `transferWithAuthorization` and replays the
request with `X-PAYMENT` against an x402-protected endpoint. Defaults to USDC on Base
via Coinbase CDP.

```bash
cargo build --release --example x402_cdp --features example-cdp
X402_TARGET_URL=https://your-x402-api/example \
X402_PRIVATE_KEY=0x<funded_key> \
./target/release/examples/x402_cdp
```

This pulls `alloy-*` and `reqwest`; it is feature-gated so default builds stay slim.
**Run with a tiny amount first** — the example has not been exercised end-to-end
against a live target inside this repository.

## Architecture in 60 seconds

The three subcommands share one runtime: the canonical `PaymentAttempt`
(`src/model.rs`), the deterministic policy evaluator (`src/policy.rs`), the
`AuditStore` trait (`src/storage/`), the `WalletState` (`src/wallet.rs`), and the
adapters (`src/adapter/{x402,ap2}.rs`).

Sandbox vs proxy diverge only in **how** they obtain a `PaymentAttempt`:

- **sandbox** synthesizes from CLI args + fake fixtures and always settles
  successfully.
- **proxy** parses real x402 traffic, evaluates policy on it, and only credits the
  wallet after the upstream actually returns success.

The policy evaluator is deterministic by construction: `evaluate(policy, attempt,
wallet, now)` takes the clock as a parameter, reads only from a `WalletState`
snapshot, and runs nine rules in a fixed order with `Block > RequireApproval > Allow`
precedence. Same inputs → same decision, always (this is what makes audit replay
useful).

The AP2 spec is **not pinned**; `src/adapter/ap2.rs` carries an
`AP2_SPEC_SNAPSHOT` constant and `docs/ap2-adapter-changelog.md` records the assumed
field paths. Audit events for AP2 attempts include the snapshot in metadata so old
records remain re-parseable as the spec evolves.

For deeper architecture notes (proxy invariants, mutex choices, replayability
constraints), see [`CLAUDE.md`](CLAUDE.md).

## Tests

```bash
cargo test --features "sqlite postgres"        # 34 lib + 4 proxy integration + 1 storage
cargo test --test proxy_integration             # just the integration suite
```

The proxy integration suite covers four invariants you must not regress:

1. HTTP `402` caches the challenge; `X-PAYMENT` replay reaches upstream `200`.
2. Malformed `X-PAYMENT` (bad base64 / JSON / structure) writes a
   `payment_blocked` audit event and returns a synthetic `402` — never silently
   `502`.
3. Policy `Allow` followed by upstream `500` writes `settlement_failed` and does
   **not** credit the wallet.
4. Cache key includes scheme + method + authority (with port) + path + query, so
   two upstreams on different ports never cross-wire challenges.

## Status & non-goals

This is v0. It is intentionally **not** any of:

- A payment processor or wallet
- A generic LLM-token cost dashboard
- A drop-in replacement for AP2 or x402
- A full enterprise spend-management suite

PRD §16 lists the open questions still on the table. PRD §13 lists the structural
risks (market timing, protocol churn, integration friction, value perception,
single-binary scope creep) and the mitigations the design bakes in.

## License

Apache-2.0.
