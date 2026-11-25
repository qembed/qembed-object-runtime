-- RUST-069: revision-aware candidate filtering for Knowledge retrieval.
--
-- This invoker-security function is the shared visibility boundary consumed by
-- retrieval query plans. Callers must materialize its output before applying
-- vector distance ordering and LIMIT/top-k. RLS remains active for the caller.

CREATE FUNCTION knowledge_visible_chunks_at(
    p_tenant_id UUID,
    p_object_id UUID,
    p_revision BIGINT
)
RETURNS TABLE (
    id UUID,
    tenant_id UUID,
    object_id UUID,
    document_id UUID,
    content TEXT,
    chunk_position INTEGER,
    metadata JSONB
)
LANGUAGE sql
STABLE
SET search_path = pg_catalog, public
AS $knowledge_visible_chunks_at$
    SELECT
        c.id,
        c.tenant_id,
        c.object_id,
        c.document_id,
        c.content,
        c.position AS chunk_position,
        c.metadata
    FROM public.knowledge_chunks AS c
    JOIN public.knowledge_documents AS d
      ON d.tenant_id = c.tenant_id
     AND d.object_id = c.object_id
     AND d.id = c.document_id
    JOIN public.knowledge_revisions AS r
      ON r.tenant_id = c.tenant_id
     AND r.object_id = c.object_id
    WHERE c.tenant_id = p_tenant_id
      AND c.object_id = p_object_id
      AND p_revision > 0
      AND p_revision <= r.current_revision
      AND d.status IN ('live', 'retired')
      AND d.valid_from_revision IS NOT NULL
      AND d.valid_from_revision <= p_revision
      AND (d.valid_to_revision IS NULL OR p_revision < d.valid_to_revision)
      AND NOT c.erased
      AND c.valid_from_revision IS NOT NULL
      AND c.valid_from_revision <= p_revision
      AND (c.valid_to_revision IS NULL OR p_revision < c.valid_to_revision)
$knowledge_visible_chunks_at$;

REVOKE ALL ON FUNCTION knowledge_visible_chunks_at(UUID, UUID, BIGINT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION knowledge_visible_chunks_at(UUID, UUID, BIGINT)
    TO qembed_runtime, qembed_admin;
