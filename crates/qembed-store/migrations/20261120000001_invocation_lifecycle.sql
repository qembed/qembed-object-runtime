-- RUST-044: bring the invocation persistence slice in line with qembed-sql.
-- This migration adds only the canonical tables and constraints required by the
-- current Rust invocation, Caller delegation, and token-budget adapters. It does
-- not modify the canonical qembed-sql repository or introduce unrelated M1+ tables.

DO $roles$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'qembed_definer') THEN
        CREATE ROLE qembed_definer NOLOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
    END IF;
END
$roles$;

ALTER ROLE qembed_definer NOLOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
GRANT USAGE ON SCHEMA public TO qembed_definer;

-- The canonical Caller model references tenants. Owner authentication remains
-- backed by owner_principals; this table is the tenant registry used by Callers.
CREATE TABLE tenants (
    id UUID PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE callers (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants(id),
    token_hash BYTEA NOT NULL,
    caller_kind TEXT NOT NULL CHECK (caller_kind IN ('Anonymous', 'Authenticated', 'Service')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at TIMESTAMPTZ,
    UNIQUE (tenant_id, token_hash),
    UNIQUE (tenant_id, id)
);

CREATE TABLE principal_caller_delegations (
    delegation_id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    principal_id UUID NOT NULL,
    caller_id UUID NOT NULL,
    granted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at TIMESTAMPTZ,
    revoked_by_principal_id UUID,
    CHECK ((revoked_at IS NULL) = (revoked_by_principal_id IS NULL)),
    CHECK (revoked_by_principal_id IS NULL OR revoked_by_principal_id = principal_id),
    FOREIGN KEY (tenant_id, principal_id)
        REFERENCES owner_principals (tenant_id, caller_id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, caller_id)
        REFERENCES callers (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, revoked_by_principal_id)
        REFERENCES owner_principals (tenant_id, caller_id) ON DELETE RESTRICT
);

CREATE UNIQUE INDEX principal_caller_delegations_active
    ON principal_caller_delegations (tenant_id, principal_id, caller_id)
    WHERE revoked_at IS NULL;

CREATE FUNCTION principal_caller_delegation_guard() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $delegation_guard$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF (NEW.delegation_id, NEW.tenant_id, NEW.principal_id, NEW.caller_id, NEW.granted_at)
           IS DISTINCT FROM
           (OLD.delegation_id, OLD.tenant_id, OLD.principal_id, OLD.caller_id, OLD.granted_at) THEN
            RAISE EXCEPTION 'delegation identity is immutable' USING ERRCODE = '55000';
        END IF;
        IF OLD.revoked_at IS NOT NULL AND
           (NEW.revoked_at, NEW.revoked_by_principal_id)
           IS DISTINCT FROM (OLD.revoked_at, OLD.revoked_by_principal_id) THEN
            RAISE EXCEPTION 'delegation revocation is final' USING ERRCODE = '55000';
        END IF;
    ELSIF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'delegation history is append-only' USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END
$delegation_guard$;

CREATE TRIGGER trg_principal_caller_delegation_guard
    BEFORE UPDATE OR DELETE ON principal_caller_delegations
    FOR EACH ROW EXECUTE FUNCTION principal_caller_delegation_guard();

-- Invocation rows pin an immutable snapshot and the current knowledge revision.
-- The composite snapshot key is required to prevent cross-object references.
ALTER TABLE snapshots
    ADD CONSTRAINT snapshots_tenant_object_snapshot_unique
    UNIQUE (tenant_id, object_id, snapshot_id);

CREATE TABLE object_invocations (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    caller_id UUID NOT NULL,
    snapshot_id UUID NOT NULL,
    knowledge_revision BIGINT NOT NULL CHECK (knowledge_revision >= 0),
    pool TEXT NOT NULL CHECK (pool IN ('anonymous', 'authenticated')),
    status TEXT NOT NULL CHECK (status IN (
        'Received', 'ContextLoading', 'Processing', 'ExecutingTool', 'Verifying',
        'WaitingForInput', 'WaitingForApproval',
        'Completed', 'CompletedDegraded', 'Failed', 'Cancelled'
    )),
    error_code TEXT,
    degraded_reason TEXT,
    trace_id UUID NOT NULL,
    cancel_requested_at TIMESTAMPTZ,
    lease_owner TEXT,
    lease_until TIMESTAMPTZ,
    heartbeat_at TIMESTAMPTZ,
    fencing_token BIGINT NOT NULL DEFAULT 0 CHECK (fencing_token >= 0),
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ,
    version BIGINT NOT NULL DEFAULT 0 CHECK (version >= 0),
    CHECK ((status IN ('Completed', 'CompletedDegraded', 'Failed', 'Cancelled')) = (finished_at IS NOT NULL)),
    CHECK (status <> 'Failed' OR error_code IS NOT NULL),
    CHECK (status <> 'CompletedDegraded' OR degraded_reason IS NOT NULL),
    CHECK ((lease_owner IS NULL) = (lease_until IS NULL)),
    CHECK (finished_at IS NULL OR lease_owner IS NULL),
    UNIQUE (tenant_id, object_id, id),
    UNIQUE (tenant_id, object_id, caller_id, id),
    UNIQUE (tenant_id, object_id, id, pool),
    FOREIGN KEY (tenant_id, object_id)
        REFERENCES objects (tenant_id, object_id),
    FOREIGN KEY (tenant_id, caller_id)
        REFERENCES callers (tenant_id, id),
    FOREIGN KEY (tenant_id, object_id, snapshot_id)
        REFERENCES snapshots (tenant_id, object_id, snapshot_id)
);

CREATE INDEX idx_inv_active ON object_invocations (tenant_id, object_id)
    WHERE status NOT IN ('Completed', 'CompletedDegraded', 'Failed', 'Cancelled');
CREATE INDEX idx_inv_lease ON object_invocations (lease_until)
    WHERE status NOT IN ('Completed', 'CompletedDegraded', 'Failed', 'Cancelled');

CREATE FUNCTION invocation_state_guard() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $invocation_state_guard$
DECLARE
    status_changed BOOLEAN;
    token_changed BOOLEAN;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.status <> 'Received' OR NEW.version <> 0 OR NEW.fencing_token <> 0
           OR NEW.lease_owner IS NOT NULL OR NEW.lease_until IS NOT NULL
           OR NEW.finished_at IS NOT NULL THEN
            RAISE EXCEPTION 'new invocation must start Received at version/token zero without a lease'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF (NEW.id, NEW.tenant_id, NEW.object_id, NEW.caller_id, NEW.snapshot_id,
        NEW.knowledge_revision, NEW.pool, NEW.trace_id, NEW.started_at)
       IS DISTINCT FROM
       (OLD.id, OLD.tenant_id, OLD.object_id, OLD.caller_id, OLD.snapshot_id,
        OLD.knowledge_revision, OLD.pool, OLD.trace_id, OLD.started_at) THEN
        RAISE EXCEPTION 'invocation identity is immutable' USING ERRCODE = '55000';
    END IF;

    IF OLD.status IN ('Completed', 'CompletedDegraded', 'Failed', 'Cancelled') THEN
        RAISE EXCEPTION 'terminal invocation is immutable' USING ERRCODE = '55000';
    END IF;

    status_changed := NEW.status IS DISTINCT FROM OLD.status;
    token_changed := NEW.fencing_token IS DISTINCT FROM OLD.fencing_token;

    IF status_changed AND NOT (
           NEW.status IN ('Failed', 'Cancelled')
        OR (OLD.status = 'Received' AND NEW.status = 'ContextLoading')
        OR (OLD.status = 'ContextLoading' AND NEW.status = 'Processing')
        OR (OLD.status = 'Processing' AND NEW.status = 'ExecutingTool')
        OR (OLD.status = 'ExecutingTool' AND NEW.status = 'Processing')
        OR (OLD.status = 'Processing' AND NEW.status = 'Verifying')
        OR (OLD.status = 'Verifying' AND NEW.status IN ('Completed', 'CompletedDegraded'))
    ) THEN
        RAISE EXCEPTION 'invalid invocation state transition: % -> %', OLD.status, NEW.status
            USING ERRCODE = '23514';
    END IF;

    IF NEW.fencing_token < OLD.fencing_token THEN
        RAISE EXCEPTION 'stale invocation fencing token' USING ERRCODE = '40001';
    END IF;

    IF status_changed AND token_changed THEN
        RAISE EXCEPTION 'state transition and lease reclaim must be separate CAS operations'
            USING ERRCODE = '23514';
    END IF;

    IF status_changed OR token_changed THEN
        IF NEW.version::numeric <> OLD.version::numeric + 1 THEN
            RAISE EXCEPTION 'invocation optimistic version must advance exactly once'
                USING ERRCODE = '40001';
        END IF;
    ELSIF NEW.version <> OLD.version THEN
        RAISE EXCEPTION 'invocation version changed without state transition or lease reclaim'
            USING ERRCODE = '40001';
    END IF;

    IF token_changed THEN
        IF NEW.fencing_token <= OLD.fencing_token
           OR OLD.lease_owner IS NULL OR OLD.lease_until IS NULL
           OR OLD.lease_until >= now()
           OR NEW.lease_owner IS NULL OR NEW.lease_until IS NULL THEN
            RAISE EXCEPTION 'fencing token may advance only when reclaiming an expired lease'
                USING ERRCODE = '23514';
        END IF;
    ELSIF OLD.lease_owner IS NOT NULL
          AND OLD.lease_until > now()
          AND NEW.lease_owner IS DISTINCT FROM OLD.lease_owner THEN
        RAISE EXCEPTION 'an active invocation lease cannot be stolen'
            USING ERRCODE = '40001';
    ELSIF OLD.lease_owner IS NOT NULL
          AND OLD.lease_until <= now()
          AND (NEW.lease_owner, NEW.lease_until)
              IS DISTINCT FROM (OLD.lease_owner, OLD.lease_until)
          AND NOT (status_changed AND NEW.status IN ('Completed', 'CompletedDegraded', 'Failed', 'Cancelled')) THEN
        RAISE EXCEPTION 'an expired invocation lease must be reclaimed with a newer fencing token'
            USING ERRCODE = '40001';
    END IF;

    RETURN NEW;
END
$invocation_state_guard$;

CREATE TRIGGER trg_invocation_state_guard
    BEFORE INSERT OR UPDATE ON object_invocations
    FOR EACH ROW EXECUTE FUNCTION invocation_state_guard();

CREATE TABLE invocation_idempotency (
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    caller_id UUID NOT NULL,
    idempotency_key TEXT NOT NULL,
    payload_hash BYTEA NOT NULL,
    invocation_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (tenant_id, object_id, caller_id, idempotency_key),
    UNIQUE (tenant_id, object_id, invocation_id),
    FOREIGN KEY (tenant_id, object_id, caller_id, invocation_id)
        REFERENCES object_invocations (tenant_id, object_id, caller_id, id)
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE interaction_messages (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    caller_id UUID NOT NULL,
    invocation_id UUID NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'tool')),
    content TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    FOREIGN KEY (tenant_id, object_id, caller_id, invocation_id)
        REFERENCES object_invocations (tenant_id, object_id, caller_id, id)
);
CREATE INDEX idx_msg_caller ON interaction_messages (tenant_id, caller_id);
CREATE INDEX idx_msg_expiry ON interaction_messages (expires_at);

-- Existing Objects must also have a current revision before the trigger is installed.
CREATE TABLE knowledge_revisions (
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    current_revision BIGINT NOT NULL DEFAULT 0 CHECK (current_revision >= 0),
    PRIMARY KEY (tenant_id, object_id),
    FOREIGN KEY (tenant_id, object_id)
        REFERENCES objects (tenant_id, object_id)
);
INSERT INTO knowledge_revisions (tenant_id, object_id)
    SELECT tenant_id, object_id FROM objects
    ON CONFLICT (tenant_id, object_id) DO NOTHING;

CREATE FUNCTION objects_init_knowledge() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO knowledge_revisions (tenant_id, object_id)
    VALUES (NEW.tenant_id, NEW.object_id);
    RETURN NEW;
END
$$;
CREATE TRIGGER trg_objects_init_knowledge
    AFTER INSERT ON objects
    FOR EACH ROW EXECUTE FUNCTION objects_init_knowledge();

-- Complete the deferred parent links intentionally omitted by the earlier
-- budget/provider migrations while object_invocations did not yet exist.
ALTER TABLE budget_reservations
    ADD CONSTRAINT budget_reservations_invocation_pool_fk
    FOREIGN KEY (tenant_id, object_id, invocation_id, pool)
    REFERENCES object_invocations (tenant_id, object_id, id, pool)
    DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE provider_call_attempts
    ADD CONSTRAINT provider_call_attempts_invocation_fk
    FOREIGN KEY (tenant_id, object_id, invocation_id)
    REFERENCES object_invocations (tenant_id, object_id, id)
    ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE caller_usage_counters
    ADD CONSTRAINT caller_usage_counters_caller_fk
    FOREIGN KEY (tenant_id, caller_id)
    REFERENCES callers (tenant_id, id)
    ON DELETE CASCADE;

ALTER TABLE tenants ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenants FORCE ROW LEVEL SECURITY;
CREATE POLICY tenants_runtime_tenant ON tenants
    FOR SELECT TO qembed_runtime
    USING (id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
CREATE POLICY tenants_admin_all ON tenants
    TO qembed_admin USING (true) WITH CHECK (true);

DO $rls$
DECLARE
    table_name TEXT;
BEGIN
    FOREACH table_name IN ARRAY ARRAY[
        'callers',
        'principal_caller_delegations',
        'object_invocations',
        'invocation_idempotency',
        'interaction_messages',
        'knowledge_revisions'
    ] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', table_name);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', table_name);
        EXECUTE format(
            'CREATE POLICY tenant_runtime_isolation ON %I TO qembed_runtime
             USING (tenant_id = NULLIF(current_setting(''app.tenant_id'', true), '''')::uuid)
             WITH CHECK (tenant_id = NULLIF(current_setting(''app.tenant_id'', true), '''')::uuid)',
            table_name
        );
    END LOOP;
END
$rls$;

CREATE POLICY owner_principals_definer_access ON owner_principals
    TO qembed_definer USING (true) WITH CHECK (true);
DO $definer_rls$
DECLARE
    table_name TEXT;
BEGIN
    FOREACH table_name IN ARRAY ARRAY[
        'callers',
        'principal_caller_delegations',
        'object_invocations',
        'invocation_idempotency',
        'interaction_messages',
        'budget_reservations',
        'provider_call_attempts',
        'caller_usage_counters'
    ] LOOP
        EXECUTE format(
            'CREATE POLICY definer_access ON %I TO qembed_definer
             USING (true) WITH CHECK (true)',
            table_name
        );
    END LOOP;
END
$definer_rls$;

GRANT SELECT ON tenants TO qembed_runtime;
GRANT SELECT, INSERT, UPDATE ON callers TO qembed_runtime;
GRANT SELECT ON principal_caller_delegations TO qembed_runtime;
GRANT SELECT, INSERT, UPDATE ON
    object_invocations, invocation_idempotency, interaction_messages, knowledge_revisions
TO qembed_runtime;
GRANT DELETE ON interaction_messages TO qembed_runtime;
GRANT SELECT ON tenants TO qembed_admin;
GRANT SELECT, INSERT ON knowledge_revisions TO qembed_admin;

GRANT SELECT ON owner_principals, callers, object_invocations TO qembed_definer;
GRANT UPDATE (disabled_at) ON owner_principals TO qembed_definer;
GRANT UPDATE (revoked_at) ON callers TO qembed_definer;
GRANT SELECT, INSERT, UPDATE ON principal_caller_delegations TO qembed_definer;
GRANT SELECT, DELETE ON invocation_idempotency, interaction_messages TO qembed_definer;
GRANT SELECT, UPDATE ON budget_reservations, provider_call_attempts, caller_usage_counters TO qembed_definer;

CREATE FUNCTION resolve_delegated_caller(p_caller_id UUID)
RETURNS TABLE (delegation_id UUID, caller_id UUID, caller_kind TEXT)
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp
AS $resolve_delegated_caller$
DECLARE
    v_tenant_id UUID := NULLIF(current_setting('app.tenant_id', true), '')::uuid;
    v_principal_id UUID := NULLIF(current_setting('app.principal_id', true), '')::uuid;
BEGIN
    IF v_tenant_id IS NULL OR v_principal_id IS NULL THEN
        RAISE EXCEPTION 'tenant and authenticated principal context are required'
            USING ERRCODE = '42501';
    END IF;

    RETURN QUERY
    SELECT d.delegation_id, c.id, c.caller_kind
      FROM principal_caller_delegations d
      JOIN owner_principals p
        ON p.tenant_id = d.tenant_id AND p.caller_id = d.principal_id
      JOIN callers c
        ON c.tenant_id = d.tenant_id AND c.id = d.caller_id
     WHERE d.tenant_id = v_tenant_id
       AND d.principal_id = v_principal_id
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

ALTER FUNCTION resolve_delegated_caller(UUID) OWNER TO qembed_definer;
REVOKE ALL ON FUNCTION resolve_delegated_caller(UUID) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION resolve_delegated_caller(UUID) TO qembed_runtime;
