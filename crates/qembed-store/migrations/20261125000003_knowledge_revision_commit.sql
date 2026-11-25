-- RUST-066: persistence contract for revisioned Knowledge.
--
-- This is the Rust Store slice of the canonical qembed-sql Knowledge schema.
-- Embedding model management remains a separate RUST-068 operation; this table
-- exposes the canonical active-model registry needed to validate commit readiness.

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE embedding_models (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    model_name TEXT NOT NULL,
    dimension INTEGER NOT NULL CHECK (dimension > 0),
    distance_metric TEXT NOT NULL CHECK (distance_metric IN ('cosine', 'l2', 'ip')),
    active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE knowledge_revision_log (
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    committed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, object_id, revision),
    FOREIGN KEY (tenant_id, object_id)
        REFERENCES objects (tenant_id, object_id)
);

CREATE TABLE knowledge_documents (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    doc_key TEXT NOT NULL,
    title TEXT NOT NULL,
    source_uri TEXT,
    trusted BOOLEAN NOT NULL DEFAULT false,
    status TEXT NOT NULL CHECK (status IN ('staged', 'indexed', 'live', 'retired', 'erased')),
    valid_from_revision BIGINT,
    valid_to_revision BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (valid_to_revision IS NULL OR
        (valid_from_revision IS NOT NULL AND valid_to_revision > valid_from_revision)),
    CHECK (status NOT IN ('staged', 'indexed') OR valid_from_revision IS NULL),
    CHECK (status <> 'live' OR
        (valid_from_revision IS NOT NULL AND valid_to_revision IS NULL)),
    CHECK (status <> 'retired' OR valid_to_revision IS NOT NULL),
    UNIQUE (tenant_id, object_id, id),
    FOREIGN KEY (tenant_id, object_id)
        REFERENCES objects (tenant_id, object_id)
);

CREATE UNIQUE INDEX uq_doc_live
    ON knowledge_documents (tenant_id, object_id, doc_key)
    WHERE valid_from_revision IS NOT NULL AND valid_to_revision IS NULL;

CREATE TABLE knowledge_chunks (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    document_id UUID NOT NULL,
    content TEXT NOT NULL,
    erased BOOLEAN NOT NULL DEFAULT false,
    position INTEGER NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}',
    valid_from_revision BIGINT,
    valid_to_revision BIGINT,
    content_tsv TSVECTOR GENERATED ALWAYS AS (to_tsvector('simple', content)) STORED,
    CHECK (valid_to_revision IS NULL OR
        (valid_from_revision IS NOT NULL AND valid_to_revision > valid_from_revision)),
    CHECK (NOT erased OR content = ''),
    UNIQUE (tenant_id, object_id, id),
    FOREIGN KEY (tenant_id, object_id, document_id)
        REFERENCES knowledge_documents (tenant_id, object_id, id) ON DELETE CASCADE
);

CREATE INDEX idx_chunks_visible
    ON knowledge_chunks (tenant_id, object_id, valid_from_revision, valid_to_revision);
CREATE INDEX idx_chunks_document
    ON knowledge_chunks (tenant_id, object_id, document_id);
CREATE INDEX idx_chunks_fts ON knowledge_chunks USING gin (content_tsv);

CREATE FUNCTION knowledge_chunks_guard() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $knowledge_chunks_guard$
BEGIN
    IF (NEW.id, NEW.tenant_id, NEW.object_id, NEW.document_id, NEW.position, NEW.metadata)
       IS DISTINCT FROM
       (OLD.id, OLD.tenant_id, OLD.object_id, OLD.document_id, OLD.position, OLD.metadata)
    THEN
        RAISE EXCEPTION 'chunk is immutable';
    END IF;
    IF NEW.content IS DISTINCT FROM OLD.content AND NOT (NEW.erased AND NOT OLD.erased)
    THEN
        RAISE EXCEPTION 'chunk content is immutable (only erasure allowed)';
    END IF;
    IF (NEW.valid_from_revision, NEW.valid_to_revision)
       IS DISTINCT FROM (OLD.valid_from_revision, OLD.valid_to_revision)
       AND current_user <> 'qembed_definer'
    THEN
        RAISE EXCEPTION 'chunk interval is changed only by the trusted revision-sync trigger';
    END IF;
    RETURN NEW;
END
$knowledge_chunks_guard$;

CREATE TRIGGER trg_chunks_guard
    BEFORE UPDATE ON knowledge_chunks
    FOR EACH ROW EXECUTE FUNCTION knowledge_chunks_guard();

