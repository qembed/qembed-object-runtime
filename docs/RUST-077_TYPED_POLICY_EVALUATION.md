# RUST-077 — Typed policy evaluation contract

## Decision model

Policy evaluation returns one of three typed verdicts:

- Allow — every required trusted check is present and Satisfied.
- Deny — any required check is Denied or Unknown, or the check set is incomplete, duplicated, or contains a check not required for the action class.
- RequireApproval — all mandatory checks are Satisfied, but at least one trusted check explicitly requires a separate approval step.

Deny/Unknown and malformed-check outcomes take precedence over approval. Unknown fails closed.

## Action/check matrix

| Action class | Required trusted checks |
|---|---|
| KnowledgeRead | PrincipalAuthorized, TenantScopeMatches, ObjectActive, BudgetAvailable |
| ToolExecution | PrincipalAuthorized, TenantScopeMatches, ObjectActive, CapabilityGranted, BudgetAvailable, BindingCurrent |
| ObjectMutation | PrincipalAuthorized, TenantScopeMatches, ObjectActive, CapabilityGranted |
| ExternalNetwork | PrincipalAuthorized, TenantScopeMatches, ObjectActive, CapabilityGranted, BudgetAvailable, BindingCurrent, NetworkTargetValidated |

Each required check must appear exactly once. Missing, duplicate, and unexpected checks deny the action.

## Trust boundary

- TrustedPolicyCheck is created only from trusted server-side facts such as authenticated identity, tenant Scope, Object state, capability/binding state, budgets, and network validation.
- Model output and document content are represented separately as UntrustedEvidenceDigest. The raw bytes are SHA-256 hashed and retained only as source + digest in the audit context.
- Untrusted evidence is never parsed into a PolicyCheck and never consulted by the decision function. Adversarial text can change its audit digest, not the verdict.
- PolicyEvaluationInput and TrustedPolicyCheck are not serializable/deserializable transport DTOs. They cannot be populated directly from model/document JSON.
- The evaluator does not mint AllowedCommand. The private mint boundary remains for RUST-078 Authorizer.

## Audit context

Every result contains invocation, Principal, Object, policy version, action class, action digest, evaluation timestamp, all trusted check outcomes, typed reason codes, and hash-only references to untrusted evidence. Raw model/document text is not stored in this audit structure.

## Verification

Unit tests cover the full action matrix, Allow/Deny/RequireApproval precedence, Unknown and missing checks, duplicate and unexpected checks, zero policy version, and adversarial model/document content that attempts to elevate a denied verdict.
