# ChatProvider contract (RUST-030)

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

## Response

- Response content is normalized plain text, non-empty and at most 1 MiB.
- Finish reason is provider-neutral: `stop`, `length`, or `content_filtered`.
- Token usage is optional because providers differ in whether they return it.
  Missing-usage policy and provider-specific usage mapping are RUST-034.

## Validation and serialization

Constructors enforce all limits. Custom deserialization routes through the
same constructors, so JSON input cannot bypass the contract. Unknown fields
are rejected for message/request/response wire objects to avoid silently
accepting provider-specific extensions.

## Deliberate exclusions

Structured output modes, capability negotiation, retries, provider error
taxonomy, tool calls, streaming, and provider-specific tuning parameters are
not part of RUST-030. They are separate roadmap tasks and must not be smuggled
into a generic options map.
