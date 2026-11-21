-- Tenant-owned objects and their immutable-by-publication versions.
CREATE TABLE objects (
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    state TEXT NOT NULL DEFAULT 'draft'
        CHECK (state IN ('draft', 'validating', 'active', 'suspended', 'archived')),
    published_version_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, object_id),
    CHECK (state NOT IN ('active', 'suspended') OR published_version_id IS NOT NULL)
);

CREATE TABLE object_versions (
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    object_version_id UUID NOT NULL,
    version_number BIGINT NOT NULL CHECK (version_number > 0),
    status TEXT NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft', 'published', 'superseded')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, object_id, object_version_id),
    UNIQUE (tenant_id, object_id, version_number),
    FOREIGN KEY (tenant_id, object_id)
        REFERENCES objects (tenant_id, object_id)
        ON DELETE RESTRICT
);

ALTER TABLE objects
    ADD CONSTRAINT objects_published_version_fk
    FOREIGN KEY (tenant_id, object_id, published_version_id)
    REFERENCES object_versions (tenant_id, object_id, object_version_id)
    ON DELETE RESTRICT;
