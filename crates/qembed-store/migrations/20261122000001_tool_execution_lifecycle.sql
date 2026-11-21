-- RUST-053: persistence prerequisites and independent ToolExecution lifecycle.
--
-- The canonical qembed-sql contract defines ToolDefinitions, ToolVersions,
-- PolicyDecisions and ToolExecutions together. This Rust migration brings in only
-- those schema prerequisites plus the ToolExecution lease/state slice; it does
-- not implement the policy engine or protected command-payload workflow.
--
-- Invocation lease/version are deliberately not reused here. ToolExecution has
-- its own version, fencing_token, lease_owner, lease_until and heartbeat_at.

CREATE TABLE tool_definitions (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE tool_versions (
    id UUID PRIMARY KEY,
    tool_id UUID NOT NULL REFERENCES tool_definitions(id),
    version TEXT NOT NULL,
    risk_level TEXT NOT NULL CHECK (risk_level IN ('R0','R1','R2','R3','R4')),
    input_schema JSONB NOT NULL,
    output_schema JSONB NOT NULL,
    timeout_ms INTEGER NOT NULL CHECK (timeout_ms > 0),
    idempotent BOOLEAN NOT NULL,
    CHECK (risk_level IN ('R0','R1') OR idempotent),
    UNIQUE (tool_id, version),
    UNIQUE (tool_id, id)
);

CREATE TABLE policy_decisions (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    invocation_id UUID NOT NULL,
    caller_id UUID NOT NULL,
    snapshot_id UUID NOT NULL,
    tool_id UUID NOT NULL,
    tool_version_id UUID,
    binding_version BIGINT,
    operation TEXT NOT NULL,
    arguments_hash BYTEA NOT NULL CHECK (octet_length(arguments_hash) = 32),
    action_hash BYTEA NOT NULL CHECK (octet_length(action_hash) = 32),
    risk_level TEXT NOT NULL CHECK (risk_level IN ('R0','R1','R2','R3','R4')),
    decision TEXT NOT NULL CHECK (decision IN ('Allow','Deny')),
    reason TEXT,
    policy_version TEXT NOT NULL,
    command_id UUID,
    issued_at TIMESTAMPTZ,
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (decision <> 'Allow' OR (
        command_id IS NOT NULL AND expires_at IS NOT NULL AND issued_at IS NOT NULL
        AND tool_version_id IS NOT NULL AND binding_version IS NOT NULL
    )),
    CHECK (decision <> 'Deny' OR command_id IS NULL),
    UNIQUE (tenant_id, object_id, id),
    UNIQUE (tenant_id, object_id, invocation_id, id),
    UNIQUE (tenant_id, object_id, invocation_id, id, decision),
    UNIQUE (
        tenant_id, object_id, invocation_id, id, decision, command_id,
        caller_id, snapshot_id, tool_id, tool_version_id, binding_version,
        operation, arguments_hash
    ),
    UNIQUE (tenant_id, command_id),
    FOREIGN KEY (tenant_id, object_id, caller_id, invocation_id)
        REFERENCES object_invocations (tenant_id, object_id, caller_id, id),
    FOREIGN KEY (tenant_id, object_id, snapshot_id)
        REFERENCES snapshots (tenant_id, object_id, snapshot_id),
    FOREIGN KEY (tool_id, tool_version_id)
        REFERENCES tool_versions (tool_id, id)
);

CREATE TABLE tool_executions (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    invocation_id UUID NOT NULL,
    caller_id UUID NOT NULL,
    decision_id UUID NOT NULL,
    decision TEXT NOT NULL DEFAULT 'Allow' CHECK (decision = 'Allow'),
    command_id UUID NOT NULL,
    tool_id UUID NOT NULL,
    tool_version_id UUID NOT NULL,
    snapshot_id UUID NOT NULL,
    binding_version BIGINT NOT NULL,
    operation TEXT NOT NULL,
    arguments_hash BYTEA NOT NULL CHECK (octet_length(arguments_hash) = 32),
    status TEXT NOT NULL CHECK (status IN ('Authorized','Executing','Succeeded','Failed','Unknown')),
    lease_owner TEXT,
    lease_until TIMESTAMPTZ,
    heartbeat_at TIMESTAMPTZ,
    fencing_token BIGINT NOT NULL DEFAULT 0 CHECK (fencing_token >= 0),
    version BIGINT NOT NULL DEFAULT 0 CHECK (version >= 0),
    external_idempotency_key TEXT,
    reconciliation_state TEXT CHECK (reconciliation_state IN
        ('pending','confirmed_succeeded','confirmed_failed','manual')),
    output JSONB,
    output_expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ,
    CHECK (output IS NULL OR output_expires_at IS NOT NULL OR status = 'Failed'),
    CHECK ((status IN ('Succeeded','Failed')) = (finished_at IS NOT NULL)),
    CHECK (status <> 'Unknown' OR reconciliation_state IS NOT NULL),
    CHECK ((lease_owner IS NULL) = (lease_until IS NULL)),
    CHECK ((lease_owner IS NULL) = (heartbeat_at IS NULL)),
    CHECK ((status = 'Executing') = (lease_owner IS NOT NULL)),
    CHECK (status <> 'Authorized' OR (
        version = 0 AND fencing_token = 0 AND lease_owner IS NULL
        AND lease_until IS NULL AND heartbeat_at IS NULL AND finished_at IS NULL
    )),
    UNIQUE (tenant_id, command_id),
    UNIQUE (tenant_id, decision_id),
    FOREIGN KEY (
        tenant_id, object_id, invocation_id, decision_id, decision,
        command_id, caller_id, snapshot_id, tool_id, tool_version_id,
        binding_version, operation, arguments_hash
    ) REFERENCES policy_decisions (
        tenant_id, object_id, invocation_id, id, decision,
        command_id, caller_id, snapshot_id, tool_id, tool_version_id,
        binding_version, operation, arguments_hash
    ),
    FOREIGN KEY (tenant_id, command_id)
        REFERENCES policy_decisions (tenant_id, command_id),
    FOREIGN KEY (tenant_id, object_id, caller_id, invocation_id)
        REFERENCES object_invocations (tenant_id, object_id, caller_id, id),
    FOREIGN KEY (tool_id, tool_version_id)
        REFERENCES tool_versions (tool_id, id)
);

CREATE INDEX idx_te_unknown ON tool_executions (tenant_id) WHERE status = 'Unknown';
CREATE INDEX idx_te_lease ON tool_executions (lease_until) WHERE status = 'Executing';

-- Bind every execution claim to the exact immutable Allow decision.
CREATE FUNCTION tool_execution_claims_guard() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $tool_execution_claims_guard$
DECLARE
    decision_row policy_decisions%ROWTYPE;
BEGIN
    SELECT * INTO decision_row
      FROM policy_decisions
     WHERE tenant_id = NEW.tenant_id AND id = NEW.decision_id;

    IF NOT FOUND OR decision_row.decision <> 'Allow'
       OR (decision_row.tenant_id, decision_row.object_id, decision_row.invocation_id,
           decision_row.caller_id, decision_row.snapshot_id, decision_row.tool_id,
           decision_row.tool_version_id, decision_row.binding_version, decision_row.operation,
           decision_row.arguments_hash, decision_row.command_id)
          IS DISTINCT FROM
          (NEW.tenant_id, NEW.object_id, NEW.invocation_id, NEW.caller_id, NEW.snapshot_id,
           NEW.tool_id, NEW.tool_version_id, NEW.binding_version, NEW.operation,
           NEW.arguments_hash, NEW.command_id) THEN
        RAISE EXCEPTION 'tool execution claims differ from Allow decision';
    END IF;
    RETURN NEW;
END
$tool_execution_claims_guard$;

CREATE TRIGGER trg_tool_execution_claims_guard
    BEFORE INSERT OR UPDATE OF tenant_id, object_id, invocation_id, caller_id, decision_id,
        command_id, tool_id, tool_version_id, snapshot_id, binding_version, operation,
        arguments_hash, decision
    ON tool_executions
    FOR EACH ROW EXECUTE FUNCTION tool_execution_claims_guard();

-- ToolExecution owns an independent optimistic version and lease fence.
-- The initial Authorized -> Executing CAS acquires the lease and advances both
-- counters atomically. Heartbeats keep the same owner/token/version. An expired
-- execution cannot be stolen and replayed: it must become Unknown for reconciliation.
CREATE FUNCTION tool_execution_state_guard() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $tool_execution_state_guard$
DECLARE
    status_changed BOOLEAN;
    token_changed BOOLEAN;
    heartbeat_only BOOLEAN;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.status <> 'Authorized' OR NEW.version <> 0 OR NEW.fencing_token <> 0
           OR NEW.lease_owner IS NOT NULL OR NEW.lease_until IS NOT NULL
           OR NEW.heartbeat_at IS NOT NULL OR NEW.finished_at IS NOT NULL THEN
            RAISE EXCEPTION 'new tool execution must start Authorized at version/token zero without a lease'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF OLD.status IN ('Succeeded','Failed') THEN
        -- retention_purge may only null output bytes and their expiry on terminal rows.
        IF current_user = 'qembed_definer'
           AND NEW.output IS NULL AND NEW.output_expires_at IS NULL
           AND (to_jsonb(NEW) - 'output' - 'output_expires_at')
             = (to_jsonb(OLD) - 'output' - 'output_expires_at') THEN
            RETURN NEW;
        END IF;
        RAISE EXCEPTION 'terminal tool execution is immutable' USING ERRCODE = '55000';
    END IF;

    IF (NEW.id, NEW.tenant_id, NEW.object_id, NEW.invocation_id, NEW.caller_id,
        NEW.decision_id, NEW.decision, NEW.command_id, NEW.tool_id, NEW.tool_version_id,
        NEW.snapshot_id, NEW.binding_version, NEW.operation, NEW.arguments_hash,
        NEW.created_at, NEW.external_idempotency_key)
       IS DISTINCT FROM
       (OLD.id, OLD.tenant_id, OLD.object_id, OLD.invocation_id, OLD.caller_id,
        OLD.decision_id, OLD.decision, OLD.command_id, OLD.tool_id, OLD.tool_version_id,
        OLD.snapshot_id, OLD.binding_version, OLD.operation, OLD.arguments_hash,
        OLD.created_at, OLD.external_idempotency_key) THEN
        RAISE EXCEPTION 'tool execution identity is immutable' USING ERRCODE = '55000';
    END IF;

    status_changed := NEW.status IS DISTINCT FROM OLD.status;
    token_changed := NEW.fencing_token IS DISTINCT FROM OLD.fencing_token;

    IF status_changed AND NOT (
           (OLD.status = 'Authorized' AND NEW.status IN ('Executing','Failed'))
        OR (OLD.status = 'Executing' AND NEW.status IN ('Succeeded','Failed','Unknown'))
        OR (OLD.status = 'Unknown' AND NEW.status IN ('Succeeded','Failed'))
    ) THEN
        RAISE EXCEPTION 'invalid tool execution state transition: % -> %', OLD.status, NEW.status
            USING ERRCODE = '23514';
    END IF;

    IF status_changed THEN
        IF NEW.version::numeric <> OLD.version::numeric + 1 THEN
            RAISE EXCEPTION 'tool execution version must advance exactly once per state transition'
                USING ERRCODE = '40001';
        END IF;
    ELSIF NEW.version <> OLD.version THEN
        RAISE EXCEPTION 'tool execution version changed without a state transition'
            USING ERRCODE = '40001';
    END IF;

    IF OLD.status = 'Authorized' AND NEW.status = 'Executing' THEN
        IF NEW.fencing_token::numeric <> OLD.fencing_token::numeric + 1
           OR NEW.lease_owner IS NULL OR NEW.lease_until <= clock_timestamp()
           OR NEW.heartbeat_at IS NULL OR NEW.finished_at IS NOT NULL
           OR NEW.reconciliation_state IS NOT NULL THEN
            RAISE EXCEPTION 'claim must acquire a live lease and advance the execution fencing token'
                USING ERRCODE = '23514';
        END IF;
    ELSIF token_changed THEN
        RAISE EXCEPTION 'tool execution fencing token may change only on initial lease acquisition'
            USING ERRCODE = '40001';
    END IF;

    IF OLD.status = 'Executing' AND NEW.status IN ('Succeeded','Failed') THEN
        IF OLD.lease_until <= clock_timestamp() OR OLD.lease_owner IS NULL
           OR NEW.lease_owner IS NOT NULL OR NEW.lease_until IS NOT NULL
           OR NEW.heartbeat_at IS NOT NULL THEN
            RAISE EXCEPTION 'an expired or unowned execution lease cannot commit a result'
                USING ERRCODE = '40001';
        END IF;
    ELSIF OLD.status = 'Executing' AND NEW.status = 'Unknown' THEN
        IF OLD.lease_until > clock_timestamp()
           OR NEW.lease_owner IS NOT NULL OR NEW.lease_until IS NOT NULL
           OR NEW.heartbeat_at IS NOT NULL
           OR NEW.reconciliation_state IS DISTINCT FROM 'pending' THEN
            RAISE EXCEPTION 'an execution becomes Unknown only after lease expiry and enters pending reconciliation'
                USING ERRCODE = '23514';
        END IF;
    ELSIF OLD.status = 'Unknown' AND NEW.status IN ('Succeeded','Failed') THEN
        IF NEW.reconciliation_state IS NULL
           OR NEW.reconciliation_state NOT IN ('confirmed_succeeded','confirmed_failed')
           OR (NEW.status = 'Succeeded') <> (NEW.reconciliation_state = 'confirmed_succeeded') THEN
            RAISE EXCEPTION 'Unknown can become terminal only through matching confirmed reconciliation'
                USING ERRCODE = '23514';
        END IF;
    ELSIF OLD.status = 'Authorized' AND NEW.status = 'Failed' THEN
        IF NEW.lease_owner IS NOT NULL OR NEW.lease_until IS NOT NULL
           OR NEW.heartbeat_at IS NOT NULL THEN
            RAISE EXCEPTION 'an unclaimed execution cannot finish with a lease'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF OLD.status = 'Executing' AND NEW.status = 'Executing' THEN
        heartbeat_only :=
            (to_jsonb(NEW) - 'lease_until' - 'heartbeat_at')
            = (to_jsonb(OLD) - 'lease_until' - 'heartbeat_at');
        IF NOT heartbeat_only OR OLD.lease_until <= clock_timestamp()
           OR NEW.lease_owner IS DISTINCT FROM OLD.lease_owner
           OR NEW.fencing_token <> OLD.fencing_token
           OR NEW.lease_until <= OLD.lease_until
           OR NEW.lease_until <= clock_timestamp()
           OR NEW.heartbeat_at IS NULL OR NEW.heartbeat_at < OLD.heartbeat_at THEN
            RAISE EXCEPTION 'heartbeat must extend the current live lease without changing execution state'
                USING ERRCODE = '40001';
        END IF;
    ELSIF NOT status_changed AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION 'tool execution fields are immutable outside a state transition or lease heartbeat'
            USING ERRCODE = '55000';
    END IF;

    RETURN NEW;
END
$tool_execution_state_guard$;

CREATE TRIGGER trg_tool_execution_state_guard
    BEFORE INSERT OR UPDATE ON tool_executions
    FOR EACH ROW EXECUTE FUNCTION tool_execution_state_guard();

-- Tenant isolation follows the same trusted transaction-local Scope as Invocation.
ALTER TABLE policy_decisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE policy_decisions FORCE ROW LEVEL SECURITY;
CREATE POLICY policy_decisions_runtime_tenant ON policy_decisions
    FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

ALTER TABLE tool_executions ENABLE ROW LEVEL SECURITY;
ALTER TABLE tool_executions FORCE ROW LEVEL SECURITY;
CREATE POLICY tool_executions_runtime_tenant ON tool_executions
    FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
CREATE POLICY tool_executions_definer_access ON tool_executions
    FOR ALL TO qembed_definer USING (true) WITH CHECK (true);

GRANT SELECT ON tool_definitions, tool_versions TO qembed_runtime;
GRANT SELECT, INSERT, UPDATE ON tool_definitions, tool_versions TO qembed_admin;
GRANT SELECT, INSERT ON policy_decisions TO qembed_runtime;
GRANT SELECT, INSERT, UPDATE ON tool_executions TO qembed_runtime;
GRANT SELECT, UPDATE ON tool_executions TO qembed_definer;
