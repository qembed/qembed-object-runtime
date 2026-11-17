-- Persist tenant-bound owner principals and high-entropy API-token digests.
-- Raw credentials are never stored in PostgreSQL.
CREATE TABLE owner_principals (
    tenant_id UUID NOT NULL,
    caller_id UUID NOT NULL,
    principal_kind TEXT NOT NULL CHECK (principal_kind = 'tenant_owner'),
    disabled_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, caller_id)
);

CREATE TABLE owner_credentials (
    tenant_id UUID NOT NULL,
    caller_id UUID NOT NULL,
    token_hash BYTEA NOT NULL CHECK (octet_length(token_hash) = 32),
    revoked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, caller_id, token_hash),
    FOREIGN KEY (tenant_id, caller_id)
        REFERENCES owner_principals (tenant_id, caller_id)
        ON DELETE RESTRICT
);

CREATE INDEX owner_credentials_active_lookup
    ON owner_credentials (tenant_id, token_hash)
    WHERE revoked_at IS NULL;
