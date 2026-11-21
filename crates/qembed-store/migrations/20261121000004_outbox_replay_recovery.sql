-- RUST-052c: durable replay markers for PostgreSQL-driven invocation recovery.
--
-- A replay reuses the original Outbox row and delivery_id. The append-only
-- outbox_publication_receipts table remains the complete PubAck history.
ALTER TABLE outbox_messages
    ADD COLUMN replay_requested_at TIMESTAMPTZ,
    ADD COLUMN replay_reason TEXT,
    ADD COLUMN replay_count INTEGER NOT NULL DEFAULT 0 CHECK (replay_count >= 0),
    DROP CONSTRAINT outbox_delivered_unleased,
    ADD CONSTRAINT outbox_delivered_unleased CHECK (
        delivered_at IS NULL OR replay_requested_at IS NOT NULL OR lease_owner IS NULL
    ),
    ADD CONSTRAINT outbox_replay_marker_pair CHECK (
        (replay_requested_at IS NULL AND replay_reason IS NULL)
        OR
        (replay_requested_at IS NOT NULL
            AND replay_reason = 'invocation_progress_deadline_expired'
            AND delivered_at IS NOT NULL)
    );

CREATE INDEX outbox_replay_requested_idx
    ON outbox_messages (replay_requested_at, created_at, delivery_id)
    WHERE replay_requested_at IS NOT NULL;

CREATE POLICY outbox_recovery_definer_access ON outbox_messages
    TO qembed_definer USING (true) WITH CHECK (true);

GRANT SELECT ON outbox_messages TO qembed_definer;
GRANT UPDATE (heartbeat_at) ON object_invocations TO qembed_definer;
GRANT UPDATE (replay_requested_at, replay_reason, replay_count)
    ON outbox_messages TO qembed_definer;

GRANT SELECT (replay_requested_at, replay_reason, replay_count)
    ON outbox_messages TO qembed_dispatcher;
GRANT UPDATE (replay_requested_at, replay_reason, replay_count)
    ON outbox_messages TO qembed_dispatcher;

CREATE FUNCTION sweep_stale_invocation_deliveries(
    p_stale_before TIMESTAMPTZ,
    p_limit INT
)
RETURNS TABLE (replayed_invocation_id UUID, replayed_delivery_id UUID)
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp
AS $invocation_replay_sweeper$
DECLARE
    v_tenant_id UUID := NULLIF(current_setting('app.tenant_id', true), '')::uuid;
    candidate RECORD;
BEGIN
    IF v_tenant_id IS NULL THEN
        RAISE EXCEPTION 'tenant context is required for invocation recovery'
            USING ERRCODE = '42501';
    END IF;
    IF p_stale_before IS NULL OR p_stale_before > now()
       OR p_limit IS NULL OR p_limit NOT BETWEEN 1 AND 500 THEN
        RAISE EXCEPTION 'invalid invocation recovery cutoff or batch size'
            USING ERRCODE = '22023';
    END IF;

    FOR candidate IN
        SELECT invocation.id AS invocation_id, delivery.delivery_id
          FROM object_invocations AS invocation
          JOIN outbox_messages AS delivery
            ON delivery.tenant_id = invocation.tenant_id
           AND delivery.topic = 'invocation.received'
           AND delivery.payload->>'invocation_id' = invocation.id::text
         WHERE invocation.tenant_id = v_tenant_id
           AND invocation.status = 'Received'
           AND invocation.cancel_requested_at IS NULL
           AND (invocation.lease_owner IS NULL OR invocation.lease_until <= now())
           AND COALESCE(invocation.heartbeat_at, invocation.started_at) <= p_stale_before
           AND delivery.delivered_at IS NOT NULL
           AND delivery.replay_requested_at IS NULL
           AND (delivery.lease_expires_at IS NULL OR delivery.lease_expires_at <= now())
           AND EXISTS (
               SELECT 1
                 FROM provider_call_attempts AS attempt
                WHERE attempt.tenant_id = invocation.tenant_id
                  AND attempt.object_id = invocation.object_id
                  AND attempt.invocation_id = invocation.id
                  AND attempt.state IN ('NotStarted', 'ConfirmedNotAccepted', 'CancelledBeforeSend')
           )
         ORDER BY COALESCE(invocation.heartbeat_at, invocation.started_at),
                  delivery.created_at
         LIMIT p_limit
         FOR UPDATE OF invocation, delivery SKIP LOCKED
    LOOP
        UPDATE outbox_messages
           SET replay_requested_at = clock_timestamp(),
               replay_reason = 'invocation_progress_deadline_expired',
               replay_count = replay_count + 1
         WHERE tenant_id = v_tenant_id
           AND delivery_id = candidate.delivery_id
           AND replay_requested_at IS NULL;

        IF FOUND THEN
            replayed_invocation_id := candidate.invocation_id;
            replayed_delivery_id := candidate.delivery_id;
            RETURN NEXT;
        END IF;
    END LOOP;
END
$invocation_replay_sweeper$;

ALTER FUNCTION sweep_stale_invocation_deliveries(TIMESTAMPTZ, INT)
    OWNER TO qembed_definer;
REVOKE ALL ON FUNCTION sweep_stale_invocation_deliveries(TIMESTAMPTZ, INT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION sweep_stale_invocation_deliveries(TIMESTAMPTZ, INT)
    TO qembed_runtime;

COMMENT ON COLUMN outbox_messages.replay_requested_at IS
    'Durable marker: Relay republishes this already-delivered Outbox row with the same delivery_id';
COMMENT ON COLUMN outbox_messages.replay_reason IS
    'Allowlisted reason for the currently pending replay request';
COMMENT ON COLUMN outbox_messages.replay_count IS
    'Number of PostgreSQL recovery sweeps that requested replay for this Outbox row';
COMMENT ON FUNCTION sweep_stale_invocation_deliveries(TIMESTAMPTZ, INT) IS
    'Tenant-scoped replay request for stale Received invocations; ambiguous provider outcomes fail closed';
