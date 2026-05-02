# PRD: Agent Payment Policy Gateway

- Status: Draft
- Date: 2026-05-02
- Owner: TBD
- Primary wedge: AP2/x402 developer sandbox + buyer-side payment guard
- Distribution: Rust single binary

## 1. Summary

Agent Payment Policy Gateway is a protocol-agnostic control layer for autonomous agent payments.

The product sits between an agent runtime and emerging agent payment protocols such as AP2 and x402. It lets developers simulate, approve, block, and audit payment attempts before real money moves.

The first version is not a generic API cost dashboard. It is a developer tool for teams building agent-native payment flows who need a safe way to test and enforce payment policy.

The product must always be able to compile into one distributable binary. The first release should be a Rust CLI that can run a local sandbox and an HTTP proxy from the same executable.

## 2. Problem

Agent-native payments are early. x402 production usage is still small, and many teams that say "agent payment" today actually mean Stripe API calls, prepaid API keys, or human-approved execution.

But the protocol direction is clear:

- AP2 focuses on payment authorization and verifiable mandates.
- x402 focuses on HTTP-native payment requests and settlement.
- Future protocols will likely define different authorization, payment, wallet, or settlement primitives.

The missing layer is policy.

Even if a user authorized an agent to spend, the agent still needs runtime checks:

- Is this payment inside the user's budget?
- Is this merchant allowed?
- Is the agent repeating the same payment in a loop?
- Is this payment consistent with the original task?
- Should this payment be simulated, paused for approval, blocked, or executed?
- Can the developer replay and audit the full decision later?

Without this layer, agent payment builders either ship unsafe demos or hardcode one-off guardrails into each integration.

## 3. Target Users

### Primary User

Developer building AP2/x402-enabled agent payment flows.

Examples:

- A developer building an agent that pays x402 APIs per request.
- A wallet or agent framework engineer adding AP2 mandate support.
- A protocol ecosystem builder creating reference demos and sample apps.

### Secondary User

Buyer-side operator who lets an agent spend from a wallet or delegated payment authority.

Examples:

- Indie developer testing autonomous API purchases.
- Small team letting an internal agent buy data, services, or API access.
- Early enterprise platform team experimenting with agent commerce.

## 4. Product Positioning

Working name:

**Agent Payment Policy Gateway**

Short description:

**A sandbox and policy gateway for AP2/x402 agent payments. Test payment flows, enforce spend rules, and audit every agent payment decision.**

What it is:

- A developer sandbox for agent payment protocols.
- A buyer-side payment guard.
- A protocol adapter layer for AP2, x402, and future agent payment protocols.
- An audit trail for mandates, payment attempts, decisions, and settlement results.

What it is not:

- Not a generic LLM token dashboard.
- Not a payment processor.
- Not a wallet.
- Not a replacement for AP2 or x402.
- Not a full enterprise spend-management suite in v1.

## 5. Core Use Cases

### Use Case 1: Simulate an x402 Payment Flow

A developer points an agent at an x402-protected API. The gateway intercepts the `402 Payment Required` challenge, parses the payment requirement, applies local policy, and returns a simulated allow/block decision.

Success means the developer can test payment behavior without moving real funds.

### Use Case 2: Enforce Buyer-Side Budget Policy

A developer configures a budget for an agent task:

- Max per payment: `$0.25`
- Max per task: `$5.00`
- Max payments per minute: `10`
- Allowed merchants: `api.example.com`, `data.vendor.com`

The agent attempts payment. The gateway checks the policy and either allows, blocks, or requires approval.

Success means a runaway agent cannot spend past the configured policy.

### Use Case 3: Validate AP2 Mandates

A developer sends an AP2 mandate into the gateway. The gateway extracts the user intent, merchant constraints, spending limits, expiry, and payment context, then evaluates whether the attempted payment matches the mandate.

Success means the developer can see why a payment is authorized or rejected.

### Use Case 4: Replay and Audit a Payment Decision

A developer opens the audit log for a blocked payment and sees:

- Agent identity
- Task context
- Protocol adapter used
- Mandate or payment requirement
- Policy evaluated
- Decision result
- Settlement status if executed

Success means the developer can debug payment behavior without reconstructing logs from multiple systems.

## 6. MVP Scope

### 6.1 Protocol Adapter Interface

Create a protocol abstraction that normalizes payment attempts across x402, AP2, and future protocols.

Canonical model:

- `protocol`: `x402 | ap2 | custom`
- `agent_id`
- `task_id`
- `merchant`
- `amount`
- `currency`
- `network`
- `resource`
- `mandate`
- `payment_requirement`
- `wallet`
- `metadata`

