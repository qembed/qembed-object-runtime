-- RUST-072: durable invocation pins protect historical Knowledge revisions from GC.
--
-- A pin is created in the same transaction as its invocation. Pin insertion
-- takes FOR SHARE on the per-Object revision row; GC takes FOR UPDATE on that
-- row before deleting anything. This is the serialization boundary that
-- prevents a concurrent invocation from losing its revision during GC.

CREATE TABLE knowledge_revision_pins (
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    invocation_id UUID NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, object_id, invocation_id),
    FOREIGN KEY (tenant_id, object_id, invocation_id)
        REFERENCES object_invocations (tenant_id, object_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, object_id, revision)
        REFERENCES knowledge_revision_log (tenant_id, object_id, revision)
);

CREATE INDEX idx_knowledge_revision_pins_revision
    ON knowledge_revision_pins (tenant_id, object_id, revision);

ALTER TABLE knowledge_revision_pins ENABLE ROW LEVEL SECURITY;
ALTER TABLE knowledge_revision_pins FORCE ROW LEVEL SECURITY;

CREATE POLICY knowledge_revision_pins_runtime_tenant
    ON knowledge_revision_pins FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
CREATE POLICY knowledge_revision_pins_admin_select
    ON knowledge_revision_pins FOR SELECT TO qembed_admin
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
CREATE POLICY knowledge_revision_pins_definer_access
    ON knowledge_revision_pins TO qembed_definer
    USING (true) WITH CHECK (true);
CREATE POLICY knowledge_revisions_definer_access
    ON knowledge_revisions TO qembed_definer
    USING (true) WITH CHECK (true);
CREATE POLICY knowledge_documents_definer_access
    ON knowledge_documents TO qembed_definer
    USING (true) WITH CHECK (true);

GRANT SELECT, INSERT ON knowledge_revision_pins TO qembed_runtime;
GRANT SELECT ON knowledge_revision_pins TO qembed_admin;
GRANT SELECT, DELETE ON knowledge_revision_pins TO qembed_definer;
GRANT SELECT, DELETE ON knowledge_documents TO qembed_definer;
GRANT SELECT, UPDATE ON knowledge_revisions TO qembed_definer;
GRANT SELECT ON knowledge_revision_log TO qembed_definer;

-- Enforce the lock protocol even if a future caller inserts pins outside
-- begin_invocation. A pin is valid only for a committed revision belonging to
-- the same still-active invocation.
CREATE FUNCTION knowledge_revision_pin_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $knowledge_revision_pin_guard$
DECLARE
    v_current_revision BIGINT;
BEGIN
    SELECT current_revision
      INTO v_current_revision
      FROM knowledge_revisions
     WHERE tenant_id = NEW.tenant_id
       AND object_id = NEW.object_id
     FOR SHARE;

    IF NOT FOUND OR NEW.revision > v_current_revision THEN
        RAISE EXCEPTION 'knowledge revision pin is not committed for this Object'
            USING ERRCODE = '23514';
    END IF;

    PERFORM 1
      FROM object_invocations
     WHERE tenant_id = NEW.tenant_id
       AND object_id = NEW.object_id
       AND id = NEW.invocation_id
       AND knowledge_revision = NEW.revision
       AND status NOT IN ('Completed', 'CompletedDegraded', 'Failed', 'Cancelled');

    IF NOT FOUND THEN
        RAISE EXCEPTION 'knowledge revision pin must belong to an active matching invocation'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END
$knowledge_revision_pin_guard$;

CREATE TRIGGER trg_knowledge_revision_pin_guard
    BEFORE INSERT ON knowledge_revision_pins
    FOR EACH ROW EXECUTE FUNCTION knowledge_revision_pin_guard();

