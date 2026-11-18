//! Atomic creation of an invocation and its durable delivery event.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use qembed_domain::caller::{CallerKind, InvocationPool};
use qembed_domain::ids::{CallerId, EventId, InvocationId, MessageId, ObjectId, SnapshotId, TraceId};
use qembed_domain::scope::Scope;
use serde::Serialize;
use sqlx::PgPool;

use crate::caller::{CallerResolutionError, ResolvedCaller, resolve_delegated_caller};
use crate::idempotency::{
    IdempotencyClaim, IdempotencyReservation, IdempotencyReservationError,
    reserve_invocation_idempotency,
};
use crate::{TenantTransaction, begin_tenant_transaction, execute, fetch_optional};

/// All caller-controlled inputs needed to start an invocation.
///
/// requested_caller_id is only a selection request. It becomes an effective
/// Caller after resolve_delegated_caller verifies the exact Principal/Caller
/// delegation and holds row locks through commit. InvocationPool is explicit
/// and is never inferred from CallerKind.
#[derive(Clone, Debug)]
pub struct BeginInvocationCommand {
    pub object_id: ObjectId,
    pub requested_caller_id: CallerId,
    pub invocation_pool: InvocationPool,
    pub snapshot_id: SnapshotId,
    pub idempotency_key: String,
    pub payload_hash: Vec<u8>,
    /// Original user input, persisted before the invocation is dispatched.
    pub input_message: String,
    /// Retention deadline for the user and assistant interaction messages.
    pub message_expires_at: SystemTime,
    pub idempotency_expires_at: SystemTime,
    pub trace_id: TraceId,
}

/// Durable outcome of begin-invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeginInvocationOutcome {
    /// A new invocation, its pinned inputs, event and outbox delivery committed.
    Created {
        invocation_id: InvocationId,
        caller_id: CallerId,
        caller_kind: CallerKind,
        snapshot_id: SnapshotId,
        knowledge_revision: u64,
        invocation_pool: InvocationPool,
    },
    /// The same effective Caller repeated the same key and payload.
    Replay { invocation_id: InvocationId },
}

/// Failure to atomically begin an invocation.
#[derive(Debug)]
pub enum BeginInvocationError {
    CallerResolution(CallerResolutionError),
    Idempotency(IdempotencyReservationError),
    ObjectNotFound,
    ObjectNotActive,
    SnapshotNotFound,
    SnapshotIsNotCurrentPublishedVersion,
    KnowledgeRevisionNotFound,
    KnowledgeRevisionOutOfRange,
    InvalidInputMessage,
    InvalidMessageExpiry,
    Database(sqlx::Error),
}

impl fmt::Display for BeginInvocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CallerResolution(error) => write!(formatter, "Caller resolution failed: {error}"),
            Self::Idempotency(error) => {
                write!(formatter, "idempotency reservation failed: {error}")
            }
            Self::ObjectNotFound => formatter.write_str("object does not exist in this tenant"),
            Self::ObjectNotActive => formatter.write_str("object is not active for invocation"),
            Self::SnapshotNotFound => {
                formatter.write_str("snapshot does not belong to this object and tenant")
            }
            Self::SnapshotIsNotCurrentPublishedVersion => {
                formatter.write_str("snapshot is not for the current published object version")
            }
            Self::KnowledgeRevisionNotFound => {
                formatter.write_str("object has no knowledge revision row")
            }
            Self::KnowledgeRevisionOutOfRange => {
                formatter.write_str("knowledge revision is outside the supported range")
            }
            Self::InvalidInputMessage => formatter.write_str("input message is empty or too large"),
            Self::InvalidMessageExpiry => formatter.write_str("message expiry is invalid"),
            Self::Database(error) => write!(formatter, "begin-invocation database error: {error}"),
        }
    }
}

