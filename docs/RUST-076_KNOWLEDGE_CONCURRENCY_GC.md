# RUST-076 — Knowledge concurrency and GC verification

RUST-076 consolidates PostgreSQL integration coverage for the concurrency and
lifecycle invariants already implemented by the Knowledge revision, invocation,
and garbage-collection paths. It adds an explicit race between two GC workers.

## Coverage matrix

- Commit race: knowledge_revision_commit.rs —
  concurrent_commits_serialize_on_revision_row_and_publish_once proves one
  concurrent commit wins and the other observes no indexed documents.
- Pinned invocation: begin_invocation.rs —
  begin_invocation_commits_all_rows_and_replays_by_effective_caller asserts the
  invocation persists the exact captured Knowledge revision pin.
- Pin-protected GC: knowledge_gc.rs —
  active_invocation_pin_blocks_gc_until_terminal_transition_releases_it proves
  a live pin retains historical content and terminal transition releases it.
- GC race: knowledge_gc.rs —
  concurrent_gc_collectors_delete_a_retired_document_exactly_once runs two
  collectors against the same retired document and proves their combined
  deletion count is exactly one. Cascaded chunks are removed and the immutable
  revision log is retained.
- Rollback: knowledge_revision_commit.rs —
  outbox_failure_rolls_back_revision_intervals_and_revision_log proves a
  failed outbox append rolls back the revision pointer, intervals, log, and
  event publication.
- Repeated publication: atomic_draft_snapshot.rs —
  identical_content_reuses_existing_version_and_snapshot_without_new_rows and
  concurrent_identical_content_has_one_creator_and_one_reuser prove identical
  content is reused and concurrent creators do not duplicate snapshots.

## Execution

All listed tests are regular qembed-store PostgreSQL integration tests and run
through the existing workspace CI PostgreSQL harness. No production SQL or
runtime behavior is changed by this test-suite task.
