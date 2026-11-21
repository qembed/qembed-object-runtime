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
