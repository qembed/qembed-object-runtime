# Deterministic Mock ChatProvider

qembed-llm provides an in-memory implementation of the domain ChatProvider port for tests.

## Script selection

A script is keyed by the complete ChatRequest value. Each request can be registered exactly once with either a response or a typed failure. A request with no script fails with UnscriptedRequest.

The mock deliberately does not use a FIFO response queue. A shared queue makes results depend on task scheduling when tests issue concurrent requests. Exact request matching makes each result independent of call order. The provider is immutable after construction and needs no mutex.

## Failure simulation

Timeout and Unavailable are immediate typed outcomes. Timeout simulation does not sleep or consult a clock: tests exercise error handling without introducing timing flakiness or slowing the suite.

## Usage fixtures

Responses are ordinary validated domain ChatResponse values. Tests can attach TokenUsage exactly as a real adapter would, while still representing providers that omit usage with None.

## Boundary

The mock depends only on qembed-domain. It has no provider SDK, HTTP client, credentials, database, or runtime side effects. It is a test adapter, not a production fallback provider.
