-- RT-050: pre-token Caller resolution receives trusted identity explicitly.
-- It only locks and returns an active delegation; the signed qctx1 token is the
-- sole source of effective SQL/RLS identity.
DO $preflight$
BEGIN
    IF to_regprocedure('qembed_crypto.current_tenant_id()') IS NULL
       OR to_regprocedure('qembed_crypto.current_principal_id()') IS NULL
       OR NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'qembed_definer') THEN
        RAISE EXCEPTION 'RT-050 verifier accessors and qembed_definer must be installed first';
    END IF;
END
$preflight$;

GRANT USAGE ON SCHEMA qembed_crypto TO qembed_definer;
GRANT EXECUTE ON FUNCTION qembed_crypto.current_tenant_id() TO qembed_definer;
GRANT EXECUTE ON FUNCTION qembed_crypto.current_principal_id() TO qembed_definer;
GRANT EXECUTE ON FUNCTION qembed_crypto.current_caller_id() TO qembed_definer;

DROP FUNCTION resolve_delegated_caller(uuid);

CREATE FUNCTION resolve_delegated_caller(
    p_tenant_id UUID,
    p_principal_id UUID,
    p_caller_id UUID
)
RETURNS TABLE (delegation_id UUID, caller_id UUID, caller_kind TEXT)
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp
AS $resolve_delegated_caller$
BEGIN
    IF p_tenant_id IS NULL OR p_principal_id IS NULL OR p_caller_id IS NULL THEN
        RAISE EXCEPTION 'explicit tenant, authenticated Principal and Caller are required'
            USING ERRCODE = '42501';
    END IF;

    -- This pre-token resolver is not an authorization source. It confirms and
    -- locks a real active delegation so the trusted Rust issuer can sign it.
    -- No identity GUC is read or written here.
    RETURN QUERY
    SELECT d.delegation_id, c.id, c.caller_kind
      FROM principal_caller_delegations d
      JOIN owner_principals p
        ON p.tenant_id = d.tenant_id AND p.caller_id = d.principal_id
      JOIN callers c
        ON c.tenant_id = d.tenant_id AND c.id = d.caller_id
     WHERE d.tenant_id = p_tenant_id
       AND d.principal_id = p_principal_id
       AND d.caller_id = p_caller_id
       AND d.revoked_at IS NULL
       AND p.disabled_at IS NULL
       AND c.revoked_at IS NULL
     FOR SHARE OF d, p, c;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'no active delegation for this Principal and Caller'
            USING ERRCODE = '42501';
    END IF;
END
$resolve_delegated_caller$;

ALTER FUNCTION resolve_delegated_caller(UUID, UUID, UUID) OWNER TO qembed_definer;
REVOKE ALL ON FUNCTION resolve_delegated_caller(UUID, UUID, UUID) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION resolve_delegated_caller(UUID, UUID, UUID) TO qembed_runtime;
