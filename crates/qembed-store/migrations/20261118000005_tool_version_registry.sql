-- RUST-079: global Tool registry and immutable ToolVersion records.
-- ToolVersions are global metadata. Tenant-specific object bindings are a
-- separate concern and are deliberately not consulted when resolving Snapshots.

CREATE TABLE tool_definitions (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE tool_versions (
    id UUID PRIMARY KEY,
    tool_id UUID NOT NULL REFERENCES tool_definitions(id) ON DELETE RESTRICT,
    version TEXT NOT NULL,
    risk_level TEXT NOT NULL CHECK (risk_level IN ('R0', 'R1', 'R2', 'R3', 'R4')),
    input_schema JSONB NOT NULL,
    output_schema JSONB NOT NULL,
    timeout_ms INTEGER NOT NULL CHECK (timeout_ms > 0),
    idempotent BOOLEAN NOT NULL,
    CHECK (risk_level IN ('R0', 'R1') OR idempotent),
    UNIQUE (tool_id, version),
    UNIQUE (tool_id, id)
);

CREATE FUNCTION reject_tool_version_mutation() RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'tool_versions are immutable'
        USING ERRCODE = '55000';
END;
$$;

CREATE TRIGGER tool_versions_immutable
BEFORE UPDATE OR DELETE ON tool_versions
FOR EACH ROW EXECUTE FUNCTION reject_tool_version_mutation();

REVOKE ALL PRIVILEGES ON TABLE tool_definitions, tool_versions
FROM PUBLIC, qembed_runtime, qembed_admin;

-- Runtime may resolve exact immutable versions but cannot register or mutate them.
GRANT SELECT ON tool_definitions, tool_versions TO qembed_runtime;

-- Administrative provisioning is separate from runtime execution.
GRANT SELECT, INSERT, UPDATE ON tool_definitions, tool_versions TO qembed_admin;
