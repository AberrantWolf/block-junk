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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use bevy::math::IVec3;

use crate::protocol::{AvatarPose, ChunkCoord, MovementMode, PlanKind, WorldClock};
// `PlanState` and `MaterialEntry` are engine-side types; the on-disk
// shape keeps `item_id` as a string and lives in `SavedPlanState` /
// `SavedMaterialEntry` below so item-registry slot renumbering across
// sessions doesn't corrupt a save.
use crate::voxel::{Chunk, ChunkEntities};

/// Bump on any breaking shape change. Loaders will refuse mismatched
/// versions; a future migration layer can branch on this.
/// v2 (2026-05-13): added `last_player_pose` to `SaveFile`.
/// v3 (2026-05-15): added `npcs` to `SaveFile`.
/// v4 (2026-05-15): added `world_clock` to `SaveFile`.
/// v5 (2026-05-16): added `plans` to `SaveFile`.
/// v6 (2026-05-18): added `world_items` + `last_player_carry` to
///                  `SaveFile` for the Phase 2 carry/pickup feature.
/// v7 (2026-05-18): `plans` value evolves from bare `PlanKind` to
///                  `SavedPlanState` (kind + materials progress) for
///                  the Phase 3 plan-materials feature.
/// v8 (2026-05-18): added `carrying` to `SavedNpc` so a save mid-haul
///                  preserves each NPC's stack (Phase 4). HaulAssignments
///                  + WorldItemReservations are deliberately *not* saved
///                  — same pattern as PlanClaims; brain resets to Idle
///                  on load and the scheduler re-pairs from scratch.
/// v9 (2026-05-18): added `last_player_tool` to `SaveFile` and `tool`
///                  to `SavedNpc` (Phase 5a). Single-slot tools live
///                  separately from carry stacks so the save shape is
///                  symmetric: each actor gets one optional tool id.
/// v10 (2026-05-19): added `craft_stations` to `SaveFile` (Phase 6b).
///                  Per-cell `SavedStationState` with queued orders +
///                  deposited inventory so workbenches survive reload
///                  mid-craft-cycle. Items stored as ids (strings)
///                  for the same registry-stability reason world
///                  items and carry use.
/// v11 (2026-05-19): added `active_work` to `SavedStationState`
///                  (serde-defaulted) so a save mid-craft-timer
///                  resumes with the work intact. Required by the
///                  "no instant crafting" rule landing alongside.
/// v12 (2026-06-10): added `block_slots` — the slot→id table the chunk
///                  grids and Build plans were written against. Loaders
///                  remap saved slots through it into the live registry
///                  and refuse the load if a *referenced* id is no
///                  longer registered. Closes the last raw-slot hole
///                  (items/recipes already stored ids). Writes also
///                  became atomic (tmp + rename) in the same pass.
/// v13 (2026-07-02): `last_player_pose`/`carry`/`tool` (the single
///                  "first reconnect wins" slot) replaced by `players`
///                  — pose + carry + tool *per netcode client id*, now
///                  that ids are stable per install (identity.rs).
///                  v12 saves migrate on load: the legacy slot becomes
///                  a `players` entry under [`UNCLAIMED_PLAYER_ID`],
///                  claimed by the first client id that connects
///                  without an entry of its own.
pub const SAVE_VERSION: u32 = 13;

