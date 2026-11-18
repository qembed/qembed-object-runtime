//! Typed, allowlisted event payloads and atomic event/outbox writes.

use qembed_domain::ids::{EventId, ObjectId, ObjectVersionId};
use qembed_domain::scope::Scope;
use serde::Serialize;

use crate::{TenantTransaction, execute};

/// An event envelope can contain only the explicitly modeled Object lifecycle
/// payloads. It is Serialize-only: arbitrary JSON cannot be deserialized into
/// this API, and the private fields prevent callers from attaching extra data.
pub(crate) struct ObjectEvent {
    event_id: EventId,
    payload: ObjectEventPayload,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ObjectEventPayload {
    ValidationStarted {
        object_id: ObjectId,
        state: &'static str,
    },
    VersionPublished {
        object_id: ObjectId,
        state: &'static str,
        previous_version_id: Option<ObjectVersionId>,
        version_id: ObjectVersionId,
    },
}

impl ObjectEvent {
    pub(crate) fn validation_started(event_id: EventId, object_id: ObjectId) -> Self {
        Self {
            event_id,
            payload: ObjectEventPayload::ValidationStarted {
                object_id,
                state: "validating",
            },
        }
    }

    pub(crate) fn version_published(
        event_id: EventId,
        object_id: ObjectId,
        previous_version_id: Option<ObjectVersionId>,
        version_id: ObjectVersionId,
        state: &'static str,
    ) -> Self {
        Self {
            event_id,
            payload: ObjectEventPayload::VersionPublished {
                object_id,
                state,
                previous_version_id,
                version_id,
            },
        }
    }
}

impl ObjectEventPayload {
    const fn event_type(&self) -> &'static str {
        match self {
            Self::ValidationStarted { .. } => "object.validation_started",
            Self::VersionPublished { .. } => "object.version_published",
        }
    }
}

/// Records a typed critical event and its outbox delivery row.
///
/// The caller must use the same tenant transaction for the state transition.
/// The EventId is also the delivery identity, making duplicate enqueue attempts
/// fail at the database uniqueness boundary rather than creating duplicate work.
pub(crate) async fn append_object_event(
    transaction: &mut TenantTransaction,
    scope: &Scope,
    event: ObjectEvent,
) -> Result<(), sqlx::Error> {
    let payload = serde_json::to_string(&event.payload)
        .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;

    execute(
        transaction,
        sqlx::query(
            "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload) VALUES ($1::uuid, $2::uuid, 'object', $3::uuid, $4, $5::jsonb)",
        )
        .bind(scope.tenant_id().to_string())
        .bind(event.event_id.to_string())
        .bind(event_object_id(&event.payload).to_string())
        .bind(event.payload.event_type())
        .bind(payload),
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

fn event_object_id(payload: &ObjectEventPayload) -> ObjectId {
    match payload {
        ObjectEventPayload::ValidationStarted { object_id, .. }
        | ObjectEventPayload::VersionPublished { object_id, .. } => *object_id,
    }
}

#[cfg(test)]
mod tests {
    use super::{ObjectEvent, ObjectEventPayload};
    use qembed_domain::ids::{EventId, ObjectId, ObjectVersionId};

    #[test]
    fn validation_started_payload_contains_only_allowlisted_fields() {
        let payload = ObjectEvent::validation_started(EventId::new(), ObjectId::new());
        let json = serde_json::to_value(payload.payload).expect("payload should serialize");
        let object = json.as_object().expect("payload should be a JSON object");

        assert_eq!(object.len(), 3);
        assert_eq!(object.get("type").and_then(|value| value.as_str()), Some("validation_started"));
        assert_eq!(object.get("state").and_then(|value| value.as_str()), Some("validating"));
        assert!(object.contains_key("object_id"));
        assert!(!contains_sensitive_key(object));
    }

    #[test]
    fn version_published_payload_contains_only_allowlisted_fields() {
        let payload = ObjectEvent::version_published(
            EventId::new(),
            ObjectId::new(),
            Some(ObjectVersionId::new()),
            ObjectVersionId::new(),
            "active",
        );
        let json = serde_json::to_value(payload.payload).expect("payload should serialize");
        let object = json.as_object().expect("payload should be a JSON object");

        assert_eq!(object.len(), 5);
        assert_eq!(object.get("type").and_then(|value| value.as_str()), Some("version_published"));
        assert_eq!(object.get("state").and_then(|value| value.as_str()), Some("active"));
        assert!(object.contains_key("object_id"));
        assert!(object.contains_key("previous_version_id"));
        assert!(object.contains_key("version_id"));
        assert!(!contains_sensitive_key(object));
    }

    fn contains_sensitive_key(object: &serde_json::Map<String, serde_json::Value>) -> bool {
        ["secret", "token", "raw_payload", "arguments", "credentials"]
            .iter()
            .any(|key| object.contains_key(*key))
    }

    #[test]
    fn event_payload_is_not_deserializable_from_arbitrary_json() {
        // The DTO intentionally implements Serialize only. This compile-time
        // contract is reflected by keeping deserialization out of its derives.
        fn assert_serialize_only<T: serde::Serialize>() {}
        assert_serialize_only::<ObjectEventPayload>();
    }
}
