-- RUST-041b: mutable token reservation accounting.
-- Physical columns and constraints mirror the canonical qembed-sql schema.
-- anonymous_share_pct is intentionally not consulted by runtime code.

CREATE TABLE object_budgets (
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    daily_token_limit BIGINT NOT NULL CHECK (daily_token_limit >= 0),
    daily_cost_limit_micros BIGINT NOT NULL CHECK (daily_cost_limit_micros >= 0),
    anonymous_share_pct SMALLINT NOT NULL DEFAULT 50 CHECK (anonymous_share_pct BETWEEN 0 AND 100),
    per_caller_daily_token_limit BIGINT NOT NULL CHECK (per_caller_daily_token_limit >= 0),
    max_concurrent_invocations INTEGER NOT NULL CHECK (max_concurrent_invocations > 0),
    max_llm_calls INTEGER NOT NULL CHECK (max_llm_calls > 0),
    max_tool_calls INTEGER NOT NULL CHECK (max_tool_calls >= 0),
    max_tokens_per_invocation BIGINT NOT NULL CHECK (max_tokens_per_invocation > 0),
    max_duration_ms INTEGER NOT NULL CHECK (max_duration_ms > 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, object_id),
    FOREIGN KEY (tenant_id, object_id)
        REFERENCES objects (tenant_id, object_id)
);

CREATE TABLE budget_reservations (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    invocation_id UUID NOT NULL,
    pool TEXT NOT NULL CHECK (pool IN ('anonymous', 'authenticated')),
    usage_date DATE NOT NULL,
    reserved_tokens BIGINT NOT NULL CHECK (reserved_tokens >= 0),
    reserved_cost_micros BIGINT NOT NULL CHECK (reserved_cost_micros >= 0),
    actual_tokens BIGINT CHECK (actual_tokens >= 0),
    actual_cost_micros BIGINT CHECK (actual_cost_micros >= 0),
    status TEXT NOT NULL CHECK (status IN ('Reserved', 'Settled', 'Released', 'Expired')),
    expires_at TIMESTAMPTZ NOT NULL,
    UNIQUE (tenant_id, object_id, invocation_id)
);

-- The runtime migration chain does not yet create object_invocations.
-- The canonical qembed-sql schema defines its deferred FK to that table;
-- this partial store migration leaves the constraint to the migration that
-- introduces object_invocations into this migration chain.

CREATE INDEX idx_resv_sweep
    ON budget_reservations (expires_at)
    WHERE status = 'Reserved';

CREATE TABLE usage_counters (
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    usage_date DATE NOT NULL,
    pool TEXT NOT NULL CHECK (pool IN ('anonymous', 'authenticated')),
    consumed_tokens BIGINT NOT NULL DEFAULT 0 CHECK (consumed_tokens >= 0),
    consumed_cost_micros BIGINT NOT NULL DEFAULT 0 CHECK (consumed_cost_micros >= 0),
    reserved_tokens BIGINT NOT NULL DEFAULT 0 CHECK (reserved_tokens >= 0),
    reserved_cost_micros BIGINT NOT NULL DEFAULT 0 CHECK (reserved_cost_micros >= 0),
    PRIMARY KEY (tenant_id, object_id, usage_date, pool),
    FOREIGN KEY (tenant_id, object_id)
        REFERENCES objects (tenant_id, object_id)
);

CREATE TABLE caller_usage_counters (
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    caller_id UUID NOT NULL,
    usage_date DATE NOT NULL,
    consumed_tokens BIGINT NOT NULL DEFAULT 0 CHECK (consumed_tokens >= 0),
    reserved_tokens BIGINT NOT NULL DEFAULT 0 CHECK (reserved_tokens >= 0),
    PRIMARY KEY (tenant_id, object_id, caller_id, usage_date),
    FOREIGN KEY (tenant_id, object_id)
        REFERENCES objects (tenant_id, object_id),
    FOREIGN KEY (tenant_id, caller_id)
        REFERENCES callers (tenant_id, id) ON DELETE CASCADE
);

-- Runtime access remains tenant-scoped. The database row lock on object_budgets
-- is the serialization point for object-wide daily and concurrency limits.
GRANT SELECT, INSERT, UPDATE ON
    object_budgets, budget_reservations, usage_counters, caller_usage_counters
TO qembed_runtime;

CREATE POLICY object_budgets_runtime_tenant ON object_budgets
    FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
CREATE POLICY budget_reservations_runtime_tenant ON budget_reservations
    FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
CREATE POLICY usage_counters_runtime_tenant ON usage_counters
    FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
CREATE POLICY caller_usage_counters_runtime_tenant ON caller_usage_counters
    FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

ALTER TABLE object_budgets ENABLE ROW LEVEL SECURITY;
ALTER TABLE object_budgets FORCE ROW LEVEL SECURITY;
ALTER TABLE budget_reservations ENABLE ROW LEVEL SECURITY;
ALTER TABLE budget_reservations FORCE ROW LEVEL SECURITY;
ALTER TABLE usage_counters ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_counters FORCE ROW LEVEL SECURITY;
ALTER TABLE caller_usage_counters ENABLE ROW LEVEL SECURITY;
ALTER TABLE caller_usage_counters FORCE ROW LEVEL SECURITY;