-- Runtime cannot delete pins directly. Every terminal invocation releases its
-- pin through this narrowly scoped definer-owned trigger.
CREATE FUNCTION release_terminal_invocation_knowledge_pin() RETURNS trigger
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp
AS $release_terminal_invocation_knowledge_pin$
BEGIN
    IF NEW.status IN ('Completed', 'CompletedDegraded', 'Failed', 'Cancelled') THEN
        DELETE FROM knowledge_revision_pins
         WHERE tenant_id = NEW.tenant_id
           AND object_id = NEW.object_id
           AND invocation_id = NEW.id;
    END IF;
    RETURN NEW;
END
$release_terminal_invocation_knowledge_pin$;

ALTER FUNCTION release_terminal_invocation_knowledge_pin() OWNER TO qembed_definer;
REVOKE ALL ON FUNCTION release_terminal_invocation_knowledge_pin() FROM PUBLIC;

CREATE TRIGGER trg_release_terminal_invocation_knowledge_pin
    AFTER UPDATE OF status ON object_invocations
    FOR EACH ROW
    WHEN (NEW.status IN ('Completed', 'CompletedDegraded', 'Failed', 'Cancelled'))
    EXECUTE FUNCTION release_terminal_invocation_knowledge_pin();

-- Delete only retired documents that cannot be observed by any active pin.
-- The revision-row FOR UPDATE lock serializes GC with both pin creation and
-- Knowledge revision commits. Chunk and embedding rows are removed by their
-- existing ON DELETE CASCADE relationships; the append-only revision log stays.
CREATE FUNCTION collect_retired_knowledge_documents(
    p_tenant_id UUID,
    p_object_id UUID,
    p_batch_size INTEGER
)
RETURNS INTEGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp
AS $collect_retired_knowledge_documents$
DECLARE
    v_current_revision BIGINT;
    v_deleted INTEGER;
    v_context_tenant UUID := NULLIF(current_setting('app.tenant_id', true), '')::uuid;
BEGIN
    IF v_context_tenant IS NULL OR v_context_tenant <> p_tenant_id THEN
        RAISE EXCEPTION 'Knowledge GC tenant differs from trusted transaction context'
            USING ERRCODE = '42501';
    END IF;
    IF p_batch_size IS NULL OR p_batch_size < 1 OR p_batch_size > 500 THEN
        RAISE EXCEPTION 'Knowledge GC batch size must be between 1 and 500'
            USING ERRCODE = '22023';
    END IF;

    SELECT current_revision
      INTO v_current_revision
      FROM knowledge_revisions
     WHERE tenant_id = p_tenant_id
       AND object_id = p_object_id
     FOR UPDATE;

    IF NOT FOUND THEN
        RETURN 0;
    END IF;

    WITH eligible AS MATERIALIZED (
        SELECT document.id
          FROM knowledge_documents AS document
         WHERE document.tenant_id = p_tenant_id
           AND document.object_id = p_object_id
           AND document.status = 'retired'
           AND document.valid_to_revision <= v_current_revision
           AND NOT EXISTS (
               SELECT 1
                 FROM knowledge_revision_pins AS pin
                WHERE pin.tenant_id = document.tenant_id
                  AND pin.object_id = document.object_id
                  AND pin.revision < document.valid_to_revision
           )
         ORDER BY document.valid_to_revision, document.id
         FOR UPDATE OF document SKIP LOCKED
         LIMIT p_batch_size
    ),
    deleted AS (
        DELETE FROM knowledge_documents AS document
         USING eligible
         WHERE document.tenant_id = p_tenant_id
           AND document.object_id = p_object_id
           AND document.id = eligible.id
        RETURNING 1
    )
    SELECT count(*)::integer INTO v_deleted FROM deleted;

    RETURN v_deleted;
END
$collect_retired_knowledge_documents$;

ALTER FUNCTION collect_retired_knowledge_documents(UUID, UUID, INTEGER) OWNER TO qembed_definer;
REVOKE ALL ON FUNCTION collect_retired_knowledge_documents(UUID, UUID, INTEGER) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION collect_retired_knowledge_documents(UUID, UUID, INTEGER) TO qembed_runtime;
