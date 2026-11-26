//! Envelope encryption for short-lived tool command arguments.
use ring::{aead, rand::{SecureRandom, SystemRandom}};
use sha2::{Digest, Sha256};
use qembed_domain::{command_payload::{CommandPayloadStore, EncryptedCommandPayload}, ids::{CommandId, TenantId}, scope::Scope};

const MAGIC: &[u8; 4] = b"QCP1";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// Plaintext data key. Key bytes are cleared when dropped.
pub struct DataKey([u8; 32]);
impl DataKey { #[must_use] pub const fn new(bytes: [u8; 32]) -> Self { Self(bytes) } }
impl Drop for DataKey { fn drop(&mut self) { self.0.fill(0); } }

/// Provider-neutral wrapping boundary. Implementations can use KMS, HSM,
/// local key agents or test keys. Raw DEKs are never persisted.
pub trait DataKeyProvider: Send + Sync {
    fn generate_and_wrap(&self) -> Result<(String, DataKey, Vec<u8>), KeyProviderError>;
    fn unwrap(&self, key_id: &str, wrapped_key: &[u8]) -> Result<DataKey, KeyProviderError>;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyProviderError;
impl std::fmt::Display for KeyProviderError { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("data-key provider operation failed") } }
impl std::error::Error for KeyProviderError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadCryptoError { KeyProvider, Randomness, InvalidKeyId, InvalidEnvelope, AuthenticationFailed, ArgumentsHashMismatch }
impl std::fmt::Display for PayloadCryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::KeyProvider => "data-key provider operation failed",
            Self::Randomness => "secure random generation failed",
            Self::InvalidKeyId => "invalid data-key identifier",
            Self::InvalidEnvelope => "invalid encrypted command payload envelope",
            Self::AuthenticationFailed => "encrypted command payload authentication failed",
            Self::ArgumentsHashMismatch => "command arguments hash does not match the Allow decision",
        })
    }
}
impl std::error::Error for PayloadCryptoError {}

/// Encrypts argument bytes only when their SHA-256 matches the Allow decision.
pub fn encrypt_command_payload<P: DataKeyProvider>(provider: &P, tenant: TenantId, command: CommandId, plaintext: &[u8], allow_hash: [u8; 32]) -> Result<EncryptedCommandPayload, PayloadCryptoError> {
    let digest: [u8; 32] = Sha256::digest(plaintext).into();
    if digest != allow_hash { return Err(PayloadCryptoError::ArgumentsHashMismatch); }
    let (key_id, data_key, wrapped) = provider.generate_and_wrap().map_err(|_| PayloadCryptoError::KeyProvider)?;
    if key_id.is_empty() || key_id.len() > u16::MAX as usize { return Err(PayloadCryptoError::InvalidKeyId); }
    if wrapped.is_empty() || wrapped.len() > u16::MAX as usize { return Err(PayloadCryptoError::InvalidEnvelope); }
    let mut nonce_bytes = [0_u8; NONCE_LEN];
    SystemRandom::new().fill(&mut nonce_bytes).map_err(|_| PayloadCryptoError::Randomness)?;
    let aad = associated_data(tenant, command, &key_id, &allow_hash);
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &data_key.0).map_err(|_| PayloadCryptoError::InvalidEnvelope)?;
    let key = aead::LessSafeKey::new(unbound);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
    let mut ciphertext = plaintext.to_vec();
    key.seal_in_place_append_tag(nonce, aead::Aad::from(aad), &mut ciphertext).map_err(|_| PayloadCryptoError::AuthenticationFailed)?;
    let mut envelope = Vec::with_capacity(6 + wrapped.len() + NONCE_LEN + ciphertext.len());
    envelope.extend_from_slice(MAGIC);
    envelope.extend_from_slice(&(wrapped.len() as u16).to_be_bytes());
    envelope.extend_from_slice(&wrapped);
    envelope.extend_from_slice(&nonce_bytes);
    envelope.extend_from_slice(&ciphertext);
    Ok(EncryptedCommandPayload::new(envelope, key_id, allow_hash))
}

