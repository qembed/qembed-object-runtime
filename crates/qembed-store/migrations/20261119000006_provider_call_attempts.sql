-- RUST-041d: durable provider-call outcome boundary.
-- The canonical qembed-sql schema references object_invocations with a deferred
-- FK. This partial Rust migration chain does not create object_invocations yet;
-- add that FK when its parent-table migration lands, matching the existing
-- budget_reservations migration boundary.

CREATE TABLE provider_call_attempts (
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    invocation_id UUID NOT NULL,
    state TEXT NOT NULL DEFAULT 'NotStarted'
        CHECK (state IN (
            'NotStarted',
            'MayHaveBeenSent',
            'ResponseReceived',
            'ConfirmedNotAccepted',
            'OutcomeUnknown',
            'CancelledBeforeSend'
        )),
    input_tokens BIGINT CHECK (input_tokens >= 0),
    output_tokens BIGINT CHECK (output_tokens >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, object_id, invocation_id),
    CHECK ((input_tokens IS NULL) = (output_tokens IS NULL)),
    CHECK (
        state = 'ResponseReceived'
        OR (input_tokens IS NULL AND output_tokens IS NULL)
    )
);

-- A reservation and its provider attempt are created in the same transaction.
-- The attempt has no parent FK yet because object_invocations is not part of
-- this repository's migration chain; the canonical SQL schema owns that FK.
CREATE FUNCTION create_provider_call_attempt() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $provider_attempt_create$
BEGIN
    INSERT INTO provider_call_attempts (
        tenant_id, object_id, invocation_id, state
    )
    VALUES (
        NEW.tenant_id, NEW.object_id, NEW.invocation_id, 'NotStarted'
    );
    RETURN NEW;
END;
$provider_attempt_create$;

CREATE TRIGGER trg_budget_reservation_provider_attempt
    AFTER INSERT ON budget_reservations
    FOR EACH ROW
    EXECUTE FUNCTION create_provider_call_attempt();

CREATE FUNCTION provider_call_attempt_state_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $provider_attempt_guard$
BEGIN
    IF (NEW.tenant_id, NEW.object_id, NEW.invocation_id)
       IS DISTINCT FROM (OLD.tenant_id, OLD.object_id, OLD.invocation_id) THEN
        RAISE EXCEPTION 'provider call attempt identity is immutable'
            USING ERRCODE = '55000';
    END IF;

    IF NEW.state IS DISTINCT FROM OLD.state AND NOT (
           (OLD.state = 'NotStarted'
                AND NEW.state IN ('MayHaveBeenSent', 'CancelledBeforeSend'))
        OR (OLD.state = 'MayHaveBeenSent'
                AND NEW.state IN ('ResponseReceived', 'ConfirmedNotAccepted', 'OutcomeUnknown'))
        OR (OLD.state = 'OutcomeUnknown'
                AND NEW.state IN ('ResponseReceived', 'ConfirmedNotAccepted'))
    ) THEN
        RAISE EXCEPTION 'invalid provider call attempt transition: % -> %',
            OLD.state, NEW.state
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state = 'ResponseReceived'
       AND (NEW.input_tokens, NEW.output_tokens)
           IS DISTINCT FROM (OLD.input_tokens, OLD.output_tokens) THEN
        RAISE EXCEPTION 'persisted provider usage is immutable'
            USING ERRCODE = '55000';
    END IF;

    IF NEW.state = 'ResponseReceived' AND NEW.updated_at < OLD.updated_at THEN
        RAISE EXCEPTION 'provider call attempt timestamp cannot move backwards'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$provider_attempt_guard$;

CREATE TRIGGER trg_provider_call_attempt_state_guard
    BEFORE UPDATE ON provider_call_attempts
    FOR EACH ROW
    EXECUTE FUNCTION provider_call_attempt_state_guard();

GRANT SELECT, INSERT, UPDATE ON provider_call_attempts TO qembed_runtime;

CREATE POLICY provider_call_attempts_runtime_tenant
    ON provider_call_attempts
    FOR ALL TO qembed_runtime
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

ALTER TABLE provider_call_attempts ENABLE ROW LEVEL SECURITY;
ALTER TABLE provider_call_attempts FORCE ROW LEVEL SECURITY;
