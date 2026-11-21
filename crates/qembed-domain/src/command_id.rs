//! Stable command identity derived from an invocation and its logical action.
//!
//! The input action hash is the SHA-256 digest of the exact action payload. This
//! module deliberately does not choose how actions are canonicalized or hashed;
//! callers must supply the digest produced by the action-contract boundary.

use uuid::Uuid;

use crate::ids::{CommandId, InvocationId};

/// Derives a UUIDv5 command ID from its durable logical identity.
///
/// The invocation UUID is the UUIDv5 namespace. The name is the versioned domain
/// separator, the zero-based step index encoded as an unsigned 32-bit big-endian
/// integer, and the 32-byte SHA-256 action digest. The encoding is unambiguous
/// and independent of process, worker, clock, or host state. UUIDv5 is an
/// identifier format, not a secret or authorization proof.
///
/// Changing the encoding requires a new domain-separator version. Persisted
/// command IDs must never be silently re-derived with a different version.
#[must_use]
pub fn derive_command_id(
    invocation_id: InvocationId,
    step_index: u32,
    action_hash: &[u8; 32],
) -> CommandId {
    const NAME_PREFIX: &[u8] = b"qembed.command-id.v1\0";

    let mut name = Vec::with_capacity(NAME_PREFIX.len() + 4 + action_hash.len());
    name.extend_from_slice(NAME_PREFIX);
    name.extend_from_slice(&step_index.to_be_bytes());
    name.extend_from_slice(action_hash);

    CommandId::from_uuid(Uuid::new_v5(invocation_id.as_uuid(), &name))
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crate::ids::InvocationId;

    use super::derive_command_id;

    #[test]
    fn uuid_v5_command_id_matches_the_versioned_deterministic_vector() {
        let invocation_id = InvocationId::from_uuid(
            Uuid::parse_str("00000000-0000-0000-0000-000000000001")
                .expect("fixed invocation UUID is valid"),
        );
        let action_hash = [0x5a; 32];

        let command_id = derive_command_id(invocation_id, 7, &action_hash);

        assert_eq!(
            command_id.to_string(),
            "51838ecd-f956-5415-b688-d7da15e41699"
        );
    }

    #[test]
    fn command_id_is_reproducible_after_reconstructing_persisted_inputs() {
        let invocation_uuid =
            Uuid::parse_str("7b8e2c1a-6f31-4d2e-9a10-123456789abc").expect("valid UUID");
        let invocation_id = InvocationId::from_uuid(invocation_uuid);
        let step_index = 12_u32;
        let action_hash = [0xa5; 32];
        let before_restart = derive_command_id(invocation_id, step_index, &action_hash);

        // Persist and reconstruct every input that defines the logical command.
        let persisted_invocation_id = invocation_id.to_string();
        let persisted_step_index = step_index.to_be_bytes();
        let persisted_action_hash = action_hash.to_vec();

        let restored_invocation_id = InvocationId::from_uuid(
            persisted_invocation_id
                .parse()
                .expect("persisted UUID parses"),
        );
        let restored_step_index = u32::from_be_bytes(persisted_step_index);
        let restored_action_hash: [u8; 32] = persisted_action_hash
            .try_into()
            .expect("persisted SHA-256 digest has 32 bytes");
        let after_restart = derive_command_id(
            restored_invocation_id,
            restored_step_index,
            &restored_action_hash,
        );

        assert_eq!(before_restart, after_restart);
    }

    #[test]
    fn distinct_invocations_steps_or_actions_produce_distinct_command_ids() {
        let invocation_a = InvocationId::from_uuid(
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("valid UUID"),
        );
        let invocation_b = InvocationId::from_uuid(
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").expect("valid UUID"),
        );
        let action_a = [0x11; 32];
        let action_b = [0x22; 32];

        let baseline = derive_command_id(invocation_a, 0, &action_a);
        let variants = [
            derive_command_id(invocation_b, 0, &action_a),
            derive_command_id(invocation_a, 1, &action_a),
            derive_command_id(invocation_a, 0, &action_b),
        ];

        for variant in variants {
            assert_ne!(baseline, variant);
        }
    }
}
