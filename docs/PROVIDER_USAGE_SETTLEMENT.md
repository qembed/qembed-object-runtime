# Provider usage settlement (RUST-041c)

RUST-041c converts one committed token reservation into final token accounting after the provider adapter returns a normalized response. It does not perform provider I/O and does not release reservations for failed or ambiguous provider calls; those lifecycle cases belong to RUST-041d.

## Usage normalization and fallback

Provider adapters expose usage as `Option<TokenUsage>`:

- A valid normalized pair of non-negative integer input/output counts is authoritative.
- Missing usage remains `None`.
- Malformed usage metadata (wrong shape, negative/fractional counts, missing required members, or values outside the supported integer range) is treated as unavailable usage, not coerced to zero and not allowed to invalidate an otherwise valid response.
- When usage is unavailable, settlement uses the original non-zero reserved estimate and marks the invocation degraded with `provider_usage_missing`.

The domain calculation is provider-neutral. Vendor SDK types and raw provider payloads do not enter the store.

## Atomic accounting transition

Settlement uses the same lock order as reservation creation:

1. Lock the tenant/object `object_budgets` row.
2. Lock the relevant `usage_counters` row for the reservation's UTC date and pool.
3. Lock the matching `caller_usage_counters` row.
4. Lock the `budget_reservations` row.

Within one tenant-scoped PostgreSQL transaction it then:

- subtracts the original estimate from both reserved counters;
- adds normalized actual tokens (or the estimate fallback) to both consumed counters;
- records `budget_reservations.actual_tokens` and changes `Reserved` to `Settled`;
- records `provider_usage_missing` on the invocation when fallback was required.

Any failed update or database error rolls back every accounting change. A settled reservation is immutable: a repeated settlement returns the persisted actual amount and degraded flag without changing counters, even if a retry supplies different in-memory usage.

## Actual usage and limits

Provider-reported usage is authoritative and may differ from the estimate in either direction. Settlement records actual usage rather than clamping it to the estimate. If actual usage exceeds the estimate, the counters reflect that overage; subsequent reservations see the higher consumed total and may be rejected by the daily limits. The reservation acceptance decision remains protected by the PostgreSQL object-budget lock.

The invocation's effective per-invocation ceiling is enforced before dispatch through the output allowance. Settlement does not fabricate a lower usage value if a provider reports more than the allowed amount; such discrepancies remain visible in accounting for operational handling.

## Completion semantics

If provider usage was unavailable, settlement persists `degraded_reason='provider_usage_missing'`. The existing completion write then transitions the invocation to `CompletedDegraded` rather than `Completed`. Valid normalized usage leaves the normal `Completed` path unchanged.

## Explicitly out of scope

- Releasing reservations for pre-provider failures.
- Reconciliation after timeout, process crash, cancellation, or ambiguous provider outcome.
- Cost settlement and `actual_cost_micros`.
- Automatically retrying provider requests.

These are not safe to infer from a missing response and are handled by RUST-041d or a separately specified cost contract.
