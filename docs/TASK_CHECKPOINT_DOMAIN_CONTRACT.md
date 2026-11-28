# Task and Checkpoint domain contract (RUST-093)

**Status:** approved project clarification and domain contract  
**Decision date:** 2026-11-29  
**Origin:** explicitly approved by the project owner in the RUST-093 architecture discussion. This is a new project decision; it is not reconstructed from or attributed to the unavailable M0 v2.2 specification.

## Aggregate relationship

A **Task is the long-lived parent entity for multiple Invocations**. It represents durable multi-step work that may span worker runs and more than one Invocation.

An **Invocation remains one execution instance/attempt** with its own lifecycle, lease, heartbeat, fencing token, and optimistic version. It is not promoted into a Task and its lifecycle is not replaced by the Task lifecycle. A Task may attach multiple distinct Invocation IDs over time; the same Invocation cannot be attached twice.

This relationship is intentionally one-way at the current domain boundary: Task tracks its child Invocation IDs. The persistence implementation in RUST-094 must enforce tenant ownership and the Task-to-Invocation relationship atomically. This contract does not prescribe a SQL schema or claim that the repository is already implemented.

## Checkpoint meaning and progression

A **Checkpoint is an immutable, serializable record of committed Task progress**, associated with both its parent Task and the Invocation that produced it. Its state is opaque JSON at this stage; the domain does not interpret application-specific checkpoint payload fields.

- Checkpoint sequence starts at 1 and advances by exactly one for each accepted checkpoint.
- Step index starts at 0 and advances by exactly one.
- A checkpoint can be accepted only while its Task is Running.
- Its Task ID must match the aggregate, and its Invocation must already be attached to that Task.
- Repeated steps, skipped sequence numbers, foreign Tasks, and unattached Invocations are rejected.
- Progress is Task-wide, not reset when a new child Invocation is attached. A later Invocation resumes at the next uncommitted step; it cannot confirm an already-confirmed step again.

The aggregate version advances on each accepted status change, Invocation attachment, and checkpoint. The persistence layer must use compare-and-set/transactional concurrency control so two workers cannot both commit the same next checkpoint.

## Task lifecycle

| Current | Allowed next states |
|---|---|
| Pending | Running, Failed, Cancelled |
| Running | Waiting, Completed, Failed, Cancelled |
| Waiting | Running, Failed, Cancelled |
| Completed / Failed / Cancelled | none |

Terminal Tasks cannot be reopened, receive new Invocation children, or accept further checkpoints.

## Concurrency boundary

Task progress and Invocation execution ownership are separate concerns:

- Invocation leases/fencing protect writes performed by a worker for one Invocation.
- Task-level optimistic version/CAS protects aggregate status, child attachment, and checkpoint sequence.
- A valid Invocation lease does not by itself authorize a Task checkpoint write; the Task's expected version/next sequence must also match.
- A Task checkpoint does not replace Invocation lease/fencing and does not prove that an external side effect was exactly-once.

RUST-093 implements only in-memory domain contracts. Tenant-scoped persistence, database constraints, atomic checkpoint writes, scheduling, resume, and crash recovery remain later tasks (RUST-094…RUST-096).

## Verification

The domain tests cover the full Task transition matrix, multiple distinct child Invocations, progress continuing across Invocation boundaries, duplicate/skipped step and sequence rejection, mismatched Task/unattached Invocation rejection, terminal-state behavior, and Task/Checkpoint serialization round-trips.

