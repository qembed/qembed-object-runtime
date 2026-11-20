-- RUST-029: separate application capabilities at the PostgreSQL boundary.
--
-- Architectural contract:
-- * qembed_runtime owns tenant-scoped object writes and append-only event/snapshot writes.
-- * qembed_dispatcher is cross-tenant by design, but can only read the outbox and
--   acknowledge delivery by changing delivered_at.
-- * qembed_admin is tenant-scoped, read-only support access and cannot read credentials.
-- * These are NOLOGIN group roles. Deployments provision LOGIN identities separately.
-- * The migration identity is separate from all application roles and needs CREATEROLE.
--
-- RLS is enabled and forced on every tenant-owned table. The tenant GUC is context,
-- not authentication: it must be established only from a trusted Scope after auth.
-- OwnerAuthenticator is the narrow exception before Scope creation: it sets the
-- requested tenant only to constrain the credential lookup, and still matches both
-- tenant_id and the token digest.

DO $roles$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'qembed_runtime') THEN
        CREATE ROLE qembed_runtime NOLOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'qembed_dispatcher') THEN
        CREATE ROLE qembed_dispatcher NOLOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'qembed_admin') THEN
        CREATE ROLE qembed_admin NOLOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'qembed_event_allocator') THEN
        CREATE ROLE qembed_event_allocator NOLOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
    END IF;
END
$roles$;

ALTER ROLE qembed_runtime NOLOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
ALTER ROLE qembed_dispatcher NOLOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
ALTER ROLE qembed_admin NOLOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
ALTER ROLE qembed_event_allocator NOLOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;

GRANT USAGE ON SCHEMA public TO qembed_runtime, qembed_dispatcher, qembed_admin, qembed_event_allocator;

-- Start from an explicit deny baseline, including any grants that might have
-- pre-existed on these role names or PUBLIC in a previously managed database.
REVOKE ALL PRIVILEGES ON TABLE
    objects, object_versions, snapshots, events, outbox_messages,
    owner_principals, owner_credentials, event_stream_counters
FROM PUBLIC, qembed_runtime, qembed_dispatcher, qembed_admin, qembed_event_allocator;

-- Runtime: mutable object state, but immutable snapshots/events are append-only.
GRANT SELECT, INSERT, UPDATE ON objects, object_versions TO qembed_runtime;
GRANT SELECT, INSERT ON snapshots, events TO qembed_runtime;
GRANT INSERT ON outbox_messages TO qembed_runtime;
GRANT SELECT ON owner_principals, owner_credentials TO qembed_runtime;

-- Dispatcher: no access to domain tables; only outbox consumption/acknowledgement.
GRANT SELECT ON outbox_messages TO qembed_dispatcher;
GRANT UPDATE (delivered_at) ON outbox_messages TO qembed_dispatcher;

-- Admin: support visibility within a tenant, never mutation or credential access.
GRANT SELECT ON objects, object_versions, snapshots, events, outbox_messages TO qembed_admin;

-- A transaction without app.tenant_id sees no tenant-owned rows and cannot insert.
CREATE POLICY objects_runtime_tenant ON objects
    FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
CREATE POLICY objects_admin_tenant ON objects
    FOR SELECT TO qembed_admin
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

CREATE POLICY object_versions_runtime_tenant ON object_versions
    FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
CREATE POLICY object_versions_admin_tenant ON object_versions
    FOR SELECT TO qembed_admin
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

CREATE POLICY snapshots_runtime_tenant ON snapshots
    FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
CREATE POLICY snapshots_admin_tenant ON snapshots
    FOR SELECT TO qembed_admin
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

CREATE POLICY events_runtime_tenant ON events
    FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
CREATE POLICY events_admin_tenant ON events
    FOR SELECT TO qembed_admin
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

-- Dispatcher is intentionally cross-tenant only for the delivery queue.
CREATE POLICY outbox_runtime_tenant ON outbox_messages
    FOR INSERT TO qembed_runtime
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
CREATE POLICY outbox_dispatcher_all_tenants ON outbox_messages
    FOR SELECT TO qembed_dispatcher
    USING (true);
CREATE POLICY outbox_dispatcher_ack ON outbox_messages
    FOR UPDATE TO qembed_dispatcher
    USING (true)
    WITH CHECK (true);
CREATE POLICY outbox_admin_tenant ON outbox_messages
    FOR SELECT TO qembed_admin
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

CREATE POLICY owner_principals_runtime_tenant ON owner_principals
    FOR SELECT TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
CREATE POLICY owner_credentials_runtime_tenant ON owner_credentials
    FOR SELECT TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

ALTER TABLE objects ENABLE ROW LEVEL SECURITY;
ALTER TABLE objects FORCE ROW LEVEL SECURITY;
ALTER TABLE object_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE object_versions FORCE ROW LEVEL SECURITY;
ALTER TABLE snapshots ENABLE ROW LEVEL SECURITY;
ALTER TABLE snapshots FORCE ROW LEVEL SECURITY;
ALTER TABLE events ENABLE ROW LEVEL SECURITY;
ALTER TABLE events FORCE ROW LEVEL SECURITY;
ALTER TABLE event_stream_counters ENABLE ROW LEVEL SECURITY;
ALTER TABLE event_stream_counters FORCE ROW LEVEL SECURITY;

-- Only the trigger's NOLOGIN owner can mutate stream counters. Runtime may
-- append an event but cannot forge or skip a stream version directly.
GRANT SELECT, INSERT, UPDATE ON event_stream_counters TO qembed_event_allocator;
CREATE POLICY event_stream_counters_allocator ON event_stream_counters
    FOR ALL TO qembed_event_allocator USING (true) WITH CHECK (true);
GRANT CREATE ON SCHEMA public TO qembed_event_allocator;
ALTER FUNCTION assign_event_stream_version() OWNER TO qembed_event_allocator;
REVOKE CREATE ON SCHEMA public FROM qembed_event_allocator;
REVOKE ALL ON FUNCTION assign_event_stream_version() FROM PUBLIC;
ALTER TABLE outbox_messages ENABLE ROW LEVEL SECURITY;
ALTER TABLE outbox_messages FORCE ROW LEVEL SECURITY;
ALTER TABLE owner_principals ENABLE ROW LEVEL SECURITY;
ALTER TABLE owner_principals FORCE ROW LEVEL SECURITY;
ALTER TABLE owner_credentials ENABLE ROW LEVEL SECURITY;
ALTER TABLE owner_credentials FORCE ROW LEVEL SECURITY;
