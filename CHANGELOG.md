# Changelog

All notable changes to this project will be documented in this file.

## [0.1.0.0] - 2026-05-10

### Added

- Added the first `agent-payment-sandbox` Rust single-binary release with `sandbox`, `proxy`, and `audit` subcommands.
- Added a deterministic YAML policy engine for buyer-side agent payment controls, including merchant, currency, network, amount, frequency, duplicate-payment, and approval-threshold rules.
- Added fake wallet, fake merchant, state-file persistence, and sandbox settlement simulation so developers can test payment flows without moving real funds.
- Added x402 and AP2 protocol adapters that normalize payment requirements and mandates into a shared `PaymentAttempt` model.
- Added an HTTP/HTTPS proxy that intercepts x402 payment flows, applies policy, fails closed, and records auditable decisions.
- Added SQLite and PostgreSQL audit storage with `audit list`, `audit show`, and `audit export` commands.
- Added a feature-gated live x402 buyer example for USDC on Base via Coinbase CDP-compatible flows.
- Added macOS and Linux CI plus GitHub Release packaging for downloadable binaries.

### Changed

- Updated the product PRD with validation gates, resolved product decisions, single-binary architecture constraints, fail-closed behavior, AP2/x402 scope, and release assumptions.

### Fixed

- Ensured proxy spend counters only update after upstream success, so failed settlement attempts do not inflate budget, duplicate, or frequency checks.
- Ensured malformed `X-PAYMENT` headers produce auditable blocked decisions instead of silently becoming generic gateway failures.
- Ensured x402 challenge cache keys include scheme, method, authority, port, path, and query to prevent cross-wired payment challenges.
- Kept generated MITM CA material out of the repository by defaulting CA storage to the user config directory and ignoring local CA/key patterns.
