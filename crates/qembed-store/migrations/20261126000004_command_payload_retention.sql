-- RUST-087: bounded, privileged cleanup of encrypted command payloads.
--
-- Terminal executions no longer need their input payload. Non-terminal payloads
-- are retained until their own TTL; this includes Unknown executions so that
-- reconciliation can still inspect the original command until expiry.
--
-- The execution ledger is never deleted or modified by this function.

ALTER TABLE public.tool_command_payloads ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.tool_command_payloads FORCE ROW LEVEL SECURITY;

CREATE POLICY tool_command_payloads_definer_access ON public.tool_command_payloads
    FOR ALL TO qembed_definer
    USING (true)
    WITH CHECK (true);

GRANT SELECT, DELETE ON public.tool_command_payloads TO qembed_definer;
-- PostgreSQL requires UPDATE privilege on at least one column for SELECT FOR UPDATE.
-- Keep SKIP LOCKED concurrency while limiting that privilege to the TTL column;
-- qembed_runtime receives no direct UPDATE or DELETE capability.
GRANT UPDATE (expires_at) ON public.tool_command_payloads TO qembed_definer;

CREATE FUNCTION public.qembed_purge_tool_command_payloads(p_batch_size INTEGER)
RETURNS BIGINT
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $qembed_purge_tool_command_payloads$
DECLARE
    deleted_count BIGINT;
BEGIN
    IF p_batch_size IS NULL OR p_batch_size < 1 OR p_batch_size > 10000 THEN
        RAISE EXCEPTION 'payload cleanup batch size must be between 1 and 10000'
            USING ERRCODE = '22023';
    END IF;

    WITH candidates AS (
        SELECT payload.tenant_id, payload.command_id
          FROM public.tool_command_payloads AS payload
          LEFT JOIN public.tool_executions AS execution
            ON execution.tenant_id = payload.tenant_id
           AND execution.command_id = payload.command_id
         WHERE payload.expires_at <= clock_timestamp()
            OR execution.status IN ('Succeeded', 'Failed')
         ORDER BY
            CASE
                WHEN execution.status IN ('Succeeded', 'Failed')
                    THEN execution.finished_at
                ELSE payload.expires_at
            END NULLS FIRST,
            payload.created_at,
            payload.tenant_id,
            payload.command_id
         LIMIT p_batch_size
         FOR UPDATE OF payload SKIP LOCKED
    ),
    deleted AS (
        DELETE FROM public.tool_command_payloads AS payload
         USING candidates
         WHERE payload.tenant_id = candidates.tenant_id
           AND payload.command_id = candidates.command_id
        RETURNING 1
    )
    SELECT count(*) INTO deleted_count FROM deleted;

    RETURN deleted_count;
END
$qembed_purge_tool_command_payloads$;

ALTER FUNCTION public.qembed_purge_tool_command_payloads(INTEGER)
    OWNER TO qembed_definer;
REVOKE ALL ON FUNCTION public.qembed_purge_tool_command_payloads(INTEGER) FROM PUBLIC;
REVOKE ALL ON FUNCTION public.qembed_purge_tool_command_payloads(INTEGER) FROM qembed_runtime;
GRANT EXECUTE ON FUNCTION public.qembed_purge_tool_command_payloads(INTEGER)
    TO qembed_definer;
