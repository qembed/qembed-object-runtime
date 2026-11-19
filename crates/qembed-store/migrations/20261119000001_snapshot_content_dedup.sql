-- Match the canonical qembed-sql snapshots uniqueness invariant.
-- Existing duplicate content hashes must be investigated rather than silently
-- discarded: snapshots are immutable and are tied to exact object versions.
-- Hold the table lock across the duplicate check and constraint creation so
-- concurrent inserts cannot introduce a duplicate between those operations.
LOCK TABLE snapshots IN ACCESS EXCLUSIVE MODE;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM snapshots
        GROUP BY tenant_id, object_id, content_hash
        HAVING count(*) > 1
    ) THEN
        RAISE EXCEPTION
            'cannot add snapshot content uniqueness: duplicate (tenant_id, object_id, content_hash) rows exist'
            USING ERRCODE = '23505';
    END IF;
END;
$$;

ALTER TABLE snapshots
    ADD CONSTRAINT snapshots_tenant_object_content_hash_key
    UNIQUE (tenant_id, object_id, content_hash);
