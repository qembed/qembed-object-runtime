# Principal and Caller identities

RUST-036 implements the Rust side of the SQL-011 delegation contract.

## Two identities, two jobs

- **Principal** answers: who authenticated? It is established by the trusted authentication adapter and represented by the strongly typed `PrincipalId`. In the current Owner authentication path, the physical SQL column is `owner_principals.caller_id`; that legacy column name does not make it a `CallerId`.
- **Caller** answers: on whose behalf is this invocation running? It is represented by `CallerId` and has an independently persisted kind: `Anonymous`, `Authenticated`, or `Service`.
- **Scope** carries the authenticated Principal and tenant boundary. It does not select or impersonate an invocation Caller.
- **InvocationPool** is a separate budget/execution classification (`anonymous` or `authenticated`). It is passed explicitly by trusted application policy; Rust does not infer it from Caller kind. In particular, Service does not imply a pool.

UUID equality between PrincipalId and CallerId has no meaning. Rust uses distinct newtypes so they cannot be passed interchangeably.

## Selecting an effective Caller

A request may ask to act as a Caller by supplying its ID, but that value is only a selection request. It is not authority.

1. Authentication produces a trusted PrincipalId and tenant.
2. `begin_tenant_transaction` sets transaction-local `app.tenant_id` and `app.principal_id` from the trusted Scope.
3. The Store calls SQL-011 `resolve_delegated_caller(requested_caller_id)`.
4. The SQL function verifies the exact active tenant/Principal/Caller delegation and checks Principal and Caller lifecycle state.
5. It locks the delegation, Principal, and Caller rows `FOR SHARE`. Those locks remain held until the begin-invocation transaction commits.
6. The Store returns `ResolvedCaller`, whose fields are private and which can only be minted by the resolver adapter.
7. Caller-scoped idempotency and `object_invocations.caller_id` use that ResolvedCaller, never PrincipalId.

No automatic self-delegation, Caller creation, UUID-equality fallback, or transitive delegation is permitted. A missing or revoked grant fails closed.

## Delegation lifecycle

SQL-011 stores direct Principal-to-Caller grants in `principal_caller_delegations`. A Principal may have multiple active Callers. Grants are historical; revocation is final and audited, and a later grant creates a new row. A Caller can be delegated to multiple Principals, but each Principal/Caller pair is authorized independently.

The resolver's row locks serialize invocation start against grant revocation, Principal disablement, and Caller revocation. Provider I/O occurs only after the transaction commits and releases those locks.

## Database trust boundary

The application sets `app.tenant_id` and `app.principal_id` only from trusted authentication state. Request DTOs never set these GUCs. The SQL runtime role has read-only access to the delegation table and EXECUTE on narrow SECURITY DEFINER functions; it has no direct delegation write privileges.

These GUCs are trusted application context, not cryptographic credentials. The runtime DB role and its connection path must remain application-internal; arbitrary SQL under that role could set custom GUCs.
