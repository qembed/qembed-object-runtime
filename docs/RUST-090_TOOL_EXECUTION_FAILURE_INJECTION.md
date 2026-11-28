# RUST-090 — ToolExecution failure-injection matrix

## Contract

A tool side effect and its PostgreSQL result cannot be committed atomically. Therefore every failure between the upstream call and the durable result write is treated as ambiguous. A worker restart must not turn that ambiguity into an automatic retry.

PostgreSQL remains authoritative for ToolExecution state. The independent lease, version and fencing token decide which worker may commit. An Unknown execution is eligible for reconciliation only; it is never claimable through the normal execution path.

## Failure matrix

| Failure point | Durable state / expected behavior | Executable evidence |
|---|---|---|
| Encrypted payload cannot be loaded or verified, before claim | Authorized; no lease and no upstream call | payload_read_failure_happens_before_claim_and_external_call |
| Worker loses the lease after claim but before the network call | No upstream call; executor returns Stale. Lease expiry recovery later moves the row to Unknown | lost_lease_before_network_call_prevents_external_execution; expired_tool_execution_becomes_unknown_and_cannot_be_retried |
| Upstream returns a definitive success/failure | Result and terminal state are written under the current owner/version/fence; replayed write is a CAS miss | definitive_provider_failure_is_persisted_as_failed; concurrent_workers_get_one_tool_execution_lease_and_only_its_fence_can_commit |
| Upstream times out or returns a transport error | Outcome is ambiguous; no terminal result is written and no unsafe retry occurs | timeout_is_ambiguous_and_non_idempotent_action_is_never_retried; idempotent_retry_reuses_the_same_upstream_key |
| Upstream accepted the side effect, process dies or result persistence fails before commit | The failed SQL transaction leaves the row Executing with no partial output. Once the lease expires, recovery changes it to Unknown; replacement workers cannot claim it | persistence_failure_after_provider_success_never_replays_the_command; database_commit_failure_after_external_success_recovers_as_unknown_without_retry |
| Old worker returns after its lease/fence is no longer current | Its completion is rejected; newer durable state is not overwritten | concurrent_workers_get_one_tool_execution_lease_and_only_its_fence_can_commit; postgres_reconciliation_rejects_stale_fencing_token |
| Process dies after PostgreSQL commits a terminal result but before the caller observes success | Terminal state remains durable; repeated completion is rejected by CAS and execution is not claimable again | concurrent_workers_get_one_tool_execution_lease_and_only_its_fence_can_commit |

## Test boundaries

- Executor unit tests inject payload-read, heartbeat and completion-store failures around the network boundary.
- PostgreSQL integration injects a trigger failure specifically on the terminal result UPDATE for one execution ID. It verifies transaction rollback (including no partial output), expires the lease, performs the real Executing -> Unknown recovery transition and proves a replacement worker cannot claim it.
- Existing tests continue to cover deterministic upstream idempotency keys, timeout ambiguity, terminal writes, lease expiry, stale versions/fencing tokens and reconciliation CAS.
- The injected provider acceptance is a controlled test condition, not a claim that the test contacts a real provider. No real provider SDK or external network call is introduced.

## Safety boundary

This suite validates local durable state and retry decisions at the ToolExecution boundary. It does not prove that an arbitrary upstream provider supports idempotency or offers a reconciliation API. Those guarantees must be established by a concrete provider adapter before that adapter is enabled.