/// Sentinel `SavedPlayer::client_id` for state not yet bound to a real
/// client id: v12-migrated legacy state. Real ids are never 0 (see
/// `identity::random_id`).
pub const UNCLAIMED_PLAYER_ID: u64 = 0;

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
    #[error(
        "save references block ids that are no longer registered: {}; \
         restore the missing mod(s) or delete the save — loading would \
         silently corrupt the world",
        .ids.join(", ")
    )]
    MissingBlockIds { ids: Vec<String> },
    #[error(
        "save chunk references block slot {slot} but the save's slot table only has {table_len} entries — the save is corrupt or predates the slot table"
    )]
    SlotOutOfRange { slot: u16, table_len: usize },
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
    /// Block id per slot, indexed by `BlockSlot.0`, as registered when
    /// this save was written. Chunk grids and `PlanKind::Build` store
    /// raw slots (a string per cell would dwarf the save); this table
    /// is what makes those slots meaningful across sessions. The load
    /// path remaps every saved slot through it into the live registry
    /// — see [`remap_block_slots`].
    #[serde(default)]
    pub block_slots: Vec<String>,
    pub edited_chunks: Vec<SavedChunk>,
    /// Pose + carry + tool per client id — everyone connected at save
    /// time plus everyone the server remembered from earlier
    /// disconnects this session. Sorted by id for deterministic bytes.
    /// An entry under [`UNCLAIMED_PLAYER_ID`] is v12-migrated legacy
    /// state waiting for its first claimant.
    #[serde(default)]
    pub players: Vec<SavedPlayer>,
    /// Every NPC alive at save time. Empty for a save made before NPCs
    /// existed (those saves are v2 and won't load anyway, but the field
    /// is `default` for forward compat — adding a new NPC system off
    /// this field doesn't require another version bump).
    #[serde(default)]
    pub npcs: Vec<SavedNpc>,
    /// Day + time-of-day at save time. `Option` so a future
    /// non-WorldClock build (or a save manually constructed without
    /// it) deserializes cleanly; the load path falls back to the
    /// default sunrise position when this is missing.
    #[serde(default)]
    pub world_clock: Option<WorldClock>,
    /// Player-issued plan tags alive at save time. Sparse: only cells
    /// the player tagged. PlanClaims is *not* saved — the brain resets
    /// to Idle on load, so any in-flight work restarts from scratch
    /// and the claim is naturally re-acquired. Each entry carries the
    /// kind plus any materials-delivery progress so a save mid-haul
    /// resumes exactly where the player left off.
    #[serde(default)]
    pub plans: Vec<(IVec3, SavedPlanState)>,
    /// Loose items in the world at save time. Empty pre-v6 saves
    /// deserialize cleanly via `serde(default)`.
    #[serde(default)]
    pub world_items: Vec<SavedWorldItem>,
    /// Craft-station state at save time — queued orders + deposited
    /// inventory, per station cell. Empty vec for sessions with no
    /// active stations OR for saves predating v10 (serde-default).
    #[serde(default)]
    pub craft_stations: Vec<(IVec3, SavedStationState)>,
}

/// One player's persisted state, keyed by their netcode client id (a
/// stable per-install random u64 — see `identity.rs`). Carry/tool use
/// the same id-not-slot stability convention as everything else.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedPlayer {
    pub client_id: u64,
    pub pose: AvatarPose,
    pub carry: Option<SavedCarry>,
    pub tool: Option<SavedTool>,
}

/// The v12 `SaveFile` layout, kept verbatim (field order is the bincode
/// wire order) so v12 saves can be decoded and migrated instead of
/// refused. Only the fields that differ from v13 carry comments.
/// Serialize exists only so tests can author v12 bytes.
#[cfg_attr(test, derive(Serialize))]
#[derive(Deserialize)]
struct SaveFileV12 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    block_slots: Vec<String>,
    edited_chunks: Vec<SavedChunk>,
    /// v13 folds this + carry + tool into a `players` entry under
    /// [`UNCLAIMED_PLAYER_ID`].
    last_player_pose: Option<AvatarPose>,
    #[serde(default)]
    npcs: Vec<SavedNpc>,
    #[serde(default)]
    world_clock: Option<WorldClock>,
    #[serde(default)]
    plans: Vec<(IVec3, SavedPlanState)>,
    #[serde(default)]
    world_items: Vec<SavedWorldItem>,
    #[serde(default)]
    last_player_carry: Option<SavedCarry>,
    #[serde(default)]
    last_player_tool: Option<SavedTool>,
    #[serde(default)]
    craft_stations: Vec<(IVec3, SavedStationState)>,
}

