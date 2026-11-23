-- RUST-058c: canonical Principal-backed delegation and per-Object public access.
-- Owner API authentication remains compatible with owner_credentials; legacy Owner
-- rows are mirrored into principals so they participate in the common delegation graph.
BEGIN;

CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- The canonical schema owns the FK to tenants(id). The isolated Rust test
-- fixture may not install the tenant registry, so add the FK only when present.
CREATE TABLE IF NOT EXISTS principals (
    id UUID PRIMARY KEY,
    tenant_id UUID,
    kind TEXT NOT NULL CHECK (kind IN ('TenantOwner', 'TenantAdmin', 'Service', 'PlatformAdmin')),
    key_hash BYTEA NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at TIMESTAMPTZ,
    CHECK ((kind = 'PlatformAdmin') = (tenant_id IS NULL)),
    UNIQUE (tenant_id, id)
);

DO $principal_tenant_fk$
BEGIN
    IF to_regclass('public.tenants') IS NOT NULL
       AND NOT EXISTS (
           SELECT 1
             FROM pg_constraint
            WHERE conrelid = 'public.principals'::regclass
              AND conname = 'principals_tenant_id_fkey'
       ) THEN
        ALTER TABLE principals
            ADD CONSTRAINT principals_tenant_id_fkey
            FOREIGN KEY (tenant_id) REFERENCES tenants(id);
    END IF;
END
$principal_tenant_fk$;

INSERT INTO principals (id, tenant_id, kind, key_hash, revoked_at)
SELECT owner.caller_id,
       owner.tenant_id,
       'TenantOwner',
       digest('legacy-owner-principal:' || owner.tenant_id::text || ':' || owner.caller_id::text, 'sha256'),
       owner.disabled_at
  FROM owner_principals AS owner
ON CONFLICT (id) DO NOTHING;

CREATE OR REPLACE FUNCTION sync_legacy_owner_principal()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp
AS $sync_legacy_owner_principal$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO principals (id, tenant_id, kind, key_hash, revoked_at)
        VALUES (
            NEW.caller_id, NEW.tenant_id, 'TenantOwner',
            digest('legacy-owner-principal:' || NEW.tenant_id::text || ':' || NEW.caller_id::text, 'sha256'),
            NEW.disabled_at
        )
        ON CONFLICT (id) DO NOTHING;
        RETURN NEW;
    END IF;

    IF NEW.disabled_at IS DISTINCT FROM OLD.disabled_at THEN
        UPDATE principals
           SET revoked_at = NEW.disabled_at
         WHERE id = NEW.caller_id AND tenant_id = NEW.tenant_id
           AND kind = 'TenantOwner'
           AND (revoked_at IS NULL OR NEW.disabled_at IS NOT NULL);
    END IF;
    RETURN NEW;
END
$sync_legacy_owner_principal$;

ALTER TABLE principals ENABLE ROW LEVEL SECURITY;
ALTER TABLE principals FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS principals_runtime_scope ON principals;
CREATE POLICY principals_runtime_scope ON principals
    FOR SELECT TO qembed_runtime
    USING (tenant_id = qembed_crypto.current_tenant_id());
DROP POLICY IF EXISTS principals_definer_scope ON principals;
CREATE POLICY principals_definer_scope ON principals
    FOR ALL TO qembed_definer USING (true) WITH CHECK (true);

DROP TRIGGER IF EXISTS trg_sync_legacy_owner_principal ON owner_principals;
CREATE TRIGGER trg_sync_legacy_owner_principal
    AFTER INSERT OR UPDATE OF disabled_at ON owner_principals
    FOR EACH ROW EXECUTE FUNCTION sync_legacy_owner_principal();

DO $identity_check$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM owner_principals owner
          LEFT JOIN principals principal
            ON principal.id = owner.caller_id
           AND principal.tenant_id = owner.tenant_id
         WHERE principal.id IS NULL OR principal.kind <> 'TenantOwner'
    ) THEN
        RAISE EXCEPTION 'legacy Owner identity cannot be mapped to a TenantOwner Principal';
    END IF;
END
$identity_check$;

-- Keep legacy Owner lifecycle changes reflected in the canonical identity row.
ALTER TABLE objects
    ADD COLUMN IF NOT EXISTS anonymous_invocation_enabled BOOLEAN NOT NULL DEFAULT TRUE;

ALTER TABLE principal_caller_delegations
    DROP CONSTRAINT IF EXISTS principal_caller_delegations_check1,
    DROP CONSTRAINT IF EXISTS principal_caller_delegations_principal_fk,
    DROP CONSTRAINT IF EXISTS principal_caller_delegations_revoker_fk,
    DROP CONSTRAINT IF EXISTS principal_caller_delegations_tenant_id_principal_id_fkey,
    DROP CONSTRAINT IF EXISTS principal_caller_delegations_tenant_id_revoked_by_principal_id_fkey;

