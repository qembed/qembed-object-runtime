-- RUST-061: durable retryable Caller deletion jobs.
-- The authoritative tombstones live only in the independent Control-plane DB.
CREATE TABLE deletion_jobs (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    caller_id UUID NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('Pending', 'Running', 'Done', 'Failed')),
    progress JSONB NOT NULL DEFAULT '{}'::jsonb,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ,
    FOREIGN KEY (tenant_id, caller_id) REFERENCES callers (tenant_id, id)
);

CREATE UNIQUE INDEX uq_deletion_active
    ON deletion_jobs (tenant_id, caller_id)
    WHERE status IN ('Pending', 'Running');

ALTER TABLE deletion_jobs ENABLE ROW LEVEL SECURITY;
ALTER TABLE deletion_jobs FORCE ROW LEVEL SECURITY;

CREATE POLICY deletion_jobs_runtime_tenant
    ON deletion_jobs
    FOR ALL TO qembed_runtime
    USING (tenant_id = (SELECT qembed_crypto.current_tenant_id()))
    WITH CHECK (tenant_id = (SELECT qembed_crypto.current_tenant_id()));

GRANT SELECT, INSERT, UPDATE ON deletion_jobs TO qembed_runtime;
GRANT SELECT ON deletion_jobs TO qembed_admin;
