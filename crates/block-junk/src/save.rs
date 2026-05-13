//! On-disk save format and IO.
//!
//! Layout: each save is a directory under `SAVE_ROOT` (workspace-relative
//! `./saves/` for dev — moves to `dirs::data_local_dir()` before shipping).
//! Inside, two files:
//!
//!   - `metadata.json` — small, human-inspectable. Read on its own for the
//!     save-list UI so we don't have to deserialize the chunk blob just to
//!     show "name + last modified."
//!   - `save.bin` — bincode-serialized `SaveFile`. Only edited chunks are
//!     persisted; procedural ones regenerate on load via the terrain
//!     function. That's what makes the save small (KBs for a normal game).
//!
//! Versioning: `SAVE_VERSION` bumps any time the on-disk shape changes
//! incompatibly. Loaders refuse mismatched versions and surface a typed
//! error rather than silently corrupting state.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::protocol::{AvatarPose, ChunkCoord};
use crate::voxel::{Chunk, ChunkEntities};

/// Bump on any breaking shape change. Loaders will refuse mismatched
/// versions; a future migration layer can branch on this.
/// v2 (2026-05-13): added `last_player_pose` to `SaveFile`.
pub const SAVE_VERSION: u32 = 2;

/// Workspace-relative for dev. Production should land in
/// `dirs::data_local_dir()` — flagged for the pre-ship pass.
const SAVE_ROOT: &str = "saves";

const METADATA_FILE: &str = "metadata.json";
const BLOB_FILE: &str = "save.bin";

#[derive(Debug, Error)]
pub enum SaveError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid save name {0:?}: must be non-empty and contain only [A-Za-z0-9_-]")]
    InvalidName(String),
    #[error("save {name:?} not found at {path}")]
    NotFound { name: String, path: PathBuf },
    #[error("save {name:?} has version {found}, expected {expected}")]
    VersionMismatch {
        name: String,
        found: u32,
        expected: u32,
    },
    #[error("bincode encode error: {0}")]
    BincodeEncode(#[from] bincode::error::EncodeError),
    #[error("bincode decode error: {0}")]
    BincodeDecode(#[from] bincode::error::DecodeError),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("system time before unix epoch (clock skew?)")]
    BadClock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveMetadata {
    pub name: String,
    /// Unix epoch seconds. Stored as u64 so the save survives 2038.
    pub created_at: u64,
    pub modified_at: u64,
    pub version: u32,
}

#[derive(Serialize, Deserialize)]
pub struct SaveFile {
    pub version: u32,
    pub edited_chunks: Vec<SavedChunk>,
    /// Position + yaw of the player at save time. For solo play this is
    /// where the next-connecting client respawns. For multi-host this is
    /// "the first player to reconnect lands here"; per-player persistence
    /// needs a stable client identity we don't have yet.
    pub last_player_pose: Option<AvatarPose>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedChunk {
    pub coord: ChunkCoord,
    pub chunk: Chunk,
    pub entities: ChunkEntities,
}

pub fn save_root() -> PathBuf {
    PathBuf::from(SAVE_ROOT)
}

pub fn save_dir_for(name: &str) -> PathBuf {
    save_root().join(name)
}

/// Save names become directory names, so we restrict to a tame charset.
/// Avoids path traversal (`..`), platform-specific reserved names, and
/// quirks of the various filesystems we might land on.
pub fn validate_name(name: &str) -> Result<(), SaveError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(SaveError::InvalidName(name.to_string()))
    }
}

fn now_unix() -> Result<u64, SaveError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| SaveError::BadClock)
}

fn read_metadata(dir: &Path) -> Result<SaveMetadata, SaveError> {
    let path = dir.join(METADATA_FILE);
    let bytes = std::fs::read(&path).map_err(|e| SaveError::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_metadata(dir: &Path, meta: &SaveMetadata) -> Result<(), SaveError> {
    let path = dir.join(METADATA_FILE);
    let bytes = serde_json::to_vec_pretty(meta)?;
    std::fs::write(&path, bytes).map_err(|e| SaveError::Io { path, source: e })
}

/// Write a save to disk, creating the directory if needed. Preserves an
/// existing `created_at` if the save already exists; updates `modified_at`
/// to now.
pub fn write_save(name: &str, save: &SaveFile) -> Result<(), SaveError> {
    validate_name(name)?;
    let dir = save_dir_for(name);
    std::fs::create_dir_all(&dir).map_err(|e| SaveError::Io {
        path: dir.clone(),
        source: e,
    })?;
    let now = now_unix()?;
    let created_at = read_metadata(&dir).map(|m| m.created_at).unwrap_or(now);
    let meta = SaveMetadata {
        name: name.to_string(),
        created_at,
        modified_at: now,
        version: SAVE_VERSION,
    };
    write_metadata(&dir, &meta)?;

    let blob = dir.join(BLOB_FILE);
    let bytes = bincode::serde::encode_to_vec(save, bincode::config::standard())?;
    std::fs::write(&blob, bytes).map_err(|e| SaveError::Io {
        path: blob,
        source: e,
    })?;
    Ok(())
}

pub fn read_save(name: &str) -> Result<SaveFile, SaveError> {
    validate_name(name)?;
    let dir = save_dir_for(name);
    if !dir.is_dir() {
        return Err(SaveError::NotFound {
            name: name.to_string(),
            path: dir,
        });
    }
    let meta = read_metadata(&dir)?;
    if meta.version != SAVE_VERSION {
        return Err(SaveError::VersionMismatch {
            name: name.to_string(),
            found: meta.version,
            expected: SAVE_VERSION,
        });
    }
    let blob = dir.join(BLOB_FILE);
    let bytes = std::fs::read(&blob).map_err(|e| SaveError::Io {
        path: blob,
        source: e,
    })?;
    let (save, _): (SaveFile, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard())?;
    Ok(save)
}

pub fn list_saves() -> Result<Vec<SaveMetadata>, SaveError> {
    let root = save_root();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&root).map_err(|e| SaveError::Io {
        path: root.clone(),
        source: e,
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Best-effort: a directory without a readable metadata.json is
        // skipped silently rather than killing the listing. (A broken save
        // shouldn't block the user from loading their good ones.)
        if let Ok(meta) = read_metadata(&path) {
            out.push(meta);
        }
    }
    out.sort_by_key(|m| std::cmp::Reverse(m.modified_at));
    Ok(out)
}

pub fn save_exists(name: &str) -> bool {
    save_dir_for(name).join(METADATA_FILE).is_file()
}

/// Permanently remove a save directory and all its contents.
pub fn delete_save(name: &str) -> Result<(), SaveError> {
    validate_name(name)?;
    let dir = save_dir_for(name);
    if !dir.exists() {
        return Ok(());
    }
    std::fs::remove_dir_all(&dir).map_err(|e| SaveError::Io {
        path: dir,
        source: e,
    })
}
