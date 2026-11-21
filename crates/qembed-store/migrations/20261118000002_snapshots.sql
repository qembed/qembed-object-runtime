-- Immutable tenant-owned snapshots. canonical_json is the source of truth;
-- JSONB is a derived projection used only for querying.
CREATE TABLE snapshots (
    tenant_id UUID NOT NULL,
    snapshot_id UUID NOT NULL,
    object_id UUID NOT NULL,
    object_version_id UUID NOT NULL,
    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
    canonical_json BYTEA NOT NULL,
    content_hash BYTEA NOT NULL CHECK (octet_length(content_hash) = 32),
    json_projection JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, snapshot_id),
    FOREIGN KEY (tenant_id, object_id, object_version_id)
        REFERENCES object_versions (tenant_id, object_id, object_version_id)
        ON DELETE RESTRICT
);

CREATE FUNCTION enforce_snapshot_projection() RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.json_projection IS DISTINCT FROM
        convert_from(NEW.canonical_json, 'UTF8')::jsonb
    THEN
        RAISE EXCEPTION 'snapshot JSONB projection does not match canonical_json'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER snapshots_projection_guard
BEFORE INSERT OR UPDATE ON snapshots
FOR EACH ROW EXECUTE FUNCTION enforce_snapshot_projection();

CREATE FUNCTION reject_snapshot_mutation() RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'snapshots are immutable'
        USING ERRCODE = '55000';
END;
$$;

CREATE TRIGGER snapshots_immutable
BEFORE UPDATE OR DELETE ON snapshots
FOR EACH ROW EXECUTE FUNCTION reject_snapshot_mutation();
