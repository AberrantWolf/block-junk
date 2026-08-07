//! Persistent Ed25519 client identity.
//!
//! The public key is the identity. A compact non-zero `u64` derived from
//! BLAKE3(public key) is presented to Lightyear and is also the save key;
//! the post-connect challenge proves possession of the corresponding secret.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bevy::prelude::*;
use ed25519_dalek::{Signer, SigningKey};

const IDENTITY_FILE: &str = "client_identity.v1";
const IDENTITY_LOCK: &str = "client_identity.lock";
const IDENTITY_VERSION: u8 = 1;
const IDENTITY_BYTES: usize = 1 + 32;

#[derive(Resource, Clone)]
pub struct ClientIdentity {
    signing_key: SigningKey,
    player_id: u64,
    persistent: bool,
}

impl std::fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientIdentity")
            .field("player_id", &self.player_id)
            .field("persistent", &self.persistent)
            .finish_non_exhaustive()
    }
}

impl ClientIdentity {
    pub fn player_id(&self) -> u64 {
        self.player_id
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    pub fn sign(&self, payload: &[u8]) -> [u8; 64] {
        self.signing_key.sign(payload).to_bytes()
    }

    pub fn is_persistent(&self) -> bool {
        self.persistent
    }
}

static ID_FILE_LOCK: OnceLock<File> = OnceLock::new();

pub fn load_or_create() -> ClientIdentity {
    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(IDENTITY_LOCK)
    {
        Ok(file) => file,
        Err(error) => {
            warn!("cannot open {IDENTITY_LOCK} ({error}); using a clearly ephemeral identity");
            return ephemeral();
        }
    };
    if let Err(error) = lock.try_lock() {
        info!("{IDENTITY_LOCK} is held by another process ({error}); using an ephemeral identity");
        return ephemeral();
    }

    let identity = match load_or_create_at(Path::new(IDENTITY_FILE)) {
        Ok(identity) => identity,
        Err(IdentityError::Corrupt(error)) => {
            // Never replace a corrupt persistent credential: doing so would
            // strand player ownership on every server that knows the key.
            error!(
                "{IDENTITY_FILE} is corrupt ({error}); refusing to replace it and using an ephemeral identity"
            );
            ephemeral()
        }
        Err(error) => {
            warn!("cannot persist {IDENTITY_FILE} ({error}); using an ephemeral identity");
            ephemeral()
        }
    };
    let _ = ID_FILE_LOCK.set(lock);
    identity
}

#[derive(Debug, thiserror::Error)]
enum IdentityError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Corrupt(String),
}

fn load_or_create_at(path: &Path) -> Result<ClientIdentity, IdentityError> {
    match File::open(path) {
        Ok(mut file) => {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            decode_identity(&bytes)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let identity = new_identity(true);
            write_identity_atomic(path, &identity)?;
            Ok(identity)
        }
        Err(error) => Err(error.into()),
    }
}

fn decode_identity(bytes: &[u8]) -> Result<ClientIdentity, IdentityError> {
    if bytes.len() != IDENTITY_BYTES {
        return Err(IdentityError::Corrupt(format!(
            "expected {IDENTITY_BYTES} bytes, found {}",
            bytes.len()
        )));
    }
    if bytes[0] != IDENTITY_VERSION {
        return Err(IdentityError::Corrupt(format!(
            "unsupported identity version {}",
            bytes[0]
        )));
    }
    let secret: [u8; 32] = bytes[1..]
        .try_into()
        .map_err(|_| IdentityError::Corrupt("invalid secret length".into()))?;
    Ok(from_signing_key(SigningKey::from_bytes(&secret), true))
}

fn write_identity_atomic(path: &Path, identity: &ClientIdentity) -> Result<(), std::io::Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temp = PathBuf::from(path);
    temp.set_extension(format!("tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options.open(&temp)?;
    let mut bytes = [0u8; IDENTITY_BYTES];
    bytes[0] = IDENTITY_VERSION;
    bytes[1..].copy_from_slice(&identity.signing_key.to_bytes());
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn ephemeral() -> ClientIdentity {
    new_identity(false)
}

fn new_identity(persistent: bool) -> ClientIdentity {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).expect("OS entropy source unavailable");
    from_signing_key(SigningKey::from_bytes(&secret), persistent)
}

fn from_signing_key(signing_key: SigningKey, persistent: bool) -> ClientIdentity {
    let public = signing_key.verifying_key().to_bytes();
    let mut id_bytes = [0u8; 8];
    id_bytes.copy_from_slice(&blake3::hash(&public).as_bytes()[..8]);
    ClientIdentity {
        signing_key,
        player_id: u64::from_le_bytes(id_bytes).max(1),
        persistent,
    }
}

pub fn player_id_from_public_key(public_key: &[u8; 32]) -> u64 {
    let mut id_bytes = [0u8; 8];
    id_bytes.copy_from_slice(&blake3::hash(public_key).as_bytes()[..8]);
    u64::from_le_bytes(id_bytes).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trip_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        let first = load_or_create_at(&path).unwrap();
        let second = load_or_create_at(&path).unwrap();
        assert_eq!(first.player_id(), second.player_id());
        assert_eq!(first.public_key(), second.public_key());
        assert_eq!(
            std::fs::metadata(path).unwrap().len(),
            IDENTITY_BYTES as u64
        );
    }

    #[test]
    fn corrupt_identity_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        std::fs::write(&path, b"broken").unwrap();
        assert!(matches!(
            load_or_create_at(&path),
            Err(IdentityError::Corrupt(_))
        ));
        assert_eq!(std::fs::read(path).unwrap(), b"broken");
    }

    #[test]
    fn signature_verifies_and_id_matches_public_key() {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let identity = new_identity(false);
        let payload = b"block-junk-ready";
        let signature = Signature::from_bytes(&identity.sign(payload));
        VerifyingKey::from_bytes(&identity.public_key())
            .unwrap()
            .verify(payload, &signature)
            .unwrap();
        assert_eq!(
            identity.player_id(),
            player_id_from_public_key(&identity.public_key())
        );
    }
}