ALTER TABLE principal_caller_delegations
    ADD CONSTRAINT principal_caller_delegations_principal_fk
        FOREIGN KEY (tenant_id, principal_id)
        REFERENCES principals (tenant_id, id) ON DELETE RESTRICT,
    ADD CONSTRAINT principal_caller_delegations_revoker_fk
        FOREIGN KEY (tenant_id, revoked_by_principal_id)
        REFERENCES principals (tenant_id, id) ON DELETE RESTRICT;

CREATE OR REPLACE FUNCTION authenticate_principal(p_key_hash BYTEA)
RETURNS TABLE (principal_id UUID, tenant_id UUID, kind TEXT)
LANGUAGE sql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp
AS $authenticate_principal$
    SELECT principal.id, principal.tenant_id, principal.kind
      FROM public.principals AS principal
     WHERE principal.key_hash = p_key_hash
       AND principal.revoked_at IS NULL
$authenticate_principal$;

ALTER FUNCTION authenticate_principal(BYTEA) OWNER TO qembed_definer;
REVOKE ALL ON FUNCTION authenticate_principal(BYTEA) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION authenticate_principal(BYTEA) TO qembed_runtime;

-- Replace the self-delegation function with an Owner-controlled grant to any
-- active Principal in the same tenant.
DROP FUNCTION IF EXISTS grant_caller_delegation(UUID);

CREATE FUNCTION grant_caller_delegation(
    p_target_principal_id UUID,
    p_caller_id UUID
)
RETURNS UUID
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp
AS $grant_caller_delegation$
DECLARE
    v_tenant_id UUID := qembed_crypto.current_tenant_id();
    v_actor_principal_id UUID := qembed_crypto.current_principal_id();
    v_delegation_id UUID := gen_random_uuid();
BEGIN
    IF v_tenant_id IS NULL OR v_actor_principal_id IS NULL
       OR p_target_principal_id IS NULL OR p_caller_id IS NULL THEN
        RAISE EXCEPTION 'tenant, authenticated Owner, target Principal and Caller are required'
            USING ERRCODE = '42501';
    END IF;

    PERFORM 1 FROM principals actor
     WHERE actor.tenant_id = v_tenant_id
       AND actor.id = v_actor_principal_id
       AND actor.kind = 'TenantOwner'
       AND actor.revoked_at IS NULL
       AND EXISTS (
           SELECT 1 FROM owner_principals legacy
            WHERE legacy.tenant_id = actor.tenant_id
              AND legacy.caller_id = actor.id
              AND legacy.disabled_at IS NULL
       )
     FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'only an active TenantOwner may grant Caller delegation'
            USING ERRCODE = '42501';
    END IF;

    PERFORM 1 FROM principals target
     WHERE target.tenant_id = v_tenant_id
       AND target.id = p_target_principal_id
       AND target.revoked_at IS NULL
       AND (
           target.kind <> 'TenantOwner'
           OR EXISTS (
               SELECT 1 FROM owner_principals legacy
                WHERE legacy.tenant_id = target.tenant_id
                  AND legacy.caller_id = target.id
                  AND legacy.disabled_at IS NULL
           )
       )
     FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'target Principal is not active in this tenant'
            USING ERRCODE = '42501';
    END IF;

    PERFORM 1 FROM callers caller
     WHERE caller.tenant_id = v_tenant_id
       AND caller.id = p_caller_id
       AND caller.revoked_at IS NULL
     FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'target Caller is not active in this tenant'
            USING ERRCODE = '42501';
    END IF;

    INSERT INTO principal_caller_delegations
        (delegation_id, tenant_id, principal_id, caller_id)
    VALUES
        (v_delegation_id, v_tenant_id, p_target_principal_id, p_caller_id);
    RETURN v_delegation_id;
END
$grant_caller_delegation$;

-- Compatibility overload: preserve Owner self-delegation callers while
-- making the target Principal explicit in all new code.
CREATE FUNCTION grant_caller_delegation(p_caller_id UUID)
RETURNS UUID
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp
AS $grant_caller_delegation_legacy$
DECLARE
    v_principal_id UUID := qembed_crypto.current_principal_id();
BEGIN
    IF v_principal_id IS NULL THEN
        RAISE EXCEPTION 'authenticated Owner context is required'
            USING ERRCODE = '42501';
    END IF;
    RETURN grant_caller_delegation(v_principal_id, p_caller_id);
END
$grant_caller_delegation_legacy$;

