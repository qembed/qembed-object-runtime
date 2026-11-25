-- RUST-080: persist monotonic Object-to-Tool binding identity and reject stale claims.
-- The per-Object counter is shared by all Tool bindings and never reuses a version.
ALTER TABLE objects
    ADD COLUMN binding_counter BIGINT NOT NULL DEFAULT 0
        CHECK (binding_counter >= 0);

CREATE TABLE object_tools (
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    tool_id UUID NOT NULL,
    tool_version_id UUID NOT NULL,
    permissions JSONB NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT true,
    binding_version BIGINT NOT NULL DEFAULT 1 CHECK (binding_version > 0),
    PRIMARY KEY (tenant_id, object_id, tool_id),
    FOREIGN KEY (tenant_id, object_id)
        REFERENCES objects (tenant_id, object_id),
    FOREIGN KEY (tool_id, tool_version_id)
        REFERENCES tool_versions (tool_id, id)
);

CREATE FUNCTION object_tools_bump_binding_version() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
DECLARE next_version BIGINT;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF (NEW.tenant_id, NEW.object_id, NEW.tool_id)
           IS DISTINCT FROM (OLD.tenant_id, OLD.object_id, OLD.tool_id) THEN
            RAISE EXCEPTION 'Object Tool binding identity is immutable'
                USING ERRCODE = '55000';
        END IF;
        IF (NEW.tool_version_id, NEW.permissions, NEW.enabled)
           IS NOT DISTINCT FROM (OLD.tool_version_id, OLD.permissions, OLD.enabled) THEN
            NEW.binding_version := OLD.binding_version;
            RETURN NEW;
        END IF;
    END IF;

    UPDATE objects
       SET binding_counter = binding_counter + 1
     WHERE tenant_id = NEW.tenant_id AND object_id = NEW.object_id
     RETURNING binding_counter INTO next_version;

    IF next_version IS NULL THEN
        RAISE EXCEPTION 'Object not found for Tool binding';
    END IF;

    NEW.binding_version := next_version;
    RETURN NEW;
END;
$$;

CREATE TRIGGER object_tools_binding_version
BEFORE INSERT OR UPDATE ON object_tools
FOR EACH ROW EXECUTE FUNCTION object_tools_bump_binding_version();

CREATE FUNCTION reject_object_tool_binding_delete() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    RAISE EXCEPTION 'Object Tool bindings cannot be deleted; disable them instead'
        USING ERRCODE = '55000';
END;
$$;

CREATE TRIGGER object_tools_no_delete
BEFORE DELETE OR TRUNCATE ON object_tools
FOR EACH STATEMENT EXECUTE FUNCTION reject_object_tool_binding_delete();

ALTER TABLE object_tools ENABLE ROW LEVEL SECURITY;
ALTER TABLE object_tools FORCE ROW LEVEL SECURITY;

CREATE POLICY object_tools_runtime_tenant ON object_tools
    FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

CREATE POLICY object_tools_admin_tenant ON object_tools
    FOR SELECT TO qembed_admin
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

GRANT SELECT, INSERT, UPDATE ON object_tools TO qembed_runtime;
GRANT SELECT ON object_tools TO qembed_admin;
