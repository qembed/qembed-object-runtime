//! Envelope encryption for short-lived tool command arguments.

use qembed_domain::{
    command_payload::{CommandPayloadStore, EncryptedCommandPayload},
    ids::{CommandId, TenantId},
    scope::Scope,
};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use sha2::{Digest, Sha256};

const MAGIC: &[u8; 4] = b"QCP1";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// Plaintext data key. Key bytes are overwritten when the value is dropped.
pub struct DataKey([u8; 32]);

impl DataKey {
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl Drop for DataKey {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Provider-neutral wrapping boundary. Implementations can use KMS, HSM,
/// local key agents or test keys. Raw DEKs are never persisted.
pub trait DataKeyProvider: Send + Sync {
    /// Generates a fresh DEK and wraps it under the active KEK.
    fn generate_and_wrap(&self) -> Result<(String, DataKey, Vec<u8>), KeyProviderError>;

    /// Unwraps a persisted DEK using the key identified by key_id.
    fn unwrap(&self, key_id: &str, wrapped_key: &[u8]) -> Result<DataKey, KeyProviderError>;
}

/// Provider failure without exposing key material in diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyProviderError;

impl std::fmt::Display for KeyProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("data-key provider operation failed")
    }
}

impl std::error::Error for KeyProviderError {}

/// Failure to encrypt, parse, authenticate or verify a command payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadCryptoError {
    KeyProvider,
    Randomness,
    InvalidKeyId,
    InvalidEnvelope,
    AuthenticationFailed,
    ArgumentsHashMismatch,
}

impl std::fmt::Display for PayloadCryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::KeyProvider => "data-key provider operation failed",
            Self::Randomness => "secure random generation failed",
            Self::InvalidKeyId => "invalid data-key identifier",
            Self::InvalidEnvelope => "invalid encrypted command payload envelope",
            Self::AuthenticationFailed => "encrypted command payload authentication failed",
            Self::ArgumentsHashMismatch => {
                "command arguments hash does not match the Allow decision"
            }
        })
    }
}

impl std::error::Error for PayloadCryptoError {}

/// Encrypts argument bytes only when their SHA-256 matches the Allow decision.
pub fn encrypt_command_payload<P: DataKeyProvider>(
    provider: &P,
    tenant: TenantId,
    command: CommandId,
    plaintext: &[u8],
    allow_hash: [u8; 32],
) -> Result<EncryptedCommandPayload, PayloadCryptoError> {
    let digest: [u8; 32] = Sha256::digest(plaintext).into();
    if digest != allow_hash {
        return Err(PayloadCryptoError::ArgumentsHashMismatch);
    }

    let (key_id, data_key, wrapped) = provider
        .generate_and_wrap()
        .map_err(|_| PayloadCryptoError::KeyProvider)?;
    if key_id.is_empty() || key_id.len() > u16::MAX as usize {
        return Err(PayloadCryptoError::InvalidKeyId);
    }
    if wrapped.is_empty() || wrapped.len() > u16::MAX as usize {
        return Err(PayloadCryptoError::InvalidEnvelope);
    }

    let mut nonce_bytes = [0_u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| PayloadCryptoError::Randomness)?;

    let aad = associated_data(tenant, command, &key_id, &allow_hash);
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &data_key.0)
        .map_err(|_| PayloadCryptoError::InvalidEnvelope)?;
    let key = aead::LessSafeKey::new(unbound);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
    let mut ciphertext = plaintext.to_vec();
    key.seal_in_place_append_tag(nonce, aead::Aad::from(aad), &mut ciphertext)
        .map_err(|_| PayloadCryptoError::AuthenticationFailed)?;

    let mut envelope = Vec::with_capacity(6 + wrapped.len() + NONCE_LEN + ciphertext.len());
    envelope.extend_from_slice(MAGIC);
    envelope.extend_from_slice(&(wrapped.len() as u16).to_be_bytes());
    envelope.extend_from_slice(&wrapped);
    envelope.extend_from_slice(&nonce_bytes);
    envelope.extend_from_slice(&ciphertext);

    Ok(EncryptedCommandPayload::new(envelope, key_id, allow_hash))
}