/// Authenticates and decrypts, then independently verifies the Allow hash.
pub fn decrypt_command_payload<P: DataKeyProvider>(provider: &P, tenant: TenantId, command: CommandId, payload: &EncryptedCommandPayload, allow_hash: [u8; 32]) -> Result<Vec<u8>, PayloadCryptoError> {
    if payload.arguments_hash() != &allow_hash { return Err(PayloadCryptoError::ArgumentsHashMismatch); }
    let bytes = payload.ciphertext();
    if bytes.len() < 6 + NONCE_LEN + TAG_LEN || &bytes[..4] != MAGIC { return Err(PayloadCryptoError::InvalidEnvelope); }
    let wrapped_len = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
    let wrapped_end = 6_usize.checked_add(wrapped_len).ok_or(PayloadCryptoError::InvalidEnvelope)?;
    let nonce_end = wrapped_end.checked_add(NONCE_LEN).ok_or(PayloadCryptoError::InvalidEnvelope)?;
    if wrapped_len == 0 || nonce_end.checked_add(TAG_LEN).is_none_or(|end| end > bytes.len()) { return Err(PayloadCryptoError::InvalidEnvelope); }
    let wrapped = &bytes[6..wrapped_end];
    let nonce_bytes: [u8; NONCE_LEN] = bytes[wrapped_end..nonce_end].try_into().map_err(|_| PayloadCryptoError::InvalidEnvelope)?;
    let mut ciphertext = bytes[nonce_end..].to_vec();
    let data_key = provider.unwrap(payload.key_id(), wrapped).map_err(|_| PayloadCryptoError::KeyProvider)?;
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &data_key.0).map_err(|_| PayloadCryptoError::InvalidEnvelope)?;
    let key = aead::LessSafeKey::new(unbound);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
    let aad = associated_data(tenant, command, payload.key_id(), &allow_hash);
    let plain = key.open_in_place(nonce, aead::Aad::from(aad), &mut ciphertext).map_err(|_| PayloadCryptoError::AuthenticationFailed)?.to_vec();
    let digest: [u8; 32] = Sha256::digest(&plain).into();
    if digest != allow_hash { return Err(PayloadCryptoError::ArgumentsHashMismatch); }
    Ok(plain)
}

/// Re-encrypts under the provider's currently active wrapping key.
pub fn rotate_command_payload<P: DataKeyProvider>(provider: &P, tenant: TenantId, command: CommandId, payload: &EncryptedCommandPayload, allow_hash: [u8; 32]) -> Result<EncryptedCommandPayload, PayloadCryptoError> {
    let plain = decrypt_command_payload(provider, tenant, command, payload, allow_hash)?;
    encrypt_command_payload(provider, tenant, command, &plain, allow_hash)
}

fn associated_data(tenant: TenantId, command: CommandId, key_id: &str, hash: &[u8; 32]) -> Vec<u8> {
    let id = key_id.as_bytes();
    let mut aad = Vec::with_capacity(66 + id.len());
    aad.extend_from_slice(tenant.as_uuid().as_bytes());
    aad.extend_from_slice(command.as_uuid().as_bytes());
    aad.extend_from_slice(&(id.len() as u16).to_be_bytes());
    aad.extend_from_slice(id);
    aad.extend_from_slice(hash);
    aad
}


/// Workflow failure while persisting or retrieving an authorized payload.
pub enum CommandPayloadWorkflowError<E> {
    Store(E),
    Crypto(PayloadCryptoError),
}

/// Encrypts arguments and persists only the authenticated envelope.
/// The digest must be taken from the immutable Allow decision.
pub async fn persist_authorized_command_payload<S, P>(
    store: &S, provider: &P, scope: &Scope, command_id: CommandId,
    plaintext: &[u8], allow_arguments_hash: [u8; 32], ttl_seconds: u32,
) -> Result<(), CommandPayloadWorkflowError<S::Error>>
where S: CommandPayloadStore, P: DataKeyProvider {
    let encrypted = encrypt_command_payload(provider, scope.tenant_id(), command_id, plaintext, allow_arguments_hash)
        .map_err(CommandPayloadWorkflowError::Crypto)?;
    store.store_command_payload(scope, command_id, encrypted, ttl_seconds).await
        .map_err(CommandPayloadWorkflowError::Store)
}

/// Returns plaintext only after the Store confirms the Allow hash and AEAD plus
/// SHA-256 verification succeed. Call this immediately before executor handoff.
pub async fn load_authorized_command_payload<S, P>(
    store: &S, provider: &P, scope: &Scope, command_id: CommandId,
    allow_arguments_hash: [u8; 32],
) -> Result<Option<Vec<u8>>, CommandPayloadWorkflowError<S::Error>>
where S: CommandPayloadStore, P: DataKeyProvider {
    let Some(encrypted) = store.load_command_payload(scope, command_id, allow_arguments_hash).await
        .map_err(CommandPayloadWorkflowError::Store)? else { return Ok(None); };
    decrypt_command_payload(provider, scope.tenant_id(), command_id, &encrypted, allow_arguments_hash)
        .map(Some).map_err(CommandPayloadWorkflowError::Crypto)
}

