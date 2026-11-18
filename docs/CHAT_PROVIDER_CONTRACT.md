# ChatProvider contract (RUST-030, RUST-032)

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
  Missing-usage policy and provider-specific usage mapping are RUST-034.
- For `json_only`, `qembed-llm::structured_output::validate_response`
  rejects malformed JSON, multiple/trailing values, prose, and markdown
  fences with the typed `MalformedJson` error.
- `none` preserves any otherwise-valid non-empty response text.

## Provider support

Before sending a request, an adapter must call
`ensure_mode_supported` with the modes supported by its selected
configuration. An unsupported requested mode returns the typed
`UnsupportedMode` error before network I/O. `none` is always accepted
because it imposes no constraint.

This support check is deliberately separate from capability discovery.
RUST-033 will define the provider-neutral capability descriptor and
negotiation path; this task does not infer support from a model name.

## Validation and serialization

Constructors enforce all size and limit constraints. Custom deserialization
routes through the same constructors, so JSON input cannot bypass the
contract. Unknown fields are rejected for message/request/response wire
objects to avoid silently accepting provider-specific extensions.

## Deliberate exclusions

Capability discovery/negotiation, retries, provider error taxonomy, tool
calls, streaming, and provider-specific tuning parameters are not part of
this contract. They must not be smuggled into a generic options map.
