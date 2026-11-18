# ChatProvider contract (RUST-030, RUST-032, RUST-033)

## Boundary

`qembed_domain::ports::ChatProvider` is the domain-facing outbound port.
Implementations belong in adapter crates and translate the contract to/from
provider SDKs. No SDK request, response, error, model, or client type may
appear in domain/runtime public APIs.

## Request

- `model_key` is an application-owned routing alias resolved by adapter
  configuration; it is not a vendor model identifier.
- Messages have only the semantic roles `system`, `user`, and `assistant`.
- The request contains 1–128 messages.
- A message body is non-empty and at most 64 KiB of UTF-8.
- Combined message bodies are at most 256 KiB of UTF-8.
- The model routing key is non-empty and at most 128 bytes.
- `max_output_tokens` is in the inclusive range 1–32,768.
- `structured_output` is a provider-neutral mode:
  - `none` (default) imposes no output-format constraint.
  - `json_only` requires the normalized response content to be exactly one
    valid JSON value, with no surrounding prose or markdown fences.
- Deserialization of older requests that omit `structured_output` defaults
  to `none`; the existing `ChatRequest::new` constructor remains source
  compatible and also defaults to `none`.

## Response

- Response content is normalized text, non-empty and at most 1 MiB.
- Finish reason is provider-neutral: `stop`, `length`, or `content_filtered`.
- Token usage is optional because providers differ in whether they return it.
  Adapters report it only when both input and output counts are known and fit
  the domain counter range; absent or partial counters normalize to `None`.
- Provider failures are normalized to `ChatProviderError`: `Timeout`,
  `RateLimited` (with an optional millisecond delay hint), `Unavailable`,
  `MalformedOutput`, or `Other`. Raw SDK errors and provider messages stay
  inside the adapter. A rate-limit hint is metadata, not a retry instruction.
- For `json_only`, `qembed-llm::structured_output::validate_response`
  rejects malformed JSON, multiple/trailing values, prose, and markdown
  fences with the typed `MalformedJson` error.
- `none` preserves any otherwise-valid non-empty response text.

## Retry identity and provider idempotency

Every invocation's primary chat completion receives a stable
`ProviderRequestContext`. Its opaque key is derived from the durable
`InvocationId` and operation version, not from a worker ID or lease token, so
reclaiming an expired lease does not change the key. The runtime supplies the
same key on every attempt for that invocation.

A provider route advertises idempotency support only when its adapter forwards
the key unchanged and the upstream provider guarantees deduplication within
its documented scope and retention window. Such adapters override
`ChatProvider::complete_with_context` and mark their capability profile with
`with_idempotency_keys()`. Providers without that guarantee may use the
compatibility default, which ignores the context; the runtime must not claim
that this prevents duplicate execution or charges.

The key is not a fencing token: it deduplicates upstream execution where
supported, while PostgreSQL lease ownership and fencing independently decide
which worker may persist the result. If the provider's key retention window has
expired, or the provider does not guarantee deduplication, duplicate execution
remains possible. No exactly-once guarantee is implied.

## Provider support

Before sending a request, the adapter exposes a local, side-effect-free
`ChatCapabilities` profile for each configured `model_key`. The descriptor
lists supported structured-output modes. `None` is universally supported
because it imposes no constraint; additional modes must be listed explicitly.

`qembed-llm::structured_output::negotiate_request` receives the request and
that descriptor. It returns `UnconfiguredModel` when no route exists and
`UnsupportedMode` when the selected profile cannot satisfy the request. The
caller must complete this check before invoking `ChatProvider::complete`, so
unsupported requests cannot trigger provider I/O. The model key is an opaque
lookup key: adapters must not parse it or infer capabilities from its name.

The deterministic mock supports multiple independently configured profiles
and exposes them through the same domain port as a real adapter.

## Validation and serialization

Constructors enforce all size and limit constraints. Custom deserialization
routes through the same constructors, so JSON input cannot bypass the
contract. Unknown fields are rejected for message/request/response wire
objects to avoid silently accepting provider-specific extensions.

## Deliberate exclusions

Capability discovery/negotiation, retry policy, tool calls, streaming, and
provider-specific tuning parameters are not part of this contract. They must
not be smuggled into a generic options map. Error normalization does not imply
that any category is automatically retryable.