CREATE FUNCTION knowledge_documents_sync() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp AS $knowledge_documents_sync$
BEGIN
    UPDATE knowledge_chunks
       SET valid_from_revision = NEW.valid_from_revision,
           valid_to_revision = NEW.valid_to_revision
     WHERE tenant_id = NEW.tenant_id
       AND object_id = NEW.object_id
       AND document_id = NEW.id;
    RETURN NEW;
END
$knowledge_documents_sync$;

CREATE TRIGGER trg_docs_sync
    AFTER UPDATE OF valid_from_revision, valid_to_revision ON knowledge_documents
    FOR EACH ROW EXECUTE FUNCTION knowledge_documents_sync();

CREATE TABLE knowledge_chunk_embeddings (
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    chunk_id UUID NOT NULL,
    model_id TEXT NOT NULL REFERENCES embedding_models(id),
    embedding VECTOR NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, chunk_id, model_id),
    FOREIGN KEY (tenant_id, object_id, chunk_id)
        REFERENCES knowledge_chunks (tenant_id, object_id, id) ON DELETE CASCADE
);

CREATE FUNCTION check_embedding_dim() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $check_embedding_dim$
DECLARE
    expected_dimension INTEGER;
BEGIN
    SELECT dimension INTO expected_dimension
      FROM embedding_models
     WHERE id = NEW.model_id;

    IF expected_dimension IS NULL OR vector_dims(NEW.embedding) <> expected_dimension THEN
        RAISE EXCEPTION 'embedding dimension mismatch for model %', NEW.model_id;
    END IF;
    RETURN NEW;
END
$check_embedding_dim$;

CREATE TRIGGER trg_kce_dim
    BEFORE INSERT OR UPDATE OF embedding, model_id
    ON knowledge_chunk_embeddings
    FOR EACH ROW EXECUTE FUNCTION check_embedding_dim();

CREATE FUNCTION reject_knowledge_revision_log_mutation() RETURNS trigger
LANGUAGE plpgsql AS $reject_knowledge_revision_log_mutation$
BEGIN
    RAISE EXCEPTION 'knowledge revision log is append-only'
        USING ERRCODE = '55000';
END
$reject_knowledge_revision_log_mutation$;

CREATE TRIGGER trg_knowledge_revision_log_append_only
    BEFORE UPDATE OR DELETE ON knowledge_revision_log
    FOR EACH ROW EXECUTE FUNCTION reject_knowledge_revision_log_mutation();

GRANT CREATE ON SCHEMA public TO qembed_definer;\nALTER FUNCTION knowledge_documents_sync() OWNER TO qembed_definer;\nREVOKE CREATE ON SCHEMA public FROM qembed_definer;
REVOKE ALL ON FUNCTION knowledge_documents_sync() FROM PUBLIC;
GRANT SELECT, UPDATE ON knowledge_chunks TO qembed_definer;
CREATE POLICY knowledge_chunks_definer_access ON knowledge_chunks
    FOR ALL TO qembed_definer USING (true) WITH CHECK (true);

DO $knowledge_rls$
DECLARE
    table_name TEXT;
BEGIN
    FOREACH table_name IN ARRAY ARRAY[
        'knowledge_revision_log',
        'knowledge_documents',
        'knowledge_chunks',
        'knowledge_chunk_embeddings'
    ] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', table_name);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', table_name);
        EXECUTE format(
            'CREATE POLICY knowledge_runtime_tenant ON %I TO qembed_runtime
             USING (tenant_id = NULLIF(current_setting(''app.tenant_id'', true), '''')::uuid)
             WITH CHECK (tenant_id = NULLIF(current_setting(''app.tenant_id'', true), '''')::uuid)',
            table_name
        );
        EXECUTE format(
            'CREATE POLICY knowledge_admin_tenant ON %I FOR SELECT TO qembed_admin
             USING (tenant_id = NULLIF(current_setting(''app.tenant_id'', true), '''')::uuid)',
            table_name
        );
    END LOOP;
END
$knowledge_rls$;

GRANT SELECT ON embedding_models TO qembed_runtime, qembed_admin;
GRANT SELECT, INSERT ON knowledge_revision_log TO qembed_runtime;
GRANT SELECT, INSERT, UPDATE ON knowledge_documents, knowledge_chunks TO qembed_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON knowledge_chunk_embeddings TO qembed_runtime;
GRANT SELECT ON knowledge_revision_log, knowledge_documents, knowledge_chunks,
    knowledge_chunk_embeddings TO qembed_admin;
