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
pub const SAVE_VERSION: u32 = 11;

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
    /// Carry stack of the spawning player at save time (the same
    /// "first reconnecting player wins" convention `last_player_pose`
    /// uses). `None` ⇒ empty-handed save, or a save predating v6.
    /// Per-player carries persistence needs a stable client identity
    /// we don't have yet.
    #[serde(default)]
    pub last_player_carry: Option<SavedCarry>,
    /// Tool slot of the spawning player at save time. Same
    /// first-reconnect convention as `last_player_carry`. `None` ⇒
    /// empty tool slot at save time, OR a save predating v9 (the
    /// load path falls back to the engine starter-axe via
    /// `STARTER_TOOL_ID`).
    #[serde(default)]
    pub last_player_tool: Option<SavedTool>,
    /// Craft-station state at save time — queued orders + deposited
    /// inventory, per station cell. Empty vec for sessions with no
    /// active stations OR for saves predating v10 (serde-default).
    #[serde(default)]
    pub craft_stations: Vec<(IVec3, SavedStationState)>,
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
/// case at the `SaveFile::last_player_carry` layer.
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

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::math::Vec3;
    use block_junk_mod_api::blocks::Cardinal;

    use crate::blocks::BlockSlot;

    /// Round-trip a SaveFile through bincode to catch serde regressions
    /// at the shape level. Covers every field the current version
    /// carries: v3 npcs, v4 world_clock, v5/v7 plans, v6 world_items +
    /// last_player_carry, v7 plan materials.
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
            edited_chunks: vec![],
            last_player_pose: Some(AvatarPose {
                translation: Vec3::new(1.0, 2.0, 3.0),
                yaw: 0.5,
            }),
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
            last_player_carry: Some(SavedCarry {
                item_id: "vanilla:wood_log".to_owned(),
                count: 3,
            }),
            last_player_tool: Some(SavedTool {
                item_id: "vanilla:axe".to_owned(),
            }),
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

        let bytes =
            bincode::serde::encode_to_vec(&original, bincode::config::standard()).unwrap();
        let (decoded, _): (SaveFile, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();

        assert_eq!(decoded.version, original.version);
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
        let pose = decoded.last_player_pose.unwrap();
        assert_eq!(pose.translation, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(pose.yaw, 0.5);
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
        assert_eq!(decoded.world_items[0].translation, Vec3::new(10.0, 8.5, -3.25));
        let carry = decoded.last_player_carry.unwrap();
        assert_eq!(carry.item_id, "vanilla:wood_log");
        assert_eq!(carry.count, 3);
        let tool = decoded.last_player_tool.unwrap();
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
}