impl std::error::Error for BeginInvocationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CallerResolution(error) => Some(error),
            Self::Idempotency(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CallerResolutionError> for BeginInvocationError {
    fn from(error: CallerResolutionError) -> Self {
        Self::CallerResolution(error)
    }
}

impl From<IdempotencyReservationError> for BeginInvocationError {
    fn from(error: IdempotencyReservationError) -> Self {
        Self::Idempotency(error)
    }
}

#[derive(Serialize)]
struct InvocationReceivedPayload {
    #[serde(rename = "type")]
    event_type: &'static str,
    invocation_id: InvocationId,
    object_id: ObjectId,
    caller_id: CallerId,
    caller_kind: &'static str,
    snapshot_id: SnapshotId,
    knowledge_revision: u64,
    invocation_pool: &'static str,
}

impl InvocationReceivedPayload {
    fn new(
        invocation_id: InvocationId,
        object_id: ObjectId,
        caller: ResolvedCaller,
        snapshot_id: SnapshotId,
        knowledge_revision: u64,
        invocation_pool: InvocationPool,
    ) -> Self {
        Self {
            event_type: "invocation_received",
            invocation_id,
            object_id,
            caller_id: caller.caller_id(),
            caller_kind: caller.kind().as_sql(),
            snapshot_id,
            knowledge_revision,
            invocation_pool: invocation_pool.as_sql(),
        }
    }
}

/// Atomically reserves idempotency, resolves Caller, pins snapshot/revision,
/// inserts the invocation, and appends its critical event plus outbox row.
///
/// The transaction is committed before this function returns. Callers must
/// perform provider I/O only after receiving Created. No provider/network call
/// is made from this module.
///
/// The effective Caller is resolved before the idempotency claim because its
/// ID defines the key namespace. The claim is the first invocation write; all
/// later reads/writes remain in the same transaction. Any error before commit
/// drops the SQLx transaction and rolls back the claim and every subsequent row.
pub async fn begin_invocation(
    pool: &PgPool,
    scope: &Scope,
    command: BeginInvocationCommand,
) -> Result<BeginInvocationOutcome, BeginInvocationError> {
    if command.input_message.trim().is_empty() || command.input_message.len() > 65_536 {
        return Err(BeginInvocationError::InvalidInputMessage);
    }
    let message_expiry_micros = command.message_expires_at
        .duration_since(UNIX_EPOCH)
        .map_err(|_| BeginInvocationError::InvalidMessageExpiry)?
        .as_micros();
    let message_expiry_micros = i64::try_from(message_expiry_micros)
        .map_err(|_| BeginInvocationError::InvalidMessageExpiry)?;
    if command.message_expires_at <= SystemTime::now() {
        return Err(BeginInvocationError::InvalidMessageExpiry);
    }

    let mut transaction = begin_tenant_transaction(pool, scope)
        .await
        .map_err(BeginInvocationError::Database)?;

    let caller = resolve_delegated_caller(&mut transaction, command.requested_caller_id).await?;

    let proposed_invocation_id = InvocationId::new();
    let claim = IdempotencyClaim::new(
        command.object_id,
        &command.idempotency_key,
        &command.payload_hash,
        proposed_invocation_id,
        command.idempotency_expires_at,
    );
    let reservation =
        reserve_invocation_idempotency(&mut transaction, scope, &caller, &claim).await?;

    let invocation_id = match reservation {
        IdempotencyReservation::Replay { invocation_id } => {
            transaction
                .commit()
                .await
                .map_err(BeginInvocationError::Database)?;
            return Ok(BeginInvocationOutcome::Replay { invocation_id });
        }
        IdempotencyReservation::Reserved { invocation_id } => invocation_id,
    };

    // Lock the object pointer while checking the snapshot. Publication locks
    // the same row first, so this transaction observes one consistent version.
    let object = fetch_optional(
        &mut transaction,
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT state, published_version_id::text FROM objects WHERE tenant_id = $1::uuid AND object_id = $2::uuid FOR SHARE",
        )
        .bind(scope.tenant_id().to_string())
        .bind(command.object_id.to_string()),
    )
    .await
    .map_err(BeginInvocationError::Database)?
    .ok_or(BeginInvocationError::ObjectNotFound)?;

    if object.0 != "active" {
        return Err(BeginInvocationError::ObjectNotActive);
    }

    let snapshot_version = fetch_optional(
        &mut transaction,
        sqlx::query_as::<_, (String, String)>(
            "SELECT snapshot.object_version_id::text, version.status FROM snapshots AS snapshot INNER JOIN object_versions AS version ON version.tenant_id = snapshot.tenant_id AND version.object_id = snapshot.object_id AND version.object_version_id = snapshot.object_version_id WHERE snapshot.tenant_id = $1::uuid AND snapshot.object_id = $2::uuid AND snapshot.snapshot_id = $3::uuid",
        )
        .bind(scope.tenant_id().to_string())
        .bind(command.object_id.to_string())
        .bind(command.snapshot_id.to_string()),
    )
    .await
    .map_err(BeginInvocationError::Database)?
    .ok_or(BeginInvocationError::SnapshotNotFound)?;

    if object.1.as_deref() != Some(snapshot_version.0.as_str()) || snapshot_version.1 != "published"
    {
        return Err(BeginInvocationError::SnapshotIsNotCurrentPublishedVersion);
    }

    // FOR SHARE pins the current revision against concurrent knowledge commits
    // until this invocation transaction commits.
    let revision = fetch_optional(
        &mut transaction,
        sqlx::query_as::<_, (i64,)>(
            "SELECT current_revision FROM knowledge_revisions WHERE tenant_id = $1::uuid AND object_id = $2::uuid FOR SHARE",
        )
        .bind(scope.tenant_id().to_string())
        .bind(command.object_id.to_string()),
    )
    .await
    .map_err(BeginInvocationError::Database)?
    .ok_or(BeginInvocationError::KnowledgeRevisionNotFound)?;
    let knowledge_revision =
        u64::try_from(revision.0).map_err(|_| BeginInvocationError::KnowledgeRevisionOutOfRange)?;

    execute(
        &mut transaction,
        sqlx::query(
            "INSERT INTO object_invocations (id, tenant_id, object_id, caller_id, snapshot_id, knowledge_revision, pool, status, trace_id) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, $5::uuid, $6, $7, 'Received', $8::uuid)",
        )
        .bind(invocation_id.to_string())
        .bind(scope.tenant_id().to_string())
        .bind(command.object_id.to_string())
        .bind(caller.caller_id().to_string())
        .bind(command.snapshot_id.to_string())
        .bind(i64::try_from(knowledge_revision).map_err(|_| BeginInvocationError::KnowledgeRevisionOutOfRange)?)
        .bind(command.invocation_pool.as_sql())
        .bind(command.trace_id.to_string()),
    )
    .await
    .map_err(BeginInvocationError::Database)?;

    execute(
        &mut transaction,
        sqlx::query(
            "INSERT INTO interaction_messages (id, tenant_id, object_id, caller_id, invocation_id, role, content, expires_at) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, $5::uuid, 'user', $6, to_timestamp($7::double precision / 1000000.0))",
        )
        .bind(MessageId::new().to_string())
        .bind(scope.tenant_id().to_string())
        .bind(command.object_id.to_string())
        .bind(caller.caller_id().to_string())
        .bind(invocation_id.to_string())
        .bind(&command.input_message)
        .bind(message_expiry_micros as f64),
    )
    .await
    .map_err(BeginInvocationError::Database)?;

    append_invocation_received(
        &mut transaction,
        scope,
        EventId::new(),
        invocation_id,
        &command,
        caller,
        knowledge_revision,
    )
    .await
    .map_err(BeginInvocationError::Database)?;

    transaction
        .commit()
        .await
        .map_err(BeginInvocationError::Database)?;

    Ok(BeginInvocationOutcome::Created {
        invocation_id,
        caller_id: caller.caller_id(),
        caller_kind: caller.kind(),
        snapshot_id: command.snapshot_id,
        knowledge_revision,
        invocation_pool: command.invocation_pool,
    })
}

