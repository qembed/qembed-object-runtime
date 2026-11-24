# RUST-059 — Hierarchical kill-switch evaluation

## Status

Accepted implementation architecture.

## Context

The canonical SQL schema already owns `kill_switches`, its management-authority trigger, and its row-level security policies. Rust must evaluate those rows without introducing a competing table or weakening the SQL authority boundary.

A Tool side effect must not begin when a matching Platform, Tenant, Object, or Tool switch is disabled.

## Decision

- Keep `kill_switches` and its DDL/RLS/management rules in `qembed-sql`; the Rust adapter is read-only.
- Evaluate matching scopes in strict order: Platform → Tenant → Object → Tool.
- A missing switch means no restriction at that scope.
- Any matching disabled switch denies the action. An enabled lower-level switch never overrides a disabled ancestor.
- A Tool switch may be global (`tenant_id IS NULL`) or tenant-specific. A global Tool switch is evaluated before a tenant-specific Tool switch.
- Ignore switches for other tenants, Objects, and Tools.
- Derive the request context from trusted `Scope`, Object ID, and Tool ID. The context tenant must equal the authenticated Scope tenant.
- Read matching rows in a signed Principal PostgreSQL transaction so canonical RLS exposes only global switches and the current tenant's switches.
- Store lookup errors fail closed. The runtime invokes the side-effect closure only after the latest switch snapshot permits the action.
- The check is a pre-side-effect admission gate; it does not retroactively cancel an already-running external operation.

## Verification

- Domain tests cover all four levels, ancestor-deny precedence, global-vs-tenant Tool precedence, and isolation from unrelated scopes.
- Runtime tests prove a denied decision does not invoke the side-effect closure.
- PostgreSQL integration coverage uses the canonical SQL schema and real signed-context/RLS adapter; a disabled Tenant switch blocks a request despite enabled Object and Tool switches.