CREATE OR REPLACE FUNCTION revoke_caller_delegation(p_delegation_id UUID)
RETURNS BOOLEAN
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp
AS $revoke_caller_delegation$
DECLARE
    v_tenant_id UUID := qembed_crypto.current_tenant_id();
    v_actor_principal_id UUID := qembed_crypto.current_principal_id();
BEGIN
    IF v_tenant_id IS NULL OR v_actor_principal_id IS NULL OR p_delegation_id IS NULL THEN
        RAISE EXCEPTION 'tenant, authenticated Owner and delegation are required'
            USING ERRCODE = '42501';
    END IF;

    PERFORM 1 FROM principals actor
     WHERE actor.tenant_id = v_tenant_id
       AND actor.id = v_actor_principal_id
       AND actor.kind = 'TenantOwner'
       AND actor.revoked_at IS NULL
       AND EXISTS (
           SELECT 1 FROM owner_principals legacy
            WHERE legacy.tenant_id = actor.tenant_id
              AND legacy.caller_id = actor.id
              AND legacy.disabled_at IS NULL
       )
     FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'only an active TenantOwner may revoke Caller delegation'
            USING ERRCODE = '42501';
    END IF;

    UPDATE principal_caller_delegations
       SET revoked_at = clock_timestamp(),
           revoked_by_principal_id = v_actor_principal_id
     WHERE delegation_id = p_delegation_id
       AND tenant_id = v_tenant_id
       AND revoked_at IS NULL;
    RETURN FOUND;
END
$revoke_caller_delegation$;

CREATE OR REPLACE FUNCTION resolve_delegated_caller(
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

    RETURN QUERY
    SELECT d.delegation_id, c.id, c.caller_kind
      FROM principal_caller_delegations d
      JOIN principals p
        ON p.tenant_id = d.tenant_id AND p.id = d.principal_id
      JOIN callers c
        ON c.tenant_id = d.tenant_id AND c.id = d.caller_id
     WHERE d.tenant_id = p_tenant_id
       AND d.principal_id = p_principal_id
       AND d.caller_id = p_caller_id
       AND d.revoked_at IS NULL
       AND p.revoked_at IS NULL
       AND (
           p.kind <> 'TenantOwner'
           OR EXISTS (
               SELECT 1 FROM owner_principals legacy
                WHERE legacy.tenant_id = p.tenant_id
                  AND legacy.caller_id = p.id
                  AND legacy.disabled_at IS NULL
           )
       )
       AND c.revoked_at IS NULL
     FOR SHARE OF d, p, c;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'no active delegation for this Principal and Caller'
            USING ERRCODE = '42501';
    END IF;
END
$resolve_delegated_caller$;

ALTER FUNCTION sync_legacy_owner_principal() OWNER TO qembed_definer;
REVOKE ALL ON FUNCTION sync_legacy_owner_principal() FROM PUBLIC;
ALTER FUNCTION grant_caller_delegation(UUID, UUID) OWNER TO qembed_definer;
ALTER FUNCTION grant_caller_delegation(UUID) OWNER TO qembed_definer;
ALTER FUNCTION revoke_caller_delegation(UUID) OWNER TO qembed_definer;
ALTER FUNCTION resolve_delegated_caller(UUID, UUID, UUID) OWNER TO qembed_definer;

REVOKE ALL ON FUNCTION grant_caller_delegation(UUID, UUID) FROM PUBLIC;
REVOKE ALL ON FUNCTION grant_caller_delegation(UUID) FROM PUBLIC;
REVOKE ALL ON FUNCTION revoke_caller_delegation(UUID) FROM PUBLIC;
REVOKE ALL ON FUNCTION resolve_delegated_caller(UUID, UUID, UUID) FROM PUBLIC;

GRANT SELECT ON principals TO qembed_runtime, qembed_definer;
GRANT INSERT, UPDATE ON principals TO qembed_definer;
GRANT EXECUTE ON FUNCTION grant_caller_delegation(UUID, UUID) TO qembed_runtime;
GRANT EXECUTE ON FUNCTION grant_caller_delegation(UUID) TO qembed_runtime;
GRANT EXECUTE ON FUNCTION revoke_caller_delegation(UUID) TO qembed_runtime;
GRANT EXECUTE ON FUNCTION resolve_delegated_caller(UUID, UUID, UUID) TO qembed_runtime;
GRANT UPDATE (revoked_at) ON principals TO qembed_definer;
GRANT SELECT, INSERT, UPDATE ON principal_caller_delegations TO qembed_definer;
GRANT UPDATE (revoked_at) ON callers TO qembed_definer;

COMMENT ON COLUMN objects.anonymous_invocation_enabled IS
    'When true, this Object may be invoked without an authenticated Principal; defaults to public.';

COMMIT;