async fn append_invocation_received(
    transaction: &mut TenantTransaction,
    scope: &Scope,
    event_id: EventId,
    invocation_id: InvocationId,
    command: &BeginInvocationCommand,
    caller: ResolvedCaller,
    knowledge_revision: u64,
) -> Result<(), sqlx::Error> {
    let payload = serde_json::to_string(&InvocationReceivedPayload::new(
        invocation_id,
        command.object_id,
        caller,
        command.snapshot_id,
        knowledge_revision,
        command.invocation_pool,
    ))
    .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;

    execute(
        transaction,
        sqlx::query(
            "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload) VALUES ($1::uuid, $2::uuid, 'invocation', $3::uuid, 'invocation.received', $4::jsonb)",
        )
        .bind(scope.tenant_id().to_string())
        .bind(event_id.to_string())
        .bind(invocation_id.to_string())
        .bind(payload),
    )
    .await?;

    execute(
        transaction,
        sqlx::query(
            "INSERT INTO outbox_messages (tenant_id, delivery_id, event_id, topic, payload) SELECT tenant_id, event_id, event_id, event_type, payload FROM events WHERE tenant_id = $1::uuid AND event_id = $2::uuid",
        )
        .bind(scope.tenant_id().to_string())
        .bind(event_id.to_string()),
    )
    .await?;

    Ok(())
}
