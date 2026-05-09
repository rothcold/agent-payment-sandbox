CREATE TABLE IF NOT EXISTS payment_events (
    id TEXT PRIMARY KEY NOT NULL,
    timestamp TEXT NOT NULL,
    kind TEXT NOT NULL,
    payment_attempt TEXT NOT NULL,
    policy_version TEXT,
    decision TEXT,
    ap2_spec_version TEXT,
    raw_payload_ref TEXT,
    detail TEXT
);

CREATE INDEX IF NOT EXISTS idx_payment_events_timestamp ON payment_events (timestamp);

CREATE INDEX IF NOT EXISTS idx_payment_events_kind ON payment_events (kind);