impl From<SaveFileV12> for SaveFile {
    fn from(old: SaveFileV12) -> Self {
        // The legacy single-player slot becomes an unclaimed entry; the
        // first client id to connect without one of its own inherits
        // it. A v12 save with no recorded pose (headless world nobody
        // joined) has nothing worth claiming.
        let players = match old.last_player_pose {
            Some(pose) => vec![SavedPlayer {
                client_id: UNCLAIMED_PLAYER_ID,
                pose,
                carry: old.last_player_carry,
                tool: old.last_player_tool,
            }],
            None => Vec::new(),
        };
        SaveFile {
            version: SAVE_VERSION,
            block_slots: old.block_slots,
            edited_chunks: old.edited_chunks,
            players,
            npcs: old.npcs,
            world_clock: old.world_clock,
            plans: old.plans,
            world_items: old.world_items,
            craft_stations: old.craft_stations,
        }
    }
}

/// On-disk shape of a [`WorldItem`](crate::protocol::WorldItem) entity.
/// `item_id` is the stable [`ItemId`] string rather than the slot, so
/// the save format survives item-registry changes between sessions
/// (slots are derived from mod load order; ids are mod-author-stable).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedWorldItem {
    pub item_id: String,
    pub translation: bevy::math::Vec3,
}

/// On-disk shape of an actor's
/// [`Carrying`](crate::protocol::Carrying) stack. Same id-not-slot
/// stability rule as [`SavedWorldItem`]. `count == 0` is canonical
/// "empty-handed" — but in practice we serialise `None` for that
/// case at the [`SavedPlayer::carry`] / `SavedNpc::carrying` layer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedCarry {
    pub item_id: String,
    pub count: u32,
}

/// On-disk shape of an actor's
/// [`EquippedTool`](crate::protocol::EquippedTool) slot. Just an item
/// id — single-slot, so no count. Same stability convention as
/// [`SavedCarry`]; the load path drops references to unknown ids
/// (mod uninstalled between sessions) with a warning.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedTool {
    pub item_id: String,
}

/// On-disk shape of a
/// [`StationState`](crate::craft_stations::StationState). Both fields
/// store ids/strings (not registry slots) so the save survives a
/// session where mods register in a different order. Recipe ids are
/// stable strings already; inventory items go through the same
/// id↔slot resolution carry + world items use.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedStationState {
    pub orders: Vec<SavedCraftOrder>,
    pub inventory: Vec<SavedStationItem>,
    /// In-progress craft snapshot. `None` for stations sitting
    /// idle, AND for saves predating v11 (the serde default lets
    /// v10 saves still load by treating "no field" as "no work").
    #[serde(default)]
    pub active_work: Option<SavedActiveWork>,
}

/// On-disk shape of an in-progress craft cycle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedActiveWork {
    pub recipe_id: String,
    pub total_secs: f32,
    pub elapsed_secs: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedCraftOrder {
    pub recipe_id: String,
    pub total: u32,
    pub completed: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedStationItem {
    pub item_id: String,
    pub count: u32,
}

/// On-disk shape of a [`PlanState`](crate::protocol::PlanState).
/// `kind` is `PlanKind` direct (cheap, stable). `materials` lives in
/// [`SavedMaterialEntry`] so item-slot renumbering between sessions
/// doesn't corrupt a save mid-haul.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedPlanState {
    pub kind: PlanKind,
    #[serde(default)]
    pub materials: Vec<SavedMaterialEntry>,
}

/// On-disk shape of a [`MaterialEntry`](crate::protocol::MaterialEntry).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedMaterialEntry {
    pub item_id: String,
    pub needed: u32,
    pub present: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedChunk {
    pub coord: ChunkCoord,
    pub chunk: Chunk,
    pub entities: ChunkEntities,
}

