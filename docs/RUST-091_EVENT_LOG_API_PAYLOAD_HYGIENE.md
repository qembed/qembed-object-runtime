# RUST-091 — Event, log and API payload hygiene

## Security contract

Secrets and raw tool-command arguments must not cross the encrypted command-payload boundary. The persisted command payload is an opaque ciphertext envelope; the Store persists ciphertext, key ID and the digest bound to the immutable Allow decision. Decryption is restricted to the executor path after the Allow hash is checked.

The public API, lifecycle events and outbox delivery projection expose only their explicit contracts. They must not serialize bearer credentials, raw request input, idempotency keys, raw command arguments, plaintext command payloads, ciphertext envelopes or provider diagnostics.

## Verified boundaries

| Boundary | Required behavior | Evidence |
|---|---|---|
| Command payload persistence | PostgreSQL stores ciphertext, key ID and arguments hash; matching Allow hash is required to load | `command_payload_store_persists_ciphertext_and_requires_matching_allow_hash` in `crates/qembed-store/tests/tool_execution_lifecycle.rs`; encryption round-trip/tamper tests in `crates/qembed-tools/src/command_payload_encryption.rs` |
| Object lifecycle events | Serialize-only private event enum with exact allowlisted fields; no arbitrary JSON attachment | `event_outbox.rs` unit tests assert exact key sets and recursively reject sensitive field names |
| Invocation.received event and outbox | Event carries invocation/object/caller/snapshot routing metadata only; it excludes request input, idempotency key and bearer credential | `owner_invoke.rs` PostgreSQL/API integration test checks exact event keys and both persisted event/outbox payloads against synthetic sentinels |
| Public invocation API | Accepted and inspection responses do not echo bearer credentials or raw request input; inspection remains a minimized projection | `owner_invoke.rs` PostgreSQL/API integration test |
| Public errors | Fixed status/code/message mapping; adapter and Store error text is discarded | `crates/qembed-api/src/errors.rs` tests, including a synthetic DSN-bearing internal error |

## Logging audit and limitation

The reviewed API, Store event/outbox, and tool-payload paths do not emit application log records. The workspace root does not declare a direct `tracing` or `log` dependency, and this task does not add logging instrumentation solely to create a test sink. Consequently there is no application log stream to capture in the current implementation; log-capture assertions become applicable when a logging adapter is introduced. At that point, tests must use synthetic credentials and payload sentinels and assert they are absent from captured records.

This is an explicit audit finding, not a claim that arbitrary deployment infrastructure, reverse proxies, database logs, or NATS server logs are covered by these Rust tests. Those systems require separate operational configuration and access-control review.

## Transport and DLQ boundary

The NATS consumer's dead-letter envelope deliberately preserves source message bytes for diagnosis. This is safe only for source messages that obey the event contract: invocation and object lifecycle events carry identifiers/state metadata, never raw user input or tool-command arguments. Producers must not publish decrypted command payloads or credentials to event subjects. DLQ retention and access controls remain an operational responsibility; this task does not change the existing byte-preservation contract.

## Scope

No provider SDK, logging backend, schema migration or new runtime logging behavior is introduced. The tests prove the current Rust API/event contracts and the ciphertext persistence boundary; they do not prove redaction in external infrastructure or arbitrary third-party producers.
