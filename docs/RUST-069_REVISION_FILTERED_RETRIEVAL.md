# RUST-069 — Revision-filtered Knowledge retrieval

## Responsibility boundary

`PgKnowledgeRetrievalStore` accepts a trusted tenant `Scope` and an immutable `PinnedKnowledgeRevision`. It rejects cross-tenant and future pins, then loads the complete set of chunks visible at that exact revision.

The Store migration adds `knowledge_visible_chunks_at(tenant, object, revision)` as an invoker-security SQL function. It joins chunk and document revision intervals, checks the committed revision pointer, excludes staged/indexed/erased documents and erased chunks, and leaves PostgreSQL RLS enabled for the caller.

## Required query-plan order

The function returns candidates only; it deliberately has no top-k parameter. RUST-070 must consume it through a materialized candidate CTE and only then join model embeddings, calculate the pgvector distance and apply `ORDER BY ... LIMIT`:

```sql
WITH visible_candidates AS MATERIALIZED (
    SELECT candidate.*, embedding.model_id, embedding.embedding
    FROM knowledge_visible_chunks_at($tenant, $object, $pinned_revision) AS candidate
    JOIN knowledge_chunk_embeddings AS embedding
      ON embedding.tenant_id = candidate.tenant_id
     AND embedding.object_id = candidate.object_id
     AND embedding.chunk_id = candidate.id
    WHERE embedding.model_id = $model_id
)
SELECT ...
FROM visible_candidates
ORDER BY embedding <=> $query_vector
LIMIT $top_k;
```

This materialization must include the embedding join, not only the chunk IDs. Otherwise PostgreSQL may scan an HNSW index in distance order and apply the visibility join as a post-filter, allowing invisible rows to consume top-k slots. Materializing the visible chunk+embedding rows first makes the outer distance sort operate only on eligible candidates.

## Verification

- PostgreSQL integration covers staged, indexed, future, closed, live, retired and erased rows at revisions 1 and 2.
- A regression control proves that unfiltered top-1 selects a future chunk and then yields no result after visibility filtering; the materialized pre-filtered query returns the best visible chunk.
- Cross-tenant and future revision pins fail closed.
