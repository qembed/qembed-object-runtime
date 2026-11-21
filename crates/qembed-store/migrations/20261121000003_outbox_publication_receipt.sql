-- RUST-051b: persist the durable transport acknowledgement alongside delivery state.
--
-- Legacy delivered rows may predate PubAck receipts. They retain all-null
-- publication metadata; every newly recorded receipt must be complete.
ALTER TABLE outbox_messages
    ADD COLUMN publication_stream TEXT,
    ADD COLUMN publication_sequence BIGINT,
    ADD COLUMN publication_duplicate BOOLEAN,
    ADD CONSTRAINT outbox_publication_receipt_consistent CHECK (
        (publication_stream IS NULL AND publication_sequence IS NULL AND publication_duplicate IS NULL)
        OR
        (
            publication_stream IS NOT NULL
            AND publication_stream <> ''
            AND publication_sequence IS NOT NULL
            AND publication_sequence > 0
            AND publication_duplicate IS NOT NULL
            AND delivered_at IS NOT NULL
        )
    );

GRANT SELECT (
    publication_stream, publication_sequence, publication_duplicate
) ON outbox_messages TO qembed_dispatcher;
GRANT UPDATE (
    publication_stream, publication_sequence, publication_duplicate
) ON outbox_messages TO qembed_dispatcher;

COMMENT ON COLUMN outbox_messages.publication_stream IS
    'Stream name returned by the durable transport publication acknowledgement';
COMMENT ON COLUMN outbox_messages.publication_sequence IS
    'Stream sequence returned by the durable transport publication acknowledgement';
COMMENT ON COLUMN outbox_messages.publication_duplicate IS
    'Whether the transport acknowledged this delivery as a deduplicated duplicate';

-- Keep every acknowledged publication, including future recovery replays.
-- The dispatcher can append receipts but cannot inspect or mutate this history.
CREATE TABLE outbox_publication_receipts (
    receipt_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL,
    delivery_id UUID NOT NULL,
    publication_stream TEXT NOT NULL CHECK (publication_stream <> ''),
    publication_sequence BIGINT NOT NULL CHECK (publication_sequence > 0),
    publication_duplicate BOOLEAN NOT NULL,
    published_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (tenant_id, delivery_id)
        REFERENCES outbox_messages (tenant_id, delivery_id) ON DELETE RESTRICT
);

CREATE INDEX outbox_publication_receipts_delivery_idx
    ON outbox_publication_receipts (tenant_id, delivery_id, published_at, receipt_id);

CREATE TRIGGER outbox_publication_receipts_append_only
    BEFORE UPDATE OR DELETE ON outbox_publication_receipts
    FOR EACH ROW EXECUTE FUNCTION forbid_mutation();

ALTER TABLE outbox_publication_receipts ENABLE ROW LEVEL SECURITY;
ALTER TABLE outbox_publication_receipts FORCE ROW LEVEL SECURITY;
CREATE POLICY disp_outbox_receipts ON outbox_publication_receipts
    FOR INSERT TO qembed_dispatcher WITH CHECK (true);

REVOKE ALL PRIVILEGES ON outbox_publication_receipts FROM PUBLIC, qembed_runtime, qembed_dispatcher;
GRANT INSERT ON outbox_publication_receipts TO qembed_dispatcher;

COMMENT ON TABLE outbox_publication_receipts IS
    'Append-only history of transport PubAck receipts; never a business completion ledger';
