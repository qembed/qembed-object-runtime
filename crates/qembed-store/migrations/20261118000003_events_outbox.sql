-- Append-only domain events and their transactional delivery records.
CREATE TABLE events (
    tenant_id UUID NOT NULL,
    event_id UUID NOT NULL,
    aggregate_type TEXT NOT NULL,
    aggregate_id UUID NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, event_id),
    UNIQUE (event_id)
);

CREATE INDEX events_aggregate_order_idx
    ON events (tenant_id, aggregate_type, aggregate_id, occurred_at, event_id);

CREATE FUNCTION reject_event_mutation() RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'events are append-only'
        USING ERRCODE = '55000';
END;
$$;

CREATE TRIGGER events_append_only
BEFORE UPDATE OR DELETE ON events
FOR EACH ROW EXECUTE FUNCTION reject_event_mutation();

CREATE TABLE outbox_messages (
    tenant_id UUID NOT NULL,
    delivery_id UUID NOT NULL,
    event_id UUID NOT NULL,
    topic TEXT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    delivered_at TIMESTAMPTZ,
    PRIMARY KEY (tenant_id, delivery_id),
    UNIQUE (tenant_id, event_id),
    FOREIGN KEY (tenant_id, event_id)
        REFERENCES events (tenant_id, event_id)
        ON DELETE RESTRICT
);

CREATE INDEX outbox_pending_idx
    ON outbox_messages (created_at, delivery_id)
    WHERE delivered_at IS NULL;
