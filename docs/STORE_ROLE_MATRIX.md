# PostgreSQL store role matrix

RUST-029 defines database-enforced capabilities for the three application processes.
The roles below are PostgreSQL **NOLOGIN group roles**. Production login identities
are provisioned separately and granted membership; the schema migration never
creates passwords or process identities.

The migration/deployment identity is separate. It must own or be able to alter the
application schema and have **CREATEROLE** to install the group roles. It is not
used by runtime, dispatcher, or admin processes.

## Capability matrix

| Resource | `qembed_runtime` | `qembed_dispatcher` | `qembed_admin` |
|---|---|---|---|
| `objects` | SELECT, INSERT, UPDATE | — | SELECT |
| `object_versions` | SELECT, INSERT, UPDATE | — | SELECT |
| `snapshots` | SELECT, INSERT | — | SELECT |
| `object_invocations` | SELECT, INSERT, UPDATE | — | — |
| `invocation_idempotency` | SELECT, INSERT, UPDATE | — | — |
| `events` | SELECT, INSERT | — | SELECT |
| `outbox_messages` | INSERT | SELECT, UPDATE(`delivered_at`) | SELECT |
| `owner_principals` | SELECT | — | — |
| `owner_credentials` | SELECT | — | — |
| Schema / DDL | — | — | — |

No application role receives DELETE privileges. Snapshots and events are immutable
by both grants and their existing database triggers. Dispatcher cannot rewrite
delivery identity or payload because its UPDATE grant is column-limited to
`delivered_at`.

## Tenant boundary

RLS is enabled and forced on every tenant-owned table. Runtime and admin policies
compare `tenant_id` to transaction-local `app.tenant_id`; absent context fails
closed. Runtime may access only its current tenant. Admin is read-only and scoped
to the same tenant.

Dispatcher is deliberately different: it has cross-tenant SELECT and delivery
acknowledgement on `outbox_messages` only. It has no grants or policies on the
domain tables. This is required for a shared delivery worker to drain a global
queue and is not a general tenant-bypass capability.

## Authentication bootstrap

Owner authentication runs before a trusted `Scope` can exist. The requested
tenant ID is therefore installed as transaction-local RLS context solely to
constrain the credential lookup. The SQL query independently matches that same
tenant ID and the token digest. A caller-supplied tenant ID is a selector, not
proof of identity; `AuthenticatedIdentity` is created only after a matching,
active credential is found. The transaction-local setting disappears on commit
or rollback and cannot leak to the next pool borrower.

## Provisioning and operational constraints

- Create LOGIN users outside migrations; grant each one membership in exactly
  the group role required by its process.
- Login users must be `NOSUPERUSER NOBYPASSRLS`; do not grant membership in
  unrelated application roles.
- Keep migration credentials out of application configuration.
- The dispatcher is trusted to process all tenants' outbox messages. Its access
  is intentionally broader than a tenant-scoped runtime, but only on the outbox.
- The current outbox schema has no lease/claim/attempt model. This role contract
  does not invent one; it covers only SELECT and `delivered_at` acknowledgement.
