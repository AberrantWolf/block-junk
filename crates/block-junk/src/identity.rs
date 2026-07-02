//! Persistent per-install client identity.
//!
//! A random u64 generated once and stored as text in `./client_id.txt`
//! (workspace-relative for dev, same convention as `save::SAVE_ROOT` —
//! both move to platform dirs in the pre-ship pass). This id is what the
//! netcode layer presents as `client_id` and what the server keys
//! per-player persistence on, so copying the file to another machine is
//! "playing as yourself there" — the SSH-key model.
//!
//! Spoof-resistance is deliberately deferred: the id file is a claim,
//! not a proof. When strangers-on-servers becomes a real threat model,
//! the server binds each id to a public key on first use (TOFU) and
//! challenges afterwards — an additive change that keeps this u64 as
//! the persistence key, so there is no migration.
//!
//! The file is held under an advisory exclusive lock for the lifetime
//! of the process. A second client launched from the same directory
//! (the standard local two-client smoke test) fails the lock and falls
//! back to an *ephemeral* random id, so both instances can connect to
//! one server without a `ClientIdInUse` collision.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::OnceLock;

use bevy::prelude::*;

const ID_FILE: &str = "client_id.txt";

/// The u64 this install presents as its netcode client id. Inserted as
/// a resource on the client App at plugin-build time; read once by
/// `start_netcode_client`.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientIdentity(pub u64);

/// Keeps the id file's advisory lock alive for the process lifetime.
/// Dropping the handle would release the lock and let a second local
/// instance read the same id.
static ID_FILE_LOCK: OnceLock<File> = OnceLock::new();

/// Load the persistent id, creating the file on first run. Every
/// failure path (locked by another instance, unreadable file, readonly
/// filesystem) degrades to an ephemeral random id with a log line —
/// identity is a persistence nicety, never a reason to refuse to play.
pub fn load_or_create() -> ClientIdentity {
    let mut file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(ID_FILE)
    {
        Ok(f) => f,
        Err(e) => {
            warn!("cannot open {ID_FILE} ({e}); using an ephemeral client id this session");
            return ClientIdentity(random_id());
        }
    };
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            info!(
                "{ID_FILE} is locked by another running instance; \
                 using an ephemeral client id this session"
            );
            return ClientIdentity(random_id());
        }
        Err(TryLockError::Error(e)) => {
            warn!("cannot lock {ID_FILE} ({e}); using an ephemeral client id this session");
            return ClientIdentity(random_id());
        }
    }

    let mut contents = String::new();
    let parsed = file
        .read_to_string(&mut contents)
        .ok()
        .and_then(|_| contents.trim().parse::<u64>().ok())
        .filter(|id| *id != 0);
    let id = match parsed {
        Some(id) => id,
        None => {
            let id = random_id();
            // Empty on first run; anything else is corrupt — either way
            // the file's new content is this id.
            if let Err(e) = file
                .seek(SeekFrom::Start(0))
                .and_then(|_| file.set_len(0))
                .and_then(|_| file.write_all(id.to_string().as_bytes()))
            {
                warn!("cannot write {ID_FILE} ({e}); id will regenerate next launch");
            } else {
                info!("generated new persistent client id in {ID_FILE}");
            }
            id
        }
    };
    // Hold the lock forever. A second load_or_create in this process
    // (there isn't one today) would just re-read the same file.
    let _ = ID_FILE_LOCK.set(file);
    ClientIdentity(id)
}

/// Random non-zero u64 from OS entropy. Zero is excluded because it
/// reads as "unset" in netcode logs and tooling.
fn random_id() -> u64 {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("OS entropy source unavailable");
    u64::from_le_bytes(bytes).max(1)
}

#[cfg(test)]
mod tests {
    use super::random_id;

    #[test]
    fn random_ids_are_nonzero_and_distinct() {
        let a = random_id();
        let b = random_id();
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        // 64-bit collision odds are negligible; a repeat here means the
        // entropy source is broken, which is worth a test failure.
        assert_ne!(a, b);
    }
}
