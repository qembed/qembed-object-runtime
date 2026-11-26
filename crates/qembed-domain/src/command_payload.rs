//! Crypto-neutral persistence contract for encrypted command payloads.
use crate::{ids::CommandId, scope::Scope};

/// Ciphertext envelope persisted for one Allow-authorized command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptedCommandPayload {
    ciphertext: Vec<u8>,
    key_id: String,
    arguments_hash: [u8; 32],
}
impl EncryptedCommandPayload {
    #[must_use]
    pub fn new(ciphertext: Vec<u8>, key_id: String, arguments_hash: [u8; 32]) -> Self {
        Self { ciphertext, key_id, arguments_hash }
    }
    #[must_use]
    pub fn ciphertext(&self) -> &[u8] { &self.ciphertext }
    #[must_use]
    pub fn key_id(&self) -> &str { &self.key_id }
    #[must_use]
    pub const fn arguments_hash(&self) -> &[u8; 32] { &self.arguments_hash }
}

/// Tenant-scoped persistence port. Implementations must compare the digest to
/// the immutable Allow decision before returning ciphertext.
#[allow(async_fn_in_trait)]
pub trait CommandPayloadStore: Send + Sync {
    type Error: Send;
    async fn store_command_payload(&self, scope: &Scope, command_id: CommandId, payload: EncryptedCommandPayload, ttl_seconds: u32) -> Result<(), Self::Error>;
    async fn load_command_payload(&self, scope: &Scope, command_id: CommandId, expected_arguments_hash: [u8; 32]) -> Result<Option<EncryptedCommandPayload>, Self::Error>;
}