/// Persistent slice of an NPC. Captures the state the brain can't
/// reconstruct from world/registry alone:
/// - Identity (`id`, `kind`) so the same NPC reappears as itself.
/// - Pose (translation + yaw) so they don't teleport on load.
/// - Movement mode (typically `Walk`; saved for completeness so a future
///   `Fly`-capable NPC doesn't lose state).
/// - Need values; decay across save/load resumes from the saved float.
/// - The brain's PRNG state, so wander-target selection isn't a fresh
///   seed on every restart.
///
/// **Not** saved: `Brain::goal` (resets to `Idle`; the planner picks a
/// fresh action on the first post-load tick), velocity, on-ground
/// state, the live A* path overlay. All transient and cheap to rebuild.
#[derive(Clone, Serialize, Deserialize)]
pub struct SavedNpc {
    pub id: u64,
    pub kind: String,
    pub pose: AvatarPose,
    pub movement_mode: MovementMode,
    pub needs: HashMap<String, f32>,
    pub rng: u64,
    /// Carry stack at save time. `None` for empty-handed NPCs and for
    /// v7-and-earlier saves (serde-default fires there). Hauling NPCs
    /// caught mid-cycle resume with their stack intact; the brain
    /// itself resets to Idle on load, so the scheduler re-pairs the
    /// NPC to a fresh assignment on the first post-load tick (the
    /// carry is then deposited at whatever plan the scheduler picks,
    /// or sits until a Q-drop / new haul disposes of it).
    #[serde(default)]
    pub carrying: Option<SavedCarry>,
    /// Tool slot at save time. `None` for empty-toolslot NPCs and for
    /// v8-and-earlier saves. NPCs don't currently equip tools (no
    /// scheduler path for that yet — Phase 5b), but the field is
    /// already in the save shape so adding NPC tool fetch later is a
    /// pure runtime change.
    #[serde(default)]
    pub tool: Option<SavedTool>,
}