/// Re-encrypts an existing record under the provider's active key without
/// changing command identity, plaintext or the Allow-decision digest.
pub async fn rotate_persisted_command_payload<S, P>(
    store: &S, provider: &P, scope: &Scope, command_id: CommandId,
    allow_arguments_hash: [u8; 32],
) -> Result<bool, CommandPayloadWorkflowError<S::Error>>
where S: CommandPayloadStore, P: DataKeyProvider {
    let Some(existing) = store.load_command_payload(scope, command_id, allow_arguments_hash).await
        .map_err(CommandPayloadWorkflowError::Store)? else { return Ok(false); };
    let rotated = rotate_command_payload(provider, scope.tenant_id(), command_id, &existing, allow_arguments_hash)
        .map_err(CommandPayloadWorkflowError::Crypto)?;
    store.store_command_payload(scope, command_id, rotated, 1).await
        .map_err(CommandPayloadWorkflowError::Store)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, sync::{Arc, RwLock}};
    #[derive(Clone)]
    struct TestProvider { active: Arc<RwLock<String>>, keys: Arc<RwLock<HashMap<String, [u8; 32]>>> }
    impl TestProvider {
        fn new() -> Self { let mut k=[0;32]; SystemRandom::new().fill(&mut k).unwrap(); let mut m=HashMap::new(); m.insert("kek-1".into(),k); Self { active:Arc::new(RwLock::new("kek-1".into())), keys:Arc::new(RwLock::new(m)) } }
        fn rotate(&self) { let mut k=[0;32]; SystemRandom::new().fill(&mut k).unwrap(); let id=format!("kek-{}",self.keys.read().unwrap().len()+1); self.keys.write().unwrap().insert(id.clone(),k); *self.active.write().unwrap()=id; }
        fn wrap(k:&[u8;32], d:&[u8;32])->Vec<u8>{d.iter().zip(k).map(|(a,b)|a^b).collect()}
    }
    impl DataKeyProvider for TestProvider {
        fn generate_and_wrap(&self)->Result<(String,DataKey,Vec<u8>),KeyProviderError>{let id=self.active.read().map_err(|_|KeyProviderError)?.clone();let k=*self.keys.read().map_err(|_|KeyProviderError)?.get(&id).ok_or(KeyProviderError)?;let mut d=[0;32];SystemRandom::new().fill(&mut d).map_err(|_|KeyProviderError)?;Ok((id,DataKey::new(d),Self::wrap(&k,&d)))}
        fn unwrap(&self,id:&str,w:&[u8])->Result<DataKey,KeyProviderError>{let k=*self.keys.read().map_err(|_|KeyProviderError)?.get(id).ok_or(KeyProviderError)?;if w.len()!=32{return Err(KeyProviderError)}let mut d=[0;32];for(i,(a,b))in w.iter().zip(k).enumerate(){d[i]=a^b;}Ok(DataKey::new(d))}
    }
    #[test] fn round_trip_and_no_plaintext_in_ciphertext(){let p=TestProvider::new();let t=TenantId::new();let c=CommandId::new();let plain=b"private argument";let h=Sha256::digest(plain).into();let e=encrypt_command_payload(&p,t,c,plain,h).unwrap();assert!(!e.ciphertext().windows(7).any(|w|w==b"private"));assert_eq!(decrypt_command_payload(&p,t,c,&e,h).unwrap(),plain);}
    #[test] fn tampering_and_wrong_associated_identity_are_rejected(){let p=TestProvider::new();let t=TenantId::new();let c=CommandId::new();let plain=b"secret";let h=Sha256::digest(plain).into();let e=encrypt_command_payload(&p,t,c,plain,h).unwrap();let mut bytes=e.ciphertext().to_vec();let n=bytes.len()-1;bytes[n]^=1;let bad=EncryptedCommandPayload::new(bytes,e.key_id().into(),h);assert_eq!(decrypt_command_payload(&p,t,c,&bad,h),Err(PayloadCryptoError::AuthenticationFailed));assert_eq!(decrypt_command_payload(&p,TenantId::new(),c,&e,h),Err(PayloadCryptoError::AuthenticationFailed));assert_eq!(decrypt_command_payload(&p,t,CommandId::new(),&e,h),Err(PayloadCryptoError::AuthenticationFailed));}
    #[test] fn wrong_key_and_allow_hash_mismatch_fail_closed(){let p=TestProvider::new();let t=TenantId::new();let c=CommandId::new();let plain=b"payload";let h=Sha256::digest(plain).into();let e=encrypt_command_payload(&p,t,c,plain,h).unwrap();assert_eq!(decrypt_command_payload(&TestProvider::new(),t,c,&e,h),Err(PayloadCryptoError::KeyProvider));assert_eq!(encrypt_command_payload(&p,t,c,plain,[0;32]),Err(PayloadCryptoError::ArgumentsHashMismatch));assert_eq!(decrypt_command_payload(&p,t,c,&e,[0;32]),Err(PayloadCryptoError::ArgumentsHashMismatch));}
    #[test] fn rotation_uses_new_key_and_preserves_old_decryptability(){let p=TestProvider::new();let t=TenantId::new();let c=CommandId::new();let plain=b"rotate me";let h=Sha256::digest(plain).into();let first=encrypt_command_payload(&p,t,c,plain,h).unwrap();p.rotate();let next=rotate_command_payload(&p,t,c,&first,h).unwrap();assert_ne!(first.key_id(),next.key_id());assert_eq!(decrypt_command_payload(&p,t,c,&next,h).unwrap(),plain);assert_eq!(decrypt_command_payload(&p,t,c,&first,h).unwrap(),plain);}
}
