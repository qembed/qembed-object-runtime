-- Append-only domain events and their transactional delivery records.
CREATE TABLE event_stream_counters (
    tenant_id UUID NOT NULL,
    aggregate_type TEXT NOT NULL,
    aggregate_id UUID NOT NULL,
    last_stream_version BIGINT NOT NULL CHECK (last_stream_version > 0),
    PRIMARY KEY (tenant_id, aggregate_type, aggregate_id)
);

CREATE TABLE events (
    tenant_id UUID NOT NULL,
    event_id UUID NOT NULL,
    aggregate_type TEXT NOT NULL,
    aggregate_id UUID NOT NULL,
    stream_version BIGINT NOT NULL CHECK (stream_version > 0),
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, event_id),
    UNIQUE (event_id),
    CONSTRAINT events_stream_version_unique
        UNIQUE (tenant_id, aggregate_type, aggregate_id, stream_version)
);

-- Allocate versions in the database so concurrent appenders cannot reuse a
-- version. The counter row is scoped to one aggregate; unrelated streams do
-- not contend on a global sequence or lock.
CREATE FUNCTION assign_event_stream_version() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $event_stream_version$
BEGIN
    INSERT INTO event_stream_counters
        (tenant_id, aggregate_type, aggregate_id, last_stream_version)
    VALUES (NEW.tenant_id, NEW.aggregate_type, NEW.aggregate_id, 1)
    ON CONFLICT (tenant_id, aggregate_type, aggregate_id)
    DO UPDATE SET last_stream_version = event_stream_counters.last_stream_version + 1
    RETURNING last_stream_version INTO NEW.stream_version;
    RETURN NEW;
END
$event_stream_version$;

CREATE TRIGGER events_assign_stream_version
    BEFORE INSERT ON events
    FOR EACH ROW EXECUTE FUNCTION assign_event_stream_version();

CREATE INDEX events_aggregate_order_idx
    ON events (tenant_id, aggregate_type, aggregate_id, stream_version);

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
