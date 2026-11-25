-- RUST-079: enforce the canonical immutable ToolVersion contract.
-- tool_definitions and tool_versions are created by RUST-053
-- (20261122000001_tool_execution_lifecycle.sql); do not recreate them here.

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