Adapter responsibilities:

- Parse protocol-specific inputs.
- Normalize payment intent into the canonical model.
- Return protocol-specific execution or simulation output.

Initial adapters:

- x402 adapter: prioritize compatibility with the official x402 documentation and reference concepts.
- AP2 adapter: parse the full AP2 mandate structure as closely as practical against the official spec.
- Future adapters: implement the same normalized interface without changing the policy engine.

### 6.2 Policy Engine

Support simple deterministic policy rules:

- Max amount per payment
- Max total amount per task
- Max total amount per agent per day
- Merchant allowlist / blocklist
- Currency allowlist
- Network allowlist
- Payment frequency limit
- Duplicate payment detection by merchant/resource/amount/time window
- Require approval above threshold

Policy decisions:

- `allow`
- `block`
- `require_approval`
- `simulate_only`

### 6.3 Sandbox Mode

Sandbox mode is the default v1 experience.

Capabilities:

- Run x402/AP2-like payment flows without real settlement.
- Generate fake merchants and payment requirements.
- Simulate fake wallet balances.
- Simulate reserved funds.
- Simulate settlement states: `pending`, `succeeded`, `failed`.
- Replay sample scenarios.
- Show policy decision traces.
- Export audit logs as JSON.

### 6.4 Buyer-Side Runtime Guard

Provide a local HTTP proxy that developers can put in front of agent payment calls.

Integration options:

- HTTP proxy for x402-style requests.
- CLI command for local sandbox runs.

v1 should prioritize CLI sandbox and local HTTP proxy. SDKs can come later, but they must not become required for the core product to work.

### 6.5 Audit Log

Every payment attempt must produce an immutable event record:

- `payment_attempt.created`
- `policy.evaluated`
- `payment.allowed`
- `payment.blocked`
- `approval.required`
- `payment.simulated`
- `settlement.succeeded`
- `settlement.failed`

Each event should include:

- Timestamp
- Agent/task identifiers
- Protocol adapter
- Normalized payment intent
- Policy version
- Decision reason
- Raw protocol payload reference

## 7. Non-Goals for MVP

- Real card issuing
- Full wallet custody
- Enterprise SSO/RBAC
- Complex fraud modeling
- Merchant-side risk scoring
- Full AP2 certification or compliance tooling
- Deep accounting integrations
- Production-grade multi-region settlement infrastructure

## 8. User Flow

1. Developer downloads one binary.
2. Developer defines a policy file.
3. Developer runs `gateway sandbox` to test fake AP2/x402 payment flows.
4. Developer runs `gateway proxy` when they want agent HTTP payment traffic to pass through the local guard.
5. Agent attempts an AP2/x402 payment flow.
6. Gateway parses the protocol payload into a normalized payment attempt.
7. Gateway evaluates policy.
8. Gateway returns allow/block/approval/simulate decision.
9. Gateway records an audit event.
10. Developer reviews the trace in the CLI.

## 9. Architecture Decisions

### 9.1 Single Binary Constraint

The main implementation language is Rust.

The project must always be able to compile into a single binary that includes:

- CLI sandbox
- Local HTTP proxy
- Policy engine
- Protocol adapters
- Fake wallet and settlement simulator
- Local audit log and replay commands

This constraint is part of the product, not an implementation detail. Early users should be able to download one executable and run the full developer experience locally.

### 9.2 Interface Decisions

First interface:

- CLI sandbox
- Local HTTP proxy

Deferred interfaces:

- SDKs
- Hosted dashboard
- SaaS control plane

### 9.3 Protocol Decisions

x402 target:

- Prioritize the official x402 documentation and reference concepts.
- Avoid coupling v1 to one chain, facilitator, or vendor SDK unless required for a sample.

AP2 target:

- Parse the full AP2 mandate structure as closely as practical.
- Store raw AP2 payloads in the audit log so parser behavior can be revalidated as the spec changes.

### 9.4 Policy Format

Policy files use YAML in v1.

Reason:

- Easy for developers to hand-write.
- Familiar to infra/devtool users.
- Good fit for examples, tutorials, and local sandbox runs.

### 9.5 Sandbox Simulation

Sandbox includes fake wallet and settlement simulation.

Required states:

- Wallet balance available
- Funds reserved
- Settlement pending
- Settlement succeeded
- Settlement failed
- Payment blocked before reserve
- Payment blocked after mandate parse
- Payment blocked after policy evaluation

## 10. Example Policy