/// Authenticates and decrypts, then independently verifies the Allow hash.
pub fn decrypt_command_payload<P: DataKeyProvider>(
    provider: &P,
    tenant: TenantId,
    command: CommandId,
    payload: &EncryptedCommandPayload,
    allow_hash: [u8; 32],
) -> Result<Vec<u8>, PayloadCryptoError> {
    if payload.arguments_hash() != &allow_hash {
        return Err(PayloadCryptoError::ArgumentsHashMismatch);
    }
    if payload.key_id().is_empty() || payload.key_id().len() > u16::MAX as usize {
        return Err(PayloadCryptoError::InvalidKeyId);
    }

    let bytes = payload.ciphertext();
    if bytes.len() < 6 + NONCE_LEN + TAG_LEN || &bytes[..4] != MAGIC {
        return Err(PayloadCryptoError::InvalidEnvelope);
    }
    let wrapped_len = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
    let wrapped_end = 6_usize
        .checked_add(wrapped_len)
        .ok_or(PayloadCryptoError::InvalidEnvelope)?;
    let nonce_end = wrapped_end
        .checked_add(NONCE_LEN)
        .ok_or(PayloadCryptoError::InvalidEnvelope)?;
    if wrapped_len == 0
        || nonce_end
            .checked_add(TAG_LEN)
            .is_none_or(|end| end > bytes.len())
    {
        return Err(PayloadCryptoError::InvalidEnvelope);
    }

    let wrapped = &bytes[6..wrapped_end];
    let nonce_bytes: [u8; NONCE_LEN] = bytes[wrapped_end..nonce_end]
        .try_into()
        .map_err(|_| PayloadCryptoError::InvalidEnvelope)?;
    let mut ciphertext = bytes[nonce_end..].to_vec();
    let data_key = provider
        .unwrap(payload.key_id(), wrapped)
        .map_err(|_| PayloadCryptoError::KeyProvider)?;
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &data_key.0)
        .map_err(|_| PayloadCryptoError::InvalidEnvelope)?;
    let key = aead::LessSafeKey::new(unbound);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
    let aad = associated_data(tenant, command, payload.key_id(), &allow_hash);
    let plaintext = key
        .open_in_place(nonce, aead::Aad::from(aad), &mut ciphertext)
        .map_err(|_| PayloadCryptoError::AuthenticationFailed)?
        .to_vec();

    let digest: [u8; 32] = Sha256::digest(&plaintext).into();
    if digest != allow_hash {
        return Err(PayloadCryptoError::ArgumentsHashMismatch);
    }
    Ok(plaintext)
}

/// Re-encrypts under the provider's currently active wrapping key.
pub fn rotate_command_payload<P: DataKeyProvider>(
    provider: &P,
    tenant: TenantId,
    command: CommandId,
    payload: &EncryptedCommandPayload,
    allow_hash: [u8; 32],
) -> Result<EncryptedCommandPayload, PayloadCryptoError> {
    let plaintext = decrypt_command_payload(provider, tenant, command, payload, allow_hash)?;
    encrypt_command_payload(provider, tenant, command, &plaintext, allow_hash)
}

fn associated_data(
    tenant: TenantId,
    command: CommandId,
    key_id: &str,
    hash: &[u8; 32],
) -> Vec<u8> {
    let key_id_bytes = key_id.as_bytes();
    let mut aad = Vec::with_capacity(66 + key_id_bytes.len());
    aad.extend_from_slice(tenant.as_uuid().as_bytes());
    aad.extend_from_slice(command.as_uuid().as_bytes());
    aad.extend_from_slice(&(key_id_bytes.len() as u16).to_be_bytes());
    aad.extend_from_slice(key_id_bytes);
    aad.extend_from_slice(hash);
    aad
}

/// Workflow failure while persisting or retrieving an authorized payload.
#[derive(Debug)]
pub enum CommandPayloadWorkflowError<E> {
    Store(E),
    Crypto(PayloadCryptoError),
}

