-- RT-050: make the recovery sweeper consume the verified signed tenant.
--
-- This is the Rust test-schema counterpart of qembed-sql's canonical
-- rt050_signed_context_rls.sql migration. Keep the function body aligned with
-- that migration; the historical RUST-052c migration remains immutable.
CREATE OR REPLACE FUNCTION sweep_stale_invocation_deliveries(
    p_stale_before TIMESTAMPTZ,
    p_limit INT
)
RETURNS TABLE (replayed_invocation_id UUID, replayed_delivery_id UUID)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp
AS $invocation_replay_sweeper$
DECLARE
    v_tenant_id UUID := qembed_crypto.current_tenant_id();
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
           -- Missing provider-attempt evidence fails closed. An ambiguous or completed
           -- provider call must never be replayed by this queue-recovery path.
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

COMMENT ON FUNCTION sweep_stale_invocation_deliveries(TIMESTAMPTZ, INT) IS
    'Tenant-scoped replay request for stale Received invocations; tenant identity comes only from verified signed context; ambiguous provider outcomes fail closed';