```yaml
agent: coding-agent-local
mode: sandbox
limits:
  max_per_payment:
    amount: 0.25
    currency: USD
  max_per_task:
    amount: 5.00
    currency: USD
  max_payments_per_minute: 10
allow:
  merchants:
    - api.example.com
    - data.vendor.com
  currencies:
    - USD
    - USDC
  networks:
    - base
block:
  merchants:
    - unknown
approval:
  required_above:
    amount: 1.00
    currency: USD
```

## 11. Success Metrics

### Product Validation Metrics

- 10 developers run the sandbox end-to-end.
- 5 developers run a real or near-real agent payment flow through the local proxy.
- 3 developers provide real payment-flow traces from AP2/x402 experiments.
- 1 developer uses it in a public demo, tutorial, or hackathon project.

### Product Quality Metrics

- A new developer can run the first sandbox payment flow in under 10 minutes.
- Every payment attempt has an explainable decision trace.
- Policy decisions are deterministic and replayable.
- Protocol adapters can be added without changing the policy engine.
- The project can compile into one binary at all times.

## 12. Risks

### Market Timing Risk

AP2/x402 production usage may stay tiny for longer than expected. If that happens, a production gateway will be too early.

Mitigation:

- Lead with sandbox and developer tooling.
- Support demos, hackathons, and protocol builders first.
- Keep the adapter layer broad enough to support future protocols.

### Protocol Churn Risk

AP2, x402, and adjacent agent payment standards may change quickly.

Mitigation:

- Isolate protocol-specific parsing in adapters.
- Keep policy evaluation on a normalized internal model.
- Store raw protocol payloads for replay and migration.

### Integration Friction Risk

Developers may not route payment traffic through a gateway unless setup is trivial.

Mitigation:

- Offer a local proxy first.
- Provide copy-paste example apps.
- Make sandbox useful before production integration.
- Keep distribution as a single binary so setup does not require a package runtime.

### Value Perception Risk

Developers may see this as unnecessary until real money is moving.

Mitigation:

- Position around safe demos, test harnesses, and protocol debugging.
- Show concrete failure cases: looped payment, wrong merchant, expired mandate, over-budget request.

### Single Binary Scope Risk

The single-binary constraint may make it tempting to delay UI, hosted collaboration, and external service integrations.

Mitigation:

- Treat the binary as the core product and use files, local logs, and CLI replay first.
- Add optional hosted services later without making them required for local operation.
- Keep the Rust crate boundaries clean so future SDKs and services can reuse the core logic.

## 13. Recommended MVP Build Order

1. Define the normalized payment attempt model.
2. Create Rust project skeleton that builds one binary.
3. Build local sandbox CLI with fake merchant, fake agent, fake wallet, and fake settlement.
4. Build x402 adapter against official x402 docs/reference concepts.
5. Build AP2 mandate parser against the official spec.
6. Add local HTTP proxy for x402-style requests.
7. Build deterministic YAML policy engine.
8. Build audit log and replay commands.
9. Add packaged binary release flow.
10. Add minimal web dashboard only if it can be served from the same binary or kept clearly optional.

## 14. Resolved Product Decisions

- First interface: CLI sandbox plus local HTTP proxy.
- x402 compatibility target: official x402 documentation and reference concepts.
- AP2 parsing scope: full AP2 mandate structure as closely as practical.
- Policy format: YAML.
- Sandbox simulation: fake wallet balance, reserved funds, and settlement states.
- Distribution: downloadable binary.
- Main implementation language: Rust.
- Hard architecture constraint: project must always compile into one single binary.

## 15. Remaining Open Questions

- What should the binary be named?
- Which operating systems are required for the first release: macOS, Linux, Windows?
- Should the first audit log storage be JSONL, SQLite, or both?
- Should the local proxy support HTTPS interception in v1, or only explicit HTTP proxying?
- What exact AP2 spec version should v1 pin to?
- Should v1 include one real x402 sample integration, or only mock/reference-compatible flows?
- Should the fake wallet model support multiple currencies and networks in v1?
- What is the first release channel: GitHub Releases, Homebrew, cargo install, or all of these?

## 16. Assignment

Before implementation, interview or DM 10 AP2/x402 builders and ask for one concrete trace:

> "Show me the payment flow you are trying to build. Where would you want a sandbox, policy check, or audit trace?"

Do not ask whether they like the idea. Ask for their actual flow.

If fewer than 3 people can show a real or near-real payment flow, keep the product as developer tooling and avoid positioning it as production spend control.

## 17. References

- AP2 official docs: https://ap2-protocol.org/
- AP2 and x402 docs: https://ap2-protocol.org/topics/ap2-and-x402/
- x402 official docs: https://docs.x402.org/
- x402 website: https://www.x402.org/
- Base x402 payment flow docs: https://docs.base.org/ai-agents/core-concepts/payments-and-transactions
