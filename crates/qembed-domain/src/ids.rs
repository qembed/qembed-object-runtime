//! Strongly typed UUID identifiers.
//!
//! Each identifier has the same UUID representation, but distinct Rust types
//! prevent IDs belonging to different domain entities from being mixed.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! define_id {
    ($name:ident) => {
        #[doc = concat!("A strongly typed identifier for ", stringify!($name), ".")]
        ///
        /// Different ID types are intentionally incompatible:
        ///
        /// ```compile_fail
        /// use qembed_domain::ids::{ObjectId, TenantId};
        ///
        /// fn require_tenant(_: TenantId) {}
        ///
        /// let object_id = ObjectId::new();
        /// require_tenant(object_id);
        /// ```
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a new random UUIDv4 identifier.
            #[must_use]
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Wraps an existing UUID without changing its value.
            #[must_use]
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            /// Borrows the underlying UUID.
            #[must_use]
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            /// Consumes the identifier and returns its underlying UUID.
            #[must_use]
            pub const fn into_uuid(self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

define_id!(TenantId);
define_id!(ObjectId);
define_id!(ObjectVersionId);
define_id!(PrincipalId);
define_id!(CallerId);
define_id!(InvocationId);
define_id!(TraceId);
define_id!(SnapshotId);
define_id!(EventId);
define_id!(CommandId);
define_id!(ToolExecutionId);
define_id!(TaskId);

#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use std::str::FromStr;

    use serde::de::DeserializeOwned;

    use super::*;

    fn assert_round_trip<T>(id: T)
    where
        T: Copy + Debug + PartialEq + fmt::Display + FromStr + Serialize + DeserializeOwned,
        T::Err: Debug,
    {
        let formatted = id.to_string();
        let parsed = formatted.parse::<T>().expect("formatted ID must parse");
        assert_eq!(parsed, id);

        let json = serde_json::to_string(&id).expect("ID must serialize");
        assert_eq!(json, format!("\"{formatted}\""));

        let decoded = serde_json::from_str::<T>(&json).expect("serialized ID must deserialize");
        assert_eq!(decoded, id);
    }

    fn assert_rejects_invalid<T>()
    where
        T: FromStr,
    {
        assert!("".parse::<T>().is_err());
        assert!("not-a-uuid".parse::<T>().is_err());
        assert!("550e8400-e29b-41d4-a716-44665544000z".parse::<T>().is_err());
    }

    #[test]
    fn all_id_types_support_parse_format_and_serde_round_trip() {
        assert_round_trip(TenantId::new());
        assert_round_trip(ObjectId::new());
        assert_round_trip(ObjectVersionId::new());
        assert_round_trip(PrincipalId::new());
        assert_round_trip(CallerId::new());
        assert_round_trip(InvocationId::new());
        assert_round_trip(TraceId::new());
        assert_round_trip(SnapshotId::new());
        assert_round_trip(EventId::new());
        assert_round_trip(CommandId::new());
        assert_round_trip(ToolExecutionId::new());
        assert_round_trip(TaskId::new());
    }

    #[test]
    fn all_id_types_reject_malformed_values() {
        assert_rejects_invalid::<TenantId>();
        assert_rejects_invalid::<ObjectId>();
        assert_rejects_invalid::<ObjectVersionId>();
        assert_rejects_invalid::<PrincipalId>();
        assert_rejects_invalid::<CallerId>();
        assert_rejects_invalid::<InvocationId>();
        assert_rejects_invalid::<TraceId>();
        assert_rejects_invalid::<SnapshotId>();
        assert_rejects_invalid::<EventId>();
        assert_rejects_invalid::<CommandId>();
        assert_rejects_invalid::<ToolExecutionId>();
        assert_rejects_invalid::<TaskId>();
    }

    #[test]
    fn ids_can_wrap_and_recover_an_existing_uuid() {
        let uuid = Uuid::new_v4();
        let id = ObjectId::from_uuid(uuid);

        assert_eq!(id.as_uuid(), &uuid);
        assert_eq!(id.into_uuid(), uuid);
    }

    #[test]
    fn serde_rejects_a_non_uuid_string() {
        assert!(serde_json::from_str::<TenantId>("\"not-a-uuid\"").is_err());
    }
}
