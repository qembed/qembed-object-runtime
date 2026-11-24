# RUST-062 — Reservation sweeper

## Purpose

Release token/cost reservations that have expired only when PostgreSQL has durable evidence that the provider request was not sent or was confirmed not accepted. The sweeper is a global worker operation, not a tenant API.

## Eligibility is deliberately conservative

A reservation is eligible only when all conditions hold:

- `budget_reservations.status = 'Reserved'`;
- `budget_reservations.expires_at < cutoff`;
- a matching `provider_call_attempts` row exists;
- provider attempt state is `NotStarted`, `ConfirmedNotAccepted`, or `CancelledBeforeSend`.

The following are never auto-released:

- `MayHaveBeenSent`;
- `OutcomeUnknown`;
- `ResponseReceived`;
- a missing provider-attempt row.

A possibly accepted provider request must be reconciled or settled. Expiry alone is not proof that the provider did not execute it.

## SQL contract

The canonical SQL contract is `sweep_expired_reservations(p_limit, p_now)`.

- `p_limit` must be between 1 and 500.
- `p_now` is an explicit cutoff; it defaults to PostgreSQL `clock_timestamp()` for ordinary scheduled sweeps.
- The function is `SECURITY DEFINER` and executable only by `qembed_worker`.
- The function locks provider-attempt rows using `FOR UPDATE SKIP LOCKED`. Competing sweeper sessions therefore claim disjoint rows.
- Reservation status and object-pool/per-caller reserved counters are updated in the same database statement transaction.
- The reservation transitions from `Reserved` to `Expired`; subsequent runs cannot release its counters again.
- Counter drift is clamped at zero rather than making one malformed row abort the entire batch.

The upgrade migration removes the previous one-argument function and installs the new signature with a default cutoff. It does not grant the normal `qembed_runtime` role access to this global operation.

## Rust adapter

`qembed_store::reservation_sweeper::PgReservationSweeper` wraps the canonical SQL function.

- It accepts a dedicated worker pool whose login inherits `qembed_worker` (or explicitly assumes that role); the `qembed_worker` group role itself is `NOLOGIN`.
- It intentionally does not accept `Scope`: the SQL function discovers eligible rows across tenants under its restricted worker capability.
- `sweep(batch_size)` uses the database clock.
- `sweep_before(cutoff, batch_size)` supplies a deterministic cutoff for tests/controlled maintenance.
- The adapter validates batch size and persisted result counts before returning.

Provision a separate worker login and grant it membership in the restricted group role, for example:\n\n```sql\nCREATE ROLE qembed_worker_app LOGIN INHERIT PASSWORD '...';\nGRANT qembed_worker TO qembed_worker_app;\n```\n\nThe adapter does not create a scheduler or start a worker process. Scheduling belongs to the future independently scalable worker host.

## Acceptance evidence

- SQL smoke verifies a reservation is retained before the supplied cutoff, expired after the cutoff, and ignored on a repeated run.
- SQL migration smoke verifies the upgrade path, default one-argument call, explicit cutoff, and worker-only execute privilege.
- Rust canonical PostgreSQL integration uses two independent worker pools started concurrently. Exactly one eligible reservation is expired; its object and Caller counters are decremented once; a second `OutcomeUnknown` reservation remains reserved.