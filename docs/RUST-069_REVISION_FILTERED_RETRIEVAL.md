# RUST-069 — Revision-filtered Knowledge retrieval

## Responsibility boundary

`PgKnowledgeRetrievalStore` accepts a trusted tenant `Scope` and an immutable `PinnedKnowledgeRevision`. It rejects cross-tenant and future pins, then loads the complete set of chunks visible at that exact revision.

The Store migration adds `knowledge_visible_chunks_at(tenant, object, revision)` as an invoker-security SQL function. It joins chunk and document revision intervals, checks the committed revision pointer, excludes staged/indexed/erased documents and erased chunks, and leaves PostgreSQL RLS enabled for the caller.

## Required query-plan order

The function returns candidates only; it deliberately has no top-k parameter. RUST-070 must consume it through a materialized candidate CTE and only then join model embeddings, calculate the pgvector distance and apply `ORDER BY ... LIMIT`:

```sql
WITH visible_candidates AS MATERIALIZED (
    SELECT * FROM knowledge_visible_chunks_at($tenant, $object, $pinned_revision)
)
SELECT ...
FROM visible_candidates AS candidate
JOIN knowledge_chunk_embeddings AS embedding ON ...
ORDER BY embedding.embedding <=> $query_vector
LIMIT $top_k;
```

This materialization is intentional: filtering after an approximate or exact top-k can allow a future or otherwise invisible chunk to consume a result slot.

## Verification

- PostgreSQL integration covers staged, indexed, future, closed, live, retired and erased rows at revisions 1 and 2.
- A regression control proves that unfiltered top-1 selects a future chunk and then yields no result after visibility filtering; the materialized pre-filtered query returns the best visible chunk.
- Cross-tenant and future revision pins fail closed.
