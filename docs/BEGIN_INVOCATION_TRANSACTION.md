# Atomic begin-invocation transaction

RUST-036 composes the SQL-011 delegation resolver and RUST-035 idempotency primitive into one PostgreSQL transaction.

## Inputs

`BeginInvocationCommand` contains the object, explicitly requested Caller ID, explicit InvocationPool, pinned Snapshot ID, idempotency key, opaque payload hash, caller-supplied absolute expiry, trace ID, original user input, and an explicit interaction-message retention deadline. The caller ID is untrusted selection input until SQL-011 resolves it. The pool is an independent trusted policy decision; Caller kind is not used to derive it.

Payload canonicalization/hash algorithm and expiry policy remain upstream contracts. This Store layer does not invent them.

## Transaction order

1. Begin a tenant transaction and set transaction-local `app.tenant_id` and `app.principal_id` from trusted Scope.
2. Resolve the requested Caller with SQL-011 `resolve_delegated_caller`. The function checks the exact active delegation and locks delegation, Principal, and Caller rows `FOR SHARE`.
3. Claim the idempotency key using the resolved Caller. The claim is the first invocation write. A matching committed key returns Replay; a mismatched payload returns conflict.
4. Lock the Object row `FOR SHARE` and require Active state.
5. Verify the requested Snapshot belongs to the tenant/object and points to the Object's current Published version.
6. Read and lock the object's current `knowledge_revisions.current_revision` `FOR SHARE`.
7. Validate the snapshot's positive integer `limits.max_tokens_per_invocation` and estimate input tokens from UTF-8 bytes.
8. Lock `object_budgets` `FOR UPDATE`; enforce the effective snapshot/live invocation cap, aggregate object UTC-day token limit, caller UTC-day token limit, and active object concurrency limit. Increment the pool-attributed object counter and caller counter, then insert the `Reserved` budget row.
9. Insert `object_invocations` with the resolved Caller, selected snapshot, pinned revision, explicit pool, Received state, and trace ID. The canonical schema's reservation-to-invocation FK is deferred to transaction commit.
10. Insert exactly one `user` row into `interaction_messages` using the same effective Caller and invocation ID. Its expiry is supplied separately from idempotency-key expiry.
11. Append the allowlisted `invocation.received` event and its outbox delivery row in the same transaction.
12. Commit. Only then may the application call a ChatProvider or perform other external I/O.

The SQL invocation-idempotency FK is DEFERRABLE INITIALLY DEFERRED. The canonical budget-reservation FK is also DEFERRABLE INITIALLY DEFERRED, permitting the reservation to be inserted before its invocation row while requiring both to exist at commit. The current partial Rust store migration chain documents the latter constraint but defers installing it until the migration that introduces `object_invocations`; see [Token budget reservation](TOKEN_BUDGET_RESERVATION.md).

## Atomicity and replay

Any failure after the claim causes transaction rollback: no idempotency reservation, invocation, input message, event, or outbox row survives. The integration suite injects failures at snapshot validation, invocation insert, event insert, and outbox insert.

A matching idempotency replay commits the read/lock transaction and returns the original InvocationId without creating another invocation, budget reservation, counter increment, input message, event, or outbox message. A different payload hash for the same effective Caller/key is a conflict. A different Caller has an independent key namespace.

## Locking and external side effects

The delegation locks are held until commit, ordering invocation start against revocation/disablement. Object and knowledge-revision locks pin the published version and current revision against concurrent updates. The Store contains no provider calls and returns only after commit, so network latency never extends the database transaction. After the provider adapter returns, the worker settles normalized usage in a separate tenant-scoped transaction via [Provider usage settlement](PROVIDER_USAGE_SETTLEMENT.md); that transaction replaces the estimate and never holds locks across provider I/O.
