# RUST-073 — Citation / Provenance Verifier V1–V7

## Purpose and boundary

The verifier is a deterministic domain component. It receives a structured
document of factual claims and the exact retrieved Knowledge chunks offered
as evidence. It rejects malformed claims, missing or unresolved citations,
and numeric or explicit-URL literals that cannot be found in the cited
chunks.

The verifier does **not** claim to prove natural-language entailment. A
citation can resolve correctly while the prose still overstates or
misinterprets its source. Semantic entailment requires a separately specified
mechanism and must not be implied by this deterministic verifier. The
application-level trust boundary, invocation/caller binding, and resistance
to forged provenance remain separate work (RUST-074).

## V1–V7 rule contract

| Rule | Check | Reject when |
|---|---|---|
| V1 — Envelope | Bound claim and retrieved-evidence counts | Either collection is empty or exceeds its configured maximum |
| V2 — Claim shape | Validate claim IDs and text | ID is malformed/duplicated; text is blank, oversized, or contains forbidden control characters |
| V3 — Required citations | Require bounded, unique citation lists | A claim has no citations, too many citations, or duplicate citations |
| V4 — Citation resolution | Resolve IDs against this retrieval result | Any citation is absent from the supplied retrieved chunks |
| V5 — Numeric grounding | Compare normalized numeric literals | A number or percentage in a claim is absent from all chunks cited by that claim |
| V6 — Link grounding | Compare explicit URLs | An explicit URL in a claim is absent from all chunks cited by that claim |
| V7 — Evidence integrity | Reject ambiguous or malformed evidence | A retrieved chunk is empty/oversized or duplicate chunk IDs make resolution ambiguous |

## Bounds

- 1–128 claims per document.
- 1–512 retrieved chunks per verification request.
- Claim text: at most 8 KiB UTF-8.
- Retrieved chunk text: at most 64 KiB UTF-8.
- 1–16 unique citations per claim.
- Claim IDs: 1–64 ASCII bytes from the set A–Z, a–z, 0–9, underscore and hyphen.

The verifier fails closed. It does not silently truncate oversized claim/evidence
collections and does not treat unresolved citations as external references.

## Numeric and URL matching

Numeric literals are compared after deterministic normalization of unambiguous
multi-group separators, decimal separators, signs, and percent units. A single
comma or dot followed by three digits is treated as ambiguous decimal notation,
not silently assumed to be a thousands separator. A percentage is not treated
as the same literal as the corresponding unqualified number.

Explicit HTTP, HTTPS, and www URLs are extracted and compared verbatim after
trimming sentence punctuation. The verifier does not fetch URLs, follow
redirects, or establish that a URL is trustworthy.

## Output

VerificationReport contains stable rule identifiers and findings. An empty
violation list means the document passed these deterministic provenance
checks, not that every sentence is semantically entailed by its evidence.

## Tests

The positive/negative corpus covers:
- valid chunk IDs and unresolved chunk IDs;
- required and duplicate citations;
- grounded and unsupported numbers/percentages;
- grounded and unsupported explicit URLs;
- duplicate evidence IDs and malformed evidence;
- empty, oversized, malformed-ID, control-bearing and malformed-JSON claims;
- deterministic generated property-style cases over malformed claim inputs.

The verifier is pure domain logic and performs no database, network, model, or
provider I/O.
