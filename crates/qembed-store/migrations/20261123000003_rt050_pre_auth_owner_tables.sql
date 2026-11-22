-- RT-050: align the Rust test schema with canonical SQL pre-auth table policy.
-- owner_principals and owner_credentials are queried before a trusted Scope exists.
-- They intentionally have no tenant RLS; qembed_runtime receives only the columns
-- required by OwnerAuthenticator. The tenant predicate and token digest are bound
-- together in one parameterized statement.
BEGIN;

DROP POLICY IF EXISTS owner_principals_runtime_tenant ON owner_principals;
DROP POLICY IF EXISTS owner_credentials_runtime_tenant ON owner_credentials;

ALTER TABLE owner_principals NO FORCE ROW LEVEL SECURITY;
ALTER TABLE owner_principals DISABLE ROW LEVEL SECURITY;
ALTER TABLE owner_credentials NO FORCE ROW LEVEL SECURITY;
ALTER TABLE owner_credentials DISABLE ROW LEVEL SECURITY;

REVOKE ALL PRIVILEGES ON TABLE owner_principals, owner_credentials
    FROM PUBLIC, qembed_runtime, qembed_dispatcher, qembed_admin, qembed_event_allocator;

GRANT SELECT (tenant_id, caller_id, principal_kind, disabled_at)
    ON owner_principals TO qembed_runtime;
GRANT SELECT (tenant_id, caller_id, token_hash, revoked_at)
    ON owner_credentials TO qembed_runtime;

COMMIT;
