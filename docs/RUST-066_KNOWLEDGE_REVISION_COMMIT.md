# RUST-066 — Atomic Knowledge revision commit

## Responsibility boundary

The domain crate defines revision numbers, document lifecycle and half-open visibility intervals. The Store crate owns PostgreSQL locking, index-readiness checks, interval persistence and the transaction that makes a revision visible. The repository accepts a trusted Scope; tenant identity is never read from a request DTO.

## Commit contract

`PgKnowledgeRepository::commit_knowledge_revision(scope, object_id)` commits every document for the Object whose persisted status is `indexed`. It returns the previous and new revision plus the document IDs published and retired.

- The repository locks the Object's `knowledge_revisions` row with `FOR UPDATE`. This is the serialization point for concurrent commits on one Object.
- At least one `embedding_models.active = true` model must exist. The repository takes a table-level `SHARE` lock on `embedding_models` so the active model set cannot change during readiness validation.
- Each indexed document must have at least one chunk. Every chunk must have one embedding for every active model. The embedding primary key prevents duplicates. Chunks and existing embedding rows are locked while this is checked, preventing concurrent ingestion/index mutation from invalidating the result.
- Two indexed documents with the same `doc_key` are rejected. For each key being published, the currently live document is retired first, closing its interval at the new revision. Indexed replacements are then made live and their intervals open at that revision.
- The database's `knowledge_documents_sync` trigger copies document interval changes to every child chunk. The trusted definer-owned trigger is the only path allowed to mutate chunk visibility intervals.
- `knowledge_revisions.current_revision` is advanced and `knowledge_revision_log` receives the immutable commit record.
- `knowledge.revision_committed` is appended to `events` and mirrored to `outbox_messages` in the same transaction. Event payload fields are allowlisted: Object ID, previous/new revision, and published/retired document counts. Delivery remains at-least-once; consumers deduplicate by event/delivery ID.

Any readiness, persistence, event or outbox failure aborts the transaction. No partial interval, revision pointer, log row or event is visible.

## Active embedding-model interpretation

For RUST-066, “required models” means **all rows with `embedding_models.active = true` at commit time**. This interpretation was explicitly confirmed during implementation. Inactive models do not block a commit. An empty active model registry fails closed.

RUST-068 will own model-registry lifecycle operations. Its mutations must remain compatible with the commit's table lock.

## Non-goals

- Embedding generation and staged ingestion are RUST-067 responsibilities; no provider/network call occurs inside this SQL transaction.
- Model registration and activation APIs are RUST-068.
- Retrieval filtering, query planning, revision pinning and garbage collection remain later RUST-069…072 work.
- This migration adds only the Store persistence slice required by the current Rust repository; qembed-sql remains the canonical full production schema.
