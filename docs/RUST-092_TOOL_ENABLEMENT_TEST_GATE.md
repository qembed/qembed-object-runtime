# RUST-092 — Automated tool-enablement test gate

## Purpose

Write-capable tools must remain disabled until the security-sensitive execution path has a green, focused CI result. The normal workspace CI remains the broad regression suite; this gate is a separate check with an explicit scope so a future change cannot accidentally remove these tests from the ordinary test run without also making the activation gate fail.

## Required suites

The `Tool enablement gate (SSRF, capability, binding, lease, Unknown, reconciliation, payload)` job runs:

| Suite | Command | Coverage |
|---|---|---|
| Domain capability contracts | `cargo nextest run -p qembed-domain --profile ci` | Authorizer/AllowedCommand construction, policy decisions and capability invariants |
| Tool security unit tests | `cargo nextest run -p qembed-tools --profile ci` | A+AAAA resolution, special IP ranges, redirects, pinned connect/proxy bypass, binding validation, execution lease/fencing, Unknown handling, reconciliation, encrypted payload and hash verification |
| PostgreSQL execution lifecycle | `cargo nextest run -p qembed-store --test tool_execution_lifecycle --profile ci` | Real database binding revalidation, lease exclusivity/fencing, Unknown/no-retry transitions, reconciliation persistence and ciphertext-only payload persistence |

The focused mode is implemented by `scripts/test-postgres.sh --tool-enablement-only`. It starts the same PostgreSQL test environment and installs the RT-050 verifier fixture used by the Store integration harness, runs only the required suites, writes separate evidence logs, and tears down the test services on exit. Unknown arguments fail closed.

## Activation policy

There is currently no production configuration flag that enables write-tools, and this change does not add one. The gate is a CI prerequisite, not a runtime claim that an upstream provider has been selected or configured. The concrete provider reconciliation adapter remains unresolved; an absent provider must continue to produce `Unknown`/unresolved rather than authorize retries.

Before any future configuration or deployment path can enable write-tools, repository rules must require the exact status check:

`Tool enablement gate (SSRF, capability, binding, lease, Unknown, reconciliation, payload)`

A failed, skipped, or missing required check must block the PR/merge path used to ship that configuration. Branch-protection/ruleset administration is outside this repository code change and must be verified by a repository administrator; this PR adds the stable check but does not claim to have changed GitHub branch protection.

## Scope

This gate does not turn on write-tools, add provider SDKs, create provider reconciliation behavior, or bypass the existing Authorizer, binding validation, lease/fencing, encrypted payload and kill-switch boundaries. A green result proves the listed regression suites passed on that commit only.
