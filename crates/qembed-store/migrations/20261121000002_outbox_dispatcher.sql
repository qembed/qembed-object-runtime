-- RUST-051: lease-fenced, cross-tenant outbox delivery.
--
-- The dispatcher role is intentionally limited to the delivery projection. It
-- cannot read events or event_id and cannot access domain tables.
ALTER TABLE outbox_messages
    ADD COLUMN lease_owner TEXT,
    ADD COLUMN lease_expires_at TIMESTAMPTZ,
    ADD COLUMN claim_token BIGINT NOT NULL DEFAULT 0 CHECK (claim_token >= 0),
    ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    ADD CONSTRAINT outbox_lease_pair
        CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL)),
    ADD CONSTRAINT outbox_delivered_unleased
        CHECK (delivered_at IS NULL OR lease_owner IS NULL);

-- The base Outbox migration already creates outbox_pending_idx on this
-- predicate; reuse it rather than maintaining a duplicate claim index.

-- The canonical schema previously granted event-log access to the dispatcher.
-- Remove both the table privilege and its permissive RLS policy.
DROP POLICY IF EXISTS disp_events ON events;
REVOKE SELECT ON events FROM qembed_dispatcher;

-- Remove table-wide SELECT: the dispatcher receives only the columns needed
-- to deliver, fence and acknowledge a message. In particular, event_id and
-- all columns from events remain outside its capability.
REVOKE SELECT ON outbox_messages FROM qembed_dispatcher;
GRANT SELECT (
    tenant_id, delivery_id, topic, payload, created_at,
    delivered_at, lease_owner, lease_expires_at, claim_token, attempts
) ON outbox_messages TO qembed_dispatcher;
GRANT UPDATE (
    delivered_at, lease_owner, lease_expires_at, claim_token, attempts
) ON outbox_messages TO qembed_dispatcher;

COMMENT ON COLUMN outbox_messages.claim_token IS
    'Monotonic fencing token; stale dispatchers cannot acknowledge a reclaimed delivery';
COMMENT ON COLUMN outbox_messages.attempts IS
    'Number of successful claims, used by later retry/dead-letter policy';