/// Encrypts arguments and persists only the authenticated envelope.
/// The digest must be taken from the immutable Allow decision.
pub async fn persist_authorized_command_payload<S, P>(
    store: &S,
    provider: &P,
    scope: &Scope,
    command_id: CommandId,
    plaintext: &[u8],
    allow_arguments_hash: [u8; 32],
    ttl_seconds: u32,
) -> Result<(), CommandPayloadWorkflowError<S::Error>>
where
    S: CommandPayloadStore,
    P: DataKeyProvider,
{
    let encrypted = encrypt_command_payload(
        provider,
        scope.tenant_id(),
        command_id,
        plaintext,
        allow_arguments_hash,
    )
    .map_err(CommandPayloadWorkflowError::Crypto)?;
    store
        .store_command_payload(scope, command_id, encrypted, ttl_seconds)
        .await
        .map_err(CommandPayloadWorkflowError::Store)
}

/// Returns plaintext only after the Store confirms the Allow hash and AEAD plus
/// SHA-256 verification succeed. Call this immediately before executor handoff.
pub async fn load_authorized_command_payload<S, P>(
    store: &S,
    provider: &P,
    scope: &Scope,
    command_id: CommandId,
    allow_arguments_hash: [u8; 32],
) -> Result<Option<Vec<u8>>, CommandPayloadWorkflowError<S::Error>>
where
    S: CommandPayloadStore,
    P: DataKeyProvider,
{
    let Some(encrypted) = store
        .load_command_payload(scope, command_id, allow_arguments_hash)
        .await
        .map_err(CommandPayloadWorkflowError::Store)?
    else {
        return Ok(None);
    };

    decrypt_command_payload(
        provider,
        scope.tenant_id(),
        command_id,
        &encrypted,
        allow_arguments_hash,
    )
    .map(Some)
    .map_err(CommandPayloadWorkflowError::Crypto)
}

