-- RUST-086: application-encrypted command payload persistence.
-- The envelope (wrapped DEK + nonce + AEAD ciphertext) is opaque to PostgreSQL.
CREATE TABLE tool_command_payloads (
    tenant_id UUID NOT NULL,
    command_id UUID NOT NULL,
    payload_ciphertext BYTEA NOT NULL,
    key_id TEXT NOT NULL,
    arguments_hash BYTEA NOT NULL CHECK (octet_length(arguments_hash) = 32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (tenant_id, command_id),
    FOREIGN KEY (tenant_id, command_id)
        REFERENCES policy_decisions (tenant_id, command_id) ON DELETE CASCADE
);
CREATE INDEX idx_tcp_expiry ON tool_command_payloads (expires_at);

ALTER TABLE tool_command_payloads ENABLE ROW LEVEL SECURITY;
ALTER TABLE tool_command_payloads FORCE ROW LEVEL SECURITY;
CREATE POLICY tool_command_payloads_runtime_tenant ON tool_command_payloads
    FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

GRANT SELECT, INSERT, UPDATE ON tool_command_payloads TO qembed_runtime;
