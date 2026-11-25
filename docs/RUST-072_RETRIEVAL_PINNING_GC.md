# RUST-072 — Retrieval pinning and GC protection

## Guarantee

An invocation pins the exact committed Knowledge revision captured by
begin_invocation. The pin is inserted in the same PostgreSQL transaction as
the invocation row, so neither can commit without the other. Revision zero
contains no visible Knowledge data and is not pinned.

## Concurrency protocol

- Pin insertion takes FOR SHARE on the Object's knowledge_revisions row.
- Revision commits already serialize on that row with FOR UPDATE.
- GC also takes FOR UPDATE on that row before selecting documents.
- Therefore, a concurrent invocation cannot commit a new pin after GC has
  checked for pins but before GC deletes historical data. Whichever operation
  obtains the row lock first determines the serial order.
- A pin protects a retired document when
  pin.revision < document.valid_to_revision, matching the half-open
  visibility interval [valid_from_revision, valid_to_revision).

## Pin lifecycle

The pin references both the invocation and the immutable revision log. The
database validates that the revision is committed and matches the active
invocation. Runtime can insert and read pins but cannot delete them. A
definer-owned trigger releases the pin when the invocation transitions to
Completed, CompletedDegraded, Failed, or Cancelled.

This includes normal completion, provider failure and cancellation. A worker
lease expiry or reclaim does not release the pin: the invocation remains
nonterminal and may resume against the same revision.

## Garbage collection

PgKnowledgeGarbageCollector calls a tenant-checked definer function. It
collects only retired documents whose end revision is committed and for
which no active pin can observe the document. It uses bounded batches of
1–500 rows and SKIP LOCKED for document-level contention. Deleting a
document cascades to its chunks and embeddings. The append-only revision log
is never pruned by this operation.

## Verification

The PostgreSQL integration test creates an active invocation pin, proves that
GC retains the retired document, transitions the invocation to a terminal
state, verifies automatic pin release, and then proves that GC deletes the
document and cascaded chunks. The begin-invocation integration test separately
asserts that invocation creation persists the exact captured revision.
