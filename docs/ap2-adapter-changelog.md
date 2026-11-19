# AP2 Adapter Changelog

The AP2 spec is tracked continuously (no version pin). This file records what
changed in our parser whenever the upstream spec moves and we have to adjust
field paths or semantics.

Source of truth: `src/adapter/ap2.rs`. The constant `AP2_SPEC_SNAPSHOT` must
match the date of the most recent entry below.

---

## 2026-05-02 — Initial adapter

Snapshot built from public AP2 docs (https://ap2-protocol.org/) plus the
W3C Verifiable Credentials structure AP2 builds on. **Field paths below were
not exhaustively validated against a published normative schema** — when the
spec settles, revisit each path and remove this caveat.

### Recognized top-level fields

- `type` — array; recognized variants ending in `Mandate` (e.g.
  `PaymentMandate`, `IntentMandate`, `CartMandate`)
- `expirationDate` (RFC3339) or `validUntil`
- `credentialSubject` (preferred) or `subject` or root document

### Recognized credentialSubject fields

- `id` — DID of the user
- `agentId` or `agent.id` — used as fallback for `agent_id`
- `intent` (preferred) or `payment` / `paymentIntent` / `transaction`
- `constraints` (top-level or under `intent`)

### Recognized intent fields

- `merchant.id` or `merchant.name` or `merchant` (string) or `payee`
- `amount.value` + `amount.currency` (also accepts `amount.amount`,
  `amount.currencyCode`)
- `network` — passed through verbatim; expected to be CAIP-2
- `taskId`

### Recognized constraint fields

- `merchantAllowList` or `allowedMerchants` — array of strings
- `spendingLimit` or `spendingLimits` or `maxAmount` — object or single-element array
  - `max.value` + `max.currency`, or top-level `amount`/`currency`
  - `period` or `window` — opaque string (e.g. `PT24H`)

### Behavior

- Raw mandate JSON is preserved verbatim in `PaymentAttempt.mandate`.
- All parsed AP2 metadata is mirrored into `PaymentAttempt.metadata` under
  `ap2_*` keys for downstream policy use.
- An invalid `expirationDate` aborts parsing with a clear error.
- Missing fields do not abort — they default to empty strings/zero. Policy
  evaluation must enforce required-field checks.

### Known gaps

- `proof` is ignored. Verifiable Credential proof verification is out of
  scope for the adapter; a downstream verifier should validate signatures.
- Multi-currency and per-merchant limit arrays are not yet expanded — only
  the first spending limit entry is captured.
- Refund / chargeback flow fields are not modeled.
