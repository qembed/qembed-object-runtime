# RUST-065 — Knowledge domain model

This change introduces domain types only. Persistence, ingestion, embedding and
atomic revision commit remain Store/application responsibilities in RUST-066
and later tasks.

## Identity and pinning

- A revision number is scoped by both `TenantId` and `ObjectId`; a bare
  integer is not a globally meaningful revision reference.
- Revision `0` is the initial state created with an object. Visible intervals
  begin at a positive committed revision.
- `PinnedKnowledgeRevision` is a copyable immutable value suitable for
  capturing the knowledge view used by an invocation. Advancing the current
  `KnowledgeRevision` does not mutate an existing pin.

## Visibility

Revision intervals are half-open: `[valid_from_revision, valid_to_revision)`.
A document/chunk is visible at revision R iff R is at or after the start and
strictly before the optional end. Both document and chunk visibility checks
also require matching tenant and object identity.

## Document lifecycle

`staged → indexed → live → retired`

Erasure is a terminal privacy override from any non-erased state. A document
cannot become live until indexing is complete. Publication assigns an open
revision interval; retirement closes it at a strictly later revision. Retirement
does not erase historical visibility: a retired document remains visible to pins
inside its earlier interval, but not at or after the end revision. Staged/indexed
documents have no visibility interval.

The statuses and interval constraints match the SQL schema:
`staged`, `indexed`, `live`, `retired`, `erased`.

## Chunk invariants

- Chunk identity, tenant, object, document, position and metadata are fixed.
- Content is immutable except that erasure clears it and marks the chunk erased.
- A chunk receives an open visibility interval at publication and closes it
  when the owning document is retired.
- Erased chunks cannot be visible.

## Serialization boundary

The document, chunk and interval deserializers validate lifecycle/interval
invariants. Malformed persisted or wire representations are rejected instead
of manufacturing impossible domain states.

## Explicit non-goals

This task does not add database queries, document ingestion, chunk splitting,
embedding generation, revision commits, or retrieval. Those operations must
be implemented by adapters and services in later roadmap tasks.
