//! Atomic append-only event and transactional outbox writes.

use qembed_domain::ids::{EventId, ObjectId, ObjectVersionId};
use qembed_domain::scope::Scope;

use crate::{TenantTransaction, execute};

/// Data needed to append one Object lifecycle event.
pub(crate) struct ObjectEvent {
    pub(crate) event_id: EventId,
    pub(crate) object_id: ObjectId,
    pub(crate) event_type: &'static str,
    pub(crate) state: &'static str,
    pub(crate) previous_version_id: Option<ObjectVersionId>,
    pub(crate) version_id: Option<ObjectVersionId>,
}

/// Records a critical Object lifecycle event and its outbox delivery row.
///
/// The caller must use the same tenant transaction for the state transition.
/// The EventId is also the delivery identity, making duplicate enqueue attempts
/// fail at the database uniqueness boundary rather than creating duplicate work.
pub(crate) async fn append_object_event(
    transaction: &mut TenantTransaction,
    scope: &Scope,
    event: ObjectEvent,
) -> Result<(), sqlx::Error> {
    execute(
        transaction,
        sqlx::query(
            "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload) VALUES ($1::uuid, $2::uuid, 'object', $3::uuid, $4, jsonb_build_object('object_id', $3::uuid, 'state', $5, 'previous_version_id', $6::uuid, 'version_id', $7::uuid))",
        )
        .bind(scope.tenant_id().to_string())
        .bind(event.event_id.to_string())
        .bind(event.object_id.to_string())
        .bind(event.event_type)
        .bind(event.state)
        .bind(event.previous_version_id.map(|id| id.to_string()))
        .bind(event.version_id.map(|id| id.to_string())),
    )
    .await?;

    execute(
        transaction,
        sqlx::query(
            "INSERT INTO outbox_messages (tenant_id, delivery_id, event_id, topic, payload) SELECT tenant_id, event_id, event_id, event_type, payload FROM events WHERE tenant_id = $1::uuid AND event_id = $2::uuid",
        )
        .bind(scope.tenant_id().to_string())
        .bind(event.event_id.to_string()),
    )
    .await?;

    Ok(())
}