/// Re-encrypts an existing record under the provider's active key without
/// changing command identity, plaintext or the Allow-decision digest.
pub async fn rotate_persisted_command_payload<S, P>(
    store: &S,
    provider: &P,
    scope: &Scope,
    command_id: CommandId,
    allow_arguments_hash: [u8; 32],
) -> Result<bool, CommandPayloadWorkflowError<S::Error>>
where
    S: CommandPayloadStore,
    P: DataKeyProvider,
{
    let Some(existing) = store
        .load_command_payload(scope, command_id, allow_arguments_hash)
        .await
        .map_err(CommandPayloadWorkflowError::Store)?
    else {
        return Ok(false);
    };

    let rotated = rotate_command_payload(
        provider,
        scope.tenant_id(),
        command_id,
        &existing,
        allow_arguments_hash,
    )
    .map_err(CommandPayloadWorkflowError::Crypto)?;
    store
        .store_command_payload(scope, command_id, rotated, 1)
        .await
        .map_err(CommandPayloadWorkflowError::Store)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{Arc, RwLock},
    };

    #[derive(Clone)]
    struct TestProvider {
        active: Arc<RwLock<String>>,
        keys: Arc<RwLock<HashMap<String, [u8; 32]>>>,
    }

    impl TestProvider {
        fn new() -> Self {
            let mut key = [0; 32];
            SystemRandom::new().fill(&mut key).unwrap();
            let mut keys = HashMap::new();
            keys.insert("kek-1".into(), key);
            Self {
                active: Arc::new(RwLock::new("kek-1".into())),
                keys: Arc::new(RwLock::new(keys)),
            }
        }

        fn rotate(&self) {
            let mut key = [0; 32];
            SystemRandom::new().fill(&mut key).unwrap();
            let id = format!("kek-{}", self.keys.read().unwrap().len() + 1);
            self.keys.write().unwrap().insert(id.clone(), key);
            *self.active.write().unwrap() = id;
        }

        fn wrap(kek: &[u8; 32], dek: &[u8; 32]) -> Vec<u8> {
            dek.iter()
                .zip(kek)
                .map(|(left, right)| *left ^ *right)
                .collect()
        }
    }

    impl DataKeyProvider for TestProvider {
        fn generate_and_wrap(&self) -> Result<(String, DataKey, Vec<u8>), KeyProviderError> {
            let id = self
                .active
                .read()
                .map_err(|_| KeyProviderError)?
                .clone();
            let kek = *self
                .keys
                .read()
                .map_err(|_| KeyProviderError)?
                .get(&id)
                .ok_or(KeyProviderError)?;
            let mut dek = [0; 32];
            SystemRandom::new()
                .fill(&mut dek)
                .map_err(|_| KeyProviderError)?;
            Ok((id, DataKey::new(dek), Self::wrap(&kek, &dek)))
        }

        fn unwrap(&self, id: &str, wrapped: &[u8]) -> Result<DataKey, KeyProviderError> {
            let kek = *self
                .keys
                .read()
                .map_err(|_| KeyProviderError)?
                .get(id)
                .ok_or(KeyProviderError)?;
            if wrapped.len() != 32 {
                return Err(KeyProviderError);
            }
            let mut dek = [0; 32];
            for (index, (left, right)) in wrapped.iter().zip(kek).enumerate() {
                dek[index] = *left ^ right;
            }
            Ok(DataKey::new(dek))
        }
    }

    #[test]
    fn round_trip_and_no_plaintext_in_ciphertext() {
        let provider = TestProvider::new();
        let tenant = TenantId::new();
        let command = CommandId::new();
        let plaintext = b"private argument";
        let hash = Sha256::digest(plaintext).into();
        let encrypted =
            encrypt_command_payload(&provider, tenant, command, plaintext, hash).unwrap();

        assert!(
            !encrypted
                .ciphertext()
                .windows(7)
                .any(|window| window == b"private")
        );
        assert_eq!(
            decrypt_command_payload(&provider, tenant, command, &encrypted, hash).unwrap(),
            plaintext
        );
    }

    #[test]
    fn tampering_and_wrong_associated_identity_are_rejected() {
        let provider = TestProvider::new();
        let tenant = TenantId::new();
        let command = CommandId::new();
        let plaintext = b"secret";
        let hash = Sha256::digest(plaintext).into();
        let encrypted =
            encrypt_command_payload(&provider, tenant, command, plaintext, hash).unwrap();
        let mut bytes = encrypted.ciphertext().to_vec();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        let tampered =
            EncryptedCommandPayload::new(bytes, encrypted.key_id().into(), hash);

        assert_eq!(
            decrypt_command_payload(&provider, tenant, command, &tampered, hash),
            Err(PayloadCryptoError::AuthenticationFailed)
        );
        assert_eq!(
            decrypt_command_payload(&provider, TenantId::new(), command, &encrypted, hash),
            Err(PayloadCryptoError::AuthenticationFailed)
        );
        assert_eq!(
            decrypt_command_payload(&provider, tenant, CommandId::new(), &encrypted, hash),
            Err(PayloadCryptoError::AuthenticationFailed)
        );
    }

    #[test]
    fn wrong_key_and_allow_hash_mismatch_fail_closed() {
        let provider = TestProvider::new();
        let tenant = TenantId::new();
        let command = CommandId::new();
        let plaintext = b"payload";
        let hash = Sha256::digest(plaintext).into();
        let encrypted =
            encrypt_command_payload(&provider, tenant, command, plaintext, hash).unwrap();

        assert_eq!(
            decrypt_command_payload(&TestProvider::new(), tenant, command, &encrypted, hash),
            Err(PayloadCryptoError::KeyProvider)
        );
        assert_eq!(
            encrypt_command_payload(&provider, tenant, command, plaintext, [0; 32]),
            Err(PayloadCryptoError::ArgumentsHashMismatch)
        );
        assert_eq!(
            decrypt_command_payload(&provider, tenant, command, &encrypted, [0; 32]),
            Err(PayloadCryptoError::ArgumentsHashMismatch)
        );
    }

    #[test]
    fn rotation_uses_new_key_and_preserves_old_decryptability() {
        let provider = TestProvider::new();
        let tenant = TenantId::new();
        let command = CommandId::new();
        let plaintext = b"rotate me";
        let hash = Sha256::digest(plaintext).into();
        let first =
            encrypt_command_payload(&provider, tenant, command, plaintext, hash).unwrap();
        provider.rotate();
        let next = rotate_command_payload(&provider, tenant, command, &first, hash).unwrap();

        assert_ne!(first.key_id(), next.key_id());
        assert_eq!(
            decrypt_command_payload(&provider, tenant, command, &next, hash).unwrap(),
            plaintext
        );
        assert_eq!(
            decrypt_command_payload(&provider, tenant, command, &first, hash).unwrap(),
            plaintext
        );
    }

    #[derive(Default)]
    struct MemoryStore(std::sync::Mutex<Option<EncryptedCommandPayload>>);

    impl CommandPayloadStore for MemoryStore {
        type Error = std::io::Error;

        async fn store_command_payload(
            &self,
            _scope: &Scope,
            _command: CommandId,
            payload: EncryptedCommandPayload,
            _ttl: u32,
        ) -> Result<(), Self::Error> {
            *self.0.lock().expect("memory store lock") = Some(payload);
            Ok(())
        }

        async fn load_command_payload(
            &self,
            _scope: &Scope,
            _command: CommandId,
            expected: [u8; 32],
        ) -> Result<Option<EncryptedCommandPayload>, Self::Error> {
            Ok(self
                .0
                .lock()
                .expect("memory store lock")
                .clone()
                .filter(|payload| payload.arguments_hash() == &expected))
        }
    }

    #[tokio::test]
    async fn persistence_workflow_never_stores_plaintext_and_loads_only_verified_arguments() {
        use qembed_domain::{ids::TenantId, test_support::scope_for_tenant};

        let provider = TestProvider::new();
        let store = MemoryStore::default();
        let scope = scope_for_tenant(TenantId::new());
        let command = CommandId::new();
        let plaintext = b"highly sensitive command arguments";
        let hash = Sha256::digest(plaintext).into();

        persist_authorized_command_payload(
            &store, &provider, &scope, command, plaintext, hash, 300,
        )
        .await
        .expect("persist");

        let stored = store
            .0
            .lock()
            .expect("memory store lock")
            .clone()
            .expect("stored ciphertext");
        assert!(
            !stored
                .ciphertext()
                .windows(plaintext.len())
                .any(|window| window == plaintext)
        );
        assert_eq!(
            load_authorized_command_payload(&store, &provider, &scope, command, hash)
                .await
                .expect("load")
                .expect("payload"),
            plaintext
        );
        assert!(
            load_authorized_command_payload(&store, &provider, &scope, command, [0; 32])
                .await
                .expect("wrong Allow hash")
                .is_none()
        );
    }

    #[tokio::test]
    async fn persistence_workflow_rejects_tampered_ciphertext_before_returning_plaintext() {
        use qembed_domain::{ids::TenantId, test_support::scope_for_tenant};

        let provider = TestProvider::new();
        let store = MemoryStore::default();
        let scope = scope_for_tenant(TenantId::new());
        let command = CommandId::new();
        let plaintext = b"do not execute tampered arguments";
        let hash = Sha256::digest(plaintext).into();

        persist_authorized_command_payload(
            &store, &provider, &scope, command, plaintext, hash, 300,
        )
        .await
        .expect("persist");

        let mut record = store.0.lock().expect("memory store lock");
        let current = record.take().expect("stored");
        let mut bytes = current.ciphertext().to_vec();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        *record = Some(EncryptedCommandPayload::new(
            bytes,
            current.key_id().to_owned(),
            hash,
        ));
        drop(record);

        assert!(matches!(
            load_authorized_command_payload(&store, &provider, &scope, command, hash).await,
            Err(CommandPayloadWorkflowError::Crypto(
                PayloadCryptoError::AuthenticationFailed
            ))
        ));
    }
}