/// Rewrite every saved [`BlockSlot`] (chunk cells — padding included,
/// since padding ships to clients and feeds the mesher — and
/// `PlanKind::Build` slots) from the save's own `block_slots` table
/// into the live registry via `lookup` (id string → current slot).
///
/// Two passes: detect, then apply. Detection collects the full set of
/// problems before touching anything, so the error lists *every*
/// missing id (the player fixes their mod set once, not once per id).
/// Ids in the table that no chunk/plan actually references are fine to
/// lose — only referenced ids block the load.
pub fn remap_block_slots(
    save: &mut SaveFile,
    lookup: impl Fn(&str) -> Option<crate::blocks::BlockSlot>,
) -> Result<(), SaveError> {
    use crate::blocks::BlockSlot;

    let map: Vec<Option<BlockSlot>> = save.block_slots.iter().map(|id| lookup(id)).collect();

    let mut missing = std::collections::BTreeSet::new();
    let mut check = |slot: BlockSlot| -> Result<(), SaveError> {
        let idx = slot.0 as usize;
        match map.get(idx) {
            None => Err(SaveError::SlotOutOfRange {
                slot: slot.0,
                table_len: map.len(),
            }),
            Some(None) => {
                missing.insert(save.block_slots[idx].clone());
                Ok(())
            }
            Some(Some(_)) => Ok(()),
        }
    };
    for saved in &save.edited_chunks {
        for slot in &saved.chunk.blocks {
            check(*slot)?;
        }
    }
    for (_, plan) in &save.plans {
        if let PlanKind::Build { slot, .. } = plan.kind {
            check(slot)?;
        }
    }
    if !missing.is_empty() {
        return Err(SaveError::MissingBlockIds {
            ids: missing.into_iter().collect(),
        });
    }

    // Apply. Detection proved every referenced slot maps, so the
    // unwraps here are structural.
    for saved in &mut save.edited_chunks {
        for slot in &mut saved.chunk.blocks {
            *slot = map[slot.0 as usize].unwrap();
        }
    }
    for (_, plan) in &mut save.plans {
        if let PlanKind::Build { slot, .. } = &mut plan.kind {
            *slot = map[slot.0 as usize].unwrap();
        }
    }
    Ok(())
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

/// Write `bytes` to `path` atomically: write a sibling `*.tmp`, then
/// rename over the target. A crash mid-write leaves either the old file
/// intact or a stray tmp — never a truncated/interleaved target.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SaveError> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| SaveError::Io {
        path: tmp.clone(),
        source: e,
    })?;
    std::fs::rename(&tmp, path).map_err(|e| SaveError::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

fn write_metadata(dir: &Path, meta: &SaveMetadata) -> Result<(), SaveError> {
    let bytes = serde_json::to_vec_pretty(meta)?;
    write_atomic(&dir.join(METADATA_FILE), &bytes)
}

/// Write a save to disk, creating the directory if needed. Preserves an
/// existing `created_at` if the save already exists; updates `modified_at`
/// to now.
///
/// Blob lands before metadata: loaders read metadata first (the version
/// gate), so a crash between the two renames leaves a save whose
/// metadata still describes the previous write — stale, but coherent
/// and loadable. The reverse order could stamp a fresh version onto a
/// blob that never arrived.
pub fn write_save(name: &str, save: &SaveFile) -> Result<(), SaveError> {
    validate_name(name)?;
    let dir = save_dir_for(name);
    std::fs::create_dir_all(&dir).map_err(|e| SaveError::Io {
        path: dir.clone(),
        source: e,
    })?;
    let bytes = bincode::serde::encode_to_vec(save, bincode::config::standard())?;
    write_atomic(&dir.join(BLOB_FILE), &bytes)?;

    let now = now_unix()?;
    let created_at = read_metadata(&dir).map(|m| m.created_at).unwrap_or(now);
    let meta = SaveMetadata {
        name: name.to_string(),
        created_at,
        modified_at: now,
        version: SAVE_VERSION,
    };
    write_metadata(&dir, &meta)
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
    if meta.version != SAVE_VERSION && meta.version != 12 {
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
    if meta.version == 12 {
        // In-memory migration only; the file upgrades on the next
        // write, so a failed session never rewrites a good v12 save.
        let (old, _): (SaveFileV12, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard())?;
        bevy::log::info!("migrated save {name:?} from v12 (single-player slot → players table)");
        return Ok(old.into());
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::math::Vec3;
    use block_junk_mod_api::blocks::Cardinal;
    use ndshape::ConstShape;

    use crate::blocks::BlockSlot;

    /// Round-trip a SaveFile through bincode to catch serde regressions
    /// at the shape level. Covers every field the current version
    /// carries: v3 npcs, v4 world_clock, v5/v7 plans, v6 world_items,
    /// v13 per-player entries.
    #[test]
    fn savefile_round_trips_all_fields() {
        let mut needs = HashMap::new();
        needs.insert("hunger".to_owned(), 0.42);
        let plans = vec![
            (
                IVec3::new(1, 2, 3),
                SavedPlanState {
                    kind: PlanKind::Remove,
                    materials: vec![],
                },
            ),
            (
                IVec3::new(-4, 5, -6),
                SavedPlanState {
                    kind: PlanKind::Build {
                        slot: BlockSlot(7),
                        orientation: Cardinal::North,
                    },
                    materials: vec![SavedMaterialEntry {
                        item_id: "vanilla:wood_log".to_owned(),
                        needed: 2,
                        present: 1,
                    }],
                },
            ),
        ];
        let world_items = vec![
            SavedWorldItem {
                item_id: "vanilla:wood_log".to_owned(),
                translation: Vec3::new(10.0, 8.5, -3.25),
            },
            SavedWorldItem {
                item_id: "vanilla:stone_chunk".to_owned(),
                translation: Vec3::new(-1.0, 1.0, 1.0),
            },
        ];
        let original = SaveFile {
            version: SAVE_VERSION,
            block_slots: vec!["vanilla:empty".to_owned(), "vanilla:stone".to_owned()],
            edited_chunks: vec![],
            players: vec![SavedPlayer {
                client_id: 0xFEED_F00D,
                pose: AvatarPose {
                    translation: Vec3::new(1.0, 2.0, 3.0),
                    yaw: 0.5,
                },
                carry: Some(SavedCarry {
                    item_id: "vanilla:wood_log".to_owned(),
                    count: 3,
                }),
                tool: Some(SavedTool {
                    item_id: "vanilla:axe".to_owned(),
                }),
            }],
            npcs: vec![SavedNpc {
                id: 7,
                kind: "vanilla:wanderer".to_owned(),
                pose: AvatarPose {
                    translation: Vec3::new(4.0, 5.0, 6.0),
                    yaw: 1.0,
                },
                movement_mode: MovementMode::Walk,
                needs: needs.clone(),
                rng: 0xCAFE_BABE_DEAD_BEEF,
                carrying: Some(SavedCarry {
                    item_id: "vanilla:stone_chunk".to_owned(),
                    count: 2,
                }),
                tool: Some(SavedTool {
                    item_id: "vanilla:pickaxe".to_owned(),
                }),
            }],
            world_clock: Some(WorldClock {
                day: 3,
                time_of_day: 0.625,
            }),
            plans: plans.clone(),
            world_items: world_items.clone(),
            craft_stations: vec![(
                IVec3::new(2, 32, 60),
                SavedStationState {
                    orders: vec![SavedCraftOrder {
                        recipe_id: "vanilla:planks_from_log".to_owned(),
                        total: 4,
                        completed: 1,
                    }],
                    inventory: vec![SavedStationItem {
                        item_id: "vanilla:wood_log".to_owned(),
                        count: 2,
                    }],
                    active_work: Some(SavedActiveWork {
                        recipe_id: "vanilla:planks_from_log".to_owned(),
                        total_secs: 4.0,
                        elapsed_secs: 1.25,
                    }),
                },
            )],
        };

        let bytes = bincode::serde::encode_to_vec(&original, bincode::config::standard()).unwrap();
        let (decoded, _): (SaveFile, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();

        assert_eq!(decoded.version, original.version);
        assert_eq!(decoded.block_slots, original.block_slots);
        assert_eq!(decoded.npcs.len(), 1);
        let np = &decoded.npcs[0];
        assert_eq!(np.id, 7);
        assert_eq!(np.kind, "vanilla:wanderer");
        assert_eq!(np.movement_mode, MovementMode::Walk);
        assert_eq!(np.needs.get("hunger"), Some(&0.42));
        assert_eq!(np.rng, 0xCAFE_BABE_DEAD_BEEF);
        let npc_carry = np.carrying.as_ref().unwrap();
        assert_eq!(npc_carry.item_id, "vanilla:stone_chunk");
        assert_eq!(npc_carry.count, 2);
        assert_eq!(decoded.players.len(), 1);
        let player = &decoded.players[0];
        assert_eq!(player.client_id, 0xFEED_F00D);
        assert_eq!(player.pose.translation, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(player.pose.yaw, 0.5);
        let clock = decoded.world_clock.unwrap();
        assert_eq!(clock.day, 3);
        assert_eq!(clock.time_of_day, 0.625);
        assert_eq!(decoded.plans.len(), 2);
        assert_eq!(decoded.plans[1].1.materials.len(), 1);
        assert_eq!(decoded.plans[1].1.materials[0].item_id, "vanilla:wood_log");
        assert_eq!(decoded.plans[1].1.materials[0].needed, 2);
        assert_eq!(decoded.plans[1].1.materials[0].present, 1);
        assert_eq!(decoded.world_items.len(), 2);
        assert_eq!(decoded.world_items[0].item_id, "vanilla:wood_log");
        assert_eq!(
            decoded.world_items[0].translation,
            Vec3::new(10.0, 8.5, -3.25)
        );
        let carry = player.carry.as_ref().unwrap();
        assert_eq!(carry.item_id, "vanilla:wood_log");
        assert_eq!(carry.count, 3);
        let tool = player.tool.as_ref().unwrap();
        assert_eq!(tool.item_id, "vanilla:axe");
        let npc_tool = decoded.npcs[0].tool.as_ref().unwrap();
        assert_eq!(npc_tool.item_id, "vanilla:pickaxe");
        assert_eq!(decoded.craft_stations.len(), 1);
        let (station_cell, station_state) = &decoded.craft_stations[0];
        assert_eq!(*station_cell, IVec3::new(2, 32, 60));
        assert_eq!(station_state.orders.len(), 1);
        assert_eq!(station_state.orders[0].recipe_id, "vanilla:planks_from_log");
        assert_eq!(station_state.orders[0].total, 4);
        assert_eq!(station_state.orders[0].completed, 1);
        assert_eq!(station_state.inventory.len(), 1);
        assert_eq!(station_state.inventory[0].item_id, "vanilla:wood_log");
        assert_eq!(station_state.inventory[0].count, 2);
        let active = station_state.active_work.as_ref().unwrap();
        assert_eq!(active.recipe_id, "vanilla:planks_from_log");
        assert_eq!(active.total_secs, 4.0);
        assert_eq!(active.elapsed_secs, 1.25);
    }

    /// A v12 blob (single last-player slot) must decode and land its
    /// legacy state as the unclaimed `players` entry, byte-compatibly
    /// with what a real v12 session wrote.
    #[test]
    fn v12_savefile_migrates_to_players_table() {
        let old = SaveFileV12 {
            version: 12,
            block_slots: vec!["vanilla:empty".to_owned()],
            edited_chunks: vec![],
            last_player_pose: Some(AvatarPose {
                translation: Vec3::new(9.0, 8.0, 7.0),
                yaw: 1.5,
            }),
            npcs: vec![],
            world_clock: None,
            plans: vec![],
            world_items: vec![],
            last_player_carry: Some(SavedCarry {
                item_id: "vanilla:stone_chunk".to_owned(),
                count: 2,
            }),
            last_player_tool: None,
            craft_stations: vec![],
        };
        let bytes = bincode::serde::encode_to_vec(&old, bincode::config::standard()).unwrap();
        let (decoded, _): (SaveFileV12, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        let migrated: SaveFile = decoded.into();
        assert_eq!(migrated.version, SAVE_VERSION);
        assert_eq!(migrated.block_slots, vec!["vanilla:empty".to_owned()]);
        assert_eq!(migrated.players.len(), 1);
        let p = &migrated.players[0];
        assert_eq!(p.client_id, UNCLAIMED_PLAYER_ID);
        assert_eq!(p.pose.translation, Vec3::new(9.0, 8.0, 7.0));
        assert_eq!(p.carry.as_ref().unwrap().item_id, "vanilla:stone_chunk");
        assert!(p.tool.is_none());
    }

    /// A v12 world nobody ever joined has no pose — and therefore
    /// nothing worth claiming after migration.
    #[test]
    fn poseless_v12_migrates_to_empty_players() {
        let old = SaveFileV12 {
            version: 12,
            block_slots: vec![],
            edited_chunks: vec![],
            last_player_pose: None,
            npcs: vec![],
            world_clock: None,
            plans: vec![],
            world_items: vec![],
            last_player_carry: None,
            last_player_tool: None,
            craft_stations: vec![],
        };
        let migrated: SaveFile = old.into();
        assert!(migrated.players.is_empty());
    }

    /// Minimal SaveFile carrying one chunk + one Build plan, written
    /// against the given slot table. Cell values are raw indices into
    /// `chunk.blocks` so tests can poke specific cells without going
    /// through `Chunk::set`'s interior-only filter (padding cells hold
    /// slots too and must remap — they ship to clients and feed the
    /// mesher).
    fn remap_fixture(table: &[&str], cells: &[(usize, u16)], plan_slot: u16) -> SaveFile {
        let mut blocks = vec![BlockSlot::EMPTY; crate::voxel::ChunkShape::USIZE];
        for &(idx, slot) in cells {
            blocks[idx] = BlockSlot(slot);
        }
        SaveFile {
            version: SAVE_VERSION,
            block_slots: table.iter().map(|s| s.to_string()).collect(),
            edited_chunks: vec![SavedChunk {
                coord: ChunkCoord(IVec3::ZERO),
                chunk: Chunk { blocks },
                entities: ChunkEntities::default(),
            }],
            players: vec![],
            npcs: vec![],
            world_clock: None,
            plans: vec![(
                IVec3::new(1, 2, 3),
                SavedPlanState {
                    kind: PlanKind::Build {
                        slot: BlockSlot(plan_slot),
                        orientation: Cardinal::North,
                    },
                    materials: vec![],
                },
            )],
            world_items: vec![],
            craft_stations: vec![],
        }
    }

    /// Registry lookup standing in for a session where registration
    /// order changed: ids "a" and "b" swapped slots since the save was
    /// written, and "gone" is no longer registered at all.
    fn reordered_lookup(id: &str) -> Option<BlockSlot> {
        match id {
            "vanilla:empty" => Some(BlockSlot::EMPTY),
            "a" => Some(BlockSlot(2)),
            "b" => Some(BlockSlot(1)),
            _ => None,
        }
    }

    #[test]
    fn remap_rewrites_chunk_cells_and_build_plans() {
        // Saved with table [empty, a, b]: cell 10 holds "a" (slot 1),
        // cell 11 holds "b" (slot 2); the Build plan targets "b".
        let mut save = remap_fixture(&["vanilla:empty", "a", "b"], &[(10, 1), (11, 2)], 2);
        remap_block_slots(&mut save, reordered_lookup).unwrap();
        let blocks = &save.edited_chunks[0].chunk.blocks;
        assert_eq!(
            blocks[10],
            BlockSlot(2),
            "cell holding \"a\" follows its id"
        );
        assert_eq!(
            blocks[11],
            BlockSlot(1),
            "cell holding \"b\" follows its id"
        );
        assert_eq!(blocks[0], BlockSlot::EMPTY);
        let PlanKind::Build { slot, .. } = save.plans[0].1.kind else {
            panic!("plan kind changed shape");
        };
        assert_eq!(slot, BlockSlot(1), "Build plan follows \"b\"");
    }

    #[test]
    fn remap_tolerates_missing_id_nothing_references() {
        // "gone" is in the table but no cell/plan uses slot 3 — the
        // load must succeed (a removed mod only blocks loads of worlds
        // that actually contain its blocks).
        let mut save = remap_fixture(&["vanilla:empty", "a", "b", "gone"], &[(5, 1)], 1);
        remap_block_slots(&mut save, reordered_lookup).unwrap();
        assert_eq!(save.edited_chunks[0].chunk.blocks[5], BlockSlot(2));
    }

    #[test]
    fn remap_refuses_missing_id_in_use_and_lists_it() {
        let mut save = remap_fixture(&["vanilla:empty", "a", "gone"], &[(5, 2)], 1);
        let err = remap_block_slots(&mut save, reordered_lookup).unwrap_err();
        match err {
            SaveError::MissingBlockIds { ids } => assert_eq!(ids, vec!["gone".to_owned()]),
            other => panic!("expected MissingBlockIds, got {other}"),
        }
        // Refusal must leave the save untouched (caller may report and
        // keep the file for a later load with the mod restored).
        assert_eq!(save.edited_chunks[0].chunk.blocks[5], BlockSlot(2));
    }

    #[test]
    fn remap_refuses_slot_beyond_table() {
        let mut save = remap_fixture(&["vanilla:empty", "a"], &[(5, 9)], 1);
        let err = remap_block_slots(&mut save, reordered_lookup).unwrap_err();
        assert!(
            matches!(
                err,
                SaveError::SlotOutOfRange {
                    slot: 9,
                    table_len: 2
                }
            ),
            "got {err}"
        );
    }

    #[test]
    fn write_atomic_replaces_existing_content() {
        let dir = std::env::temp_dir().join(format!("bj-save-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blob.bin");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert!(
            !path.with_extension("tmp").exists(),
            "tmp file must not linger after a successful rename"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
