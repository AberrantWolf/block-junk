use bevy::ecs::entity::{EntityMapper, MapEntities};
use bevy::prelude::*;
use block_junk_mod_api::blocks::{BlockId, Cardinal};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::{Deref, DerefMut};

use crate::blocks::BlockSlot;
use crate::items::ItemSlot;
use crate::voxel::{EntityEntry, EntryKind};

pub const CHUNK_SIZE: u32 = 32;
pub const CHUNK_PADDED: u32 = CHUNK_SIZE + 2;
pub const CHUNK_PADDED_CELLS: usize = CHUNK_PADDED.pow(3) as usize;
pub const MAX_WIRE_ID_BYTES: usize = 128;
pub const MAX_BLOCK_FOOTPRINT_CELLS: usize = 256;
pub const MAX_APPLIED_BLOCK_CELLS: usize = 512;
pub const MAX_REASSEMBLED_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_SPATIAL_PAGE_BYTES: usize = 32 * 1024;

/// A string whose UTF-8 byte length is rejected during deserialization.
/// This keeps an attacker-controlled length prefix from allocating an
/// unbounded `String` before a handler gets a chance to validate it.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct BoundedString<const N: usize>(String);

impl<const N: usize> BoundedString<N> {
    pub fn new(value: impl Into<String>) -> Result<Self, BoundExceeded> {
        let value = value.into();
        if value.len() > N {
            return Err(BoundExceeded {
                limit: N,
                actual: value.len(),
            });
        }
        Ok(Self(value))
    }
}

impl<const N: usize> Deref for BoundedString<N> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<const N: usize> fmt::Debug for BoundedString<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<const N: usize> fmt::Display for BoundedString<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<const N: usize> AsRef<str> for BoundedString<N> {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de, const N: usize> Deserialize<'de> for BoundedString<N> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor<const N: usize>;

        impl<'de, const N: usize> serde::de::Visitor<'de> for Visitor<N> {
            type Value = BoundedString<N>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a UTF-8 string of at most {N} bytes")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                BoundedString::new(value).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                BoundedString::new(value).map_err(E::custom)
            }

            fn visit_borrowed_bytes<E>(self, value: &'de [u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.len() > N {
                    return Err(E::custom(BoundExceeded {
                        limit: N,
                        actual: value.len(),
                    }));
                }
                let value = std::str::from_utf8(value).map_err(E::custom)?;
                BoundedString::new(value).map_err(E::custom)
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.len() > N {
                    return Err(E::custom(BoundExceeded {
                        limit: N,
                        actual: value.len(),
                    }));
                }
                let value = std::str::from_utf8(value).map_err(E::custom)?;
                BoundedString::new(value).map_err(E::custom)
            }

            fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.len() > N {
                    return Err(E::custom(BoundExceeded {
                        limit: N,
                        actual: value.len(),
                    }));
                }
                let value = String::from_utf8(value).map_err(E::custom)?;
                BoundedString::new(value).map_err(E::custom)
            }
        }

        if deserializer.is_human_readable() {
            deserializer.deserialize_string(Visitor::<N>)
        } else {
            deserializer.deserialize_bytes(Visitor::<N>)
        }
    }
}

/// A vector capped at `N` elements during deserialization. The visitor
/// checks a declared length before reserving and also checks every element,
/// covering formats that omit or lie about their size hint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BoundedVec<T, const N: usize>(Vec<T>);

impl<T, const N: usize> BoundedVec<T, N> {
    pub fn new(value: Vec<T>) -> Result<Self, BoundExceeded> {
        if value.len() > N {
            return Err(BoundExceeded {
                limit: N,
                actual: value.len(),
            });
        }
        Ok(Self(value))
    }

    pub fn into_inner(self) -> Vec<T> {
        self.0
    }
}

impl<T, const N: usize> Deref for BoundedVec<T, N> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T, const N: usize> DerefMut for BoundedVec<T, N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T, const N: usize> IntoIterator for BoundedVec<T, N> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a BoundedVec<T, N> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'de, T, const N: usize> Deserialize<'de> for BoundedVec<T, N>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor<T, const N: usize>(std::marker::PhantomData<T>);

        impl<'de, T, const N: usize> serde::de::Visitor<'de> for Visitor<T, N>
        where
            T: Deserialize<'de>,
        {
            type Value = BoundedVec<T, N>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a sequence of at most {N} elements")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                if let Some(size) = seq.size_hint()
                    && size > N
                {
                    return Err(serde::de::Error::custom(BoundExceeded {
                        limit: N,
                        actual: size,
                    }));
                }
                let mut values = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(N));
                while let Some(value) = seq.next_element()? {
                    if values.len() == N {
                        return Err(serde::de::Error::custom(BoundExceeded {
                            limit: N,
                            actual: N.saturating_add(1),
                        }));
                    }
                    values.push(value);
                }
                Ok(BoundedVec(values))
            }
        }

        deserializer.deserialize_seq(Visitor::<T, N>(std::marker::PhantomData))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("wire collection bound exceeded: limit {limit}, got {actual}")]
pub struct BoundExceeded {
    pub limit: usize,
    pub actual: usize,
}

/// Stable identifier for a chunk in the world grid. Both client and server
/// key their `ChunkMap` by this — see the networking-design skill for why
/// we avoid `Entity` in cross-side messages.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkCoord(pub IVec3);

/// Client → server: a place-or-break request. Server → client (after
/// validation): the applied edit broadcast to everyone in AoI.
///
/// On a request:
///   - `slot != EMPTY` → place this block at `anchor`, rotated by `orientation`.
///     The server expands the def's footprint, validates every footprint
///     cell is empty (and chunks loaded), and applies atomically.
///   - `slot == EMPTY` → break. `anchor` is the cell the player clicked,
///     which may be any cell of a multi-cell entity; the server resolves
///     to the entity's anchor via the chunk sidecar before clearing.
///
/// On a broadcast:
///   - `slot != EMPTY` → a place was applied; `anchor` is authoritative
///     and `orientation` is the placed orientation.
///   - `slot == EMPTY` → a break was applied; `anchor` is the resolved
///     anchor of whatever was removed (single-cell or entity), and
///     `orientation` is the removed entity's orientation. Recipients use
///     this to rotate the def's footprint and clear all of its cells.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BlockEdit {
    pub anchor: IVec3,
    pub slot: BlockSlot,
    pub orientation: Cardinal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockWorkTarget {
    Plan { cell: IVec3 },
    Break { cell: IVec3 },
}

/// Client intent lease. The client refreshes an unchanged target every
/// 250 ms; the server expires it after 500 ms and owns all progress.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BlockWorkIntent {
    pub sequence: u64,
    pub target: Option<BlockWorkTarget>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthoritativeCellState {
    pub world: IVec3,
    pub slot: BlockSlot,
    pub entity: Option<EntryKind>,
}

/// Server fact containing every affected cell. Clients apply this payload
/// directly and never reconstruct a destructive footprint from stale state.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct TerrainEditRecord {
    pub anchor: IVec3,
    pub old_slot: BlockSlot,
    pub orientation: Cardinal,
    pub cells: BoundedVec<AuthoritativeCellState, MAX_APPLIED_BLOCK_CELLS>,
}

/// The ONE reach for direct world interactions — mine, place, pickup,
/// deposit, station work. The client's targeting raycast uses it so the
/// outline never advertises a verb the server would refuse; the server
/// enforces it (plus [`REACH_SLACK`]) on every mutating request. Keep
/// these two uses on the same constant: the 256-vs-12 split this
/// replaced meant the UI lied at range and the server dropped the
/// click silently.
pub const INTERACT_REACH: f32 = 12.0;

/// Reach for Plan-mode designation (Build/Remove tags). Deliberately
/// longer than [`INTERACT_REACH`] — tags are orders NPCs walk to, not
/// direct mutations, so planning works at "anything I can see" scale —
/// but still bounded so a client can't tag cells in chunks far outside
/// its own view.
pub const PLAN_REACH: f32 = 64.0;

/// Server-side tolerance added to reach gates. The client measures from
/// the camera eye, the server from the avatar pose; without slack a
/// click the client legitimately accepted at max range gets rejected by
/// the height difference.
pub const REACH_SLACK: f32 = 1.0;

/// Server → requesting client when a request was received and refused.
/// Feeds the worldspace-toast UI: the player sees *why* their click did
/// nothing instead of inferring packet loss. Sent only for requests a
/// well-behaved client believed were valid — silent drops remain the
/// right call for impossible inputs.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ActionRejected {
    /// Cell the toast anchors to — the target of the refused action.
    pub cell: IVec3,
    pub reason: RejectReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    OutOfReach,
    QueueFull,
    InventoryFull,
}

impl RejectReason {
    /// Player-facing toast text.
    pub fn text(self) -> &'static str {
        match self {
            RejectReason::OutOfReach => "Too far away",
            RejectReason::QueueFull => "Station queue is full",
            RejectReason::InventoryFull => "Station inventory is full",
        }
    }
}

/// Server-internal local-bus event, NOT a wire message. Emitted once per
/// world cell whose slot changed. Subscribers (room dirty-marking, drop
/// spawning, mod scripting hooks) react cell-by-cell without needing to
/// know about block-entity footprints.
///
/// `slot` is the *new* slot at this cell after the edit; `prev_slot` is
/// what occupied it before. For a place: `prev_slot == EMPTY` by
/// construction (the edit is rejected if the cell was occupied). For a
/// break: `prev_slot` is whatever was destroyed, which is what drops /
/// post-destroy effects need to look up in the registry.
///
/// `is_anchor` is true on exactly one edit per placed/destroyed block —
/// the anchor cell's (trivially true for single-cell blocks). Per-cell
/// subscribers (room dirtying, plan clearing, settling) ignore it;
/// per-*block* subscribers (drop spawning) key on it so a multi-cell
/// footprint doesn't multiply a once-per-block effect.
#[derive(Message, Clone, Copy, Debug)]
pub struct CellEdit {
    pub world: IVec3,
    pub slot: BlockSlot,
    pub prev_slot: BlockSlot,
    pub is_anchor: bool,
}

/// Server → client on connect: the full mod-set fingerprint — slot ↔ id
/// tables for every registry the wire references, plus a content hash
/// over the serialized defs. The client diffs it against its own
/// registries and refuses the session on any disagreement (a divergent
/// mod set desyncs silently long after connect, which is strictly worse
/// than failing loudly at the door). Construction and comparison live
/// in `modset.rs`.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct ModSetManifest {
    /// Slot index = position in this Vec. Slot 0 is always `vanilla:empty`.
    pub blocks: Vec<BlockId>,
    /// Slot index = position, same convention as `blocks`.
    pub items: Vec<block_junk_mod_api::items::ItemId>,
    /// Recipe ids in slot order.
    pub recipes: Vec<String>,
    /// NPC kind ids, sorted (the registry is keyed by string, not slot).
    pub npc_kinds: Vec<String>,
    /// Room pattern ids in registration order. Pattern ids cross the
    /// wire in [`RoomSummary`] and the client resolves display names /
    /// constraints locally, so disagreement here desyncs the room UI.
    pub room_patterns: Vec<String>,
    /// Stable FNV-1a over the serialized block/item/recipe/room-pattern
    /// defs in slot order. Catches same-ids-but-different-definitions.
    pub defs_hash: u64,
}

pub const READY_DOMAIN: &[u8] = b"block-junk/client-ready/v1\0";

/// First application message sent after the transport connects. No game
/// state, replication, or subscriptions are enabled before its challenge is
/// answered and verified.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct ServerHello {
    pub protocol_id: u64,
    pub connection_nonce: [u8; 32],
    pub manifest_hash: [u8; 32],
    pub spatial_registry_fingerprint: [u8; 32],
    pub manifest: ModSetManifest,
}

/// Signed proof of identity and content agreement.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct ClientReady {
    pub public_key: [u8; 32],
    pub signature: BoundedVec<u8, 64>,
}

/// Verified player identity attached to the server connection entity.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthenticatedPlayer {
    pub player_id: u64,
    pub public_key: [u8; 32],
    pub administrator: bool,
}

/// Application readiness. This, not Lightyear's `Connected`, is the sole
/// lifecycle gate for avatar creation, replication, AoI, and feature sync.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct GameReady;

pub fn ready_payload(
    protocol_id: u64,
    connection_nonce: &[u8; 32],
    manifest_hash: &[u8; 32],
    spatial_registry_fingerprint: &[u8; 32],
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(READY_DOMAIN.len() + 8 + 32 + 32 + 32);
    payload.extend_from_slice(READY_DOMAIN);
    payload.extend_from_slice(&protocol_id.to_le_bytes());
    payload.extend_from_slice(connection_nonce);
    payload.extend_from_slice(manifest_hash);
    payload.extend_from_slice(spatial_registry_fingerprint);
    payload
}

pub fn manifest_hash(manifest: &ModSetManifest) -> [u8; 32] {
    let bytes = bincode::serde::encode_to_vec(manifest, bincode::config::standard())
        .expect("the mod-set manifest is serializable");
    *blake3::hash(&bytes).as_bytes()
}

/// Server → client only: tells a client what to put in a chunk it just
/// entered AoI of. Two payload variants — see `ChunkData`. Subsequent
/// changes arrive as `BlockEdit` broadcasts; this message fires once
/// per (chunk, client) pair on AoI entry.
///
/// Unedited chunks travel with `entities` empty — terrain has no
/// block-entities. Edited chunks ship the sidecar so anchors/ghosts
/// arrive atomically with the slot grid.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ChunkData {
    /// The chunk has never been edited. The client generates it locally
    /// from the deterministic terrain function (`Chunk::from_terrain`).
    /// ~13 B on the wire.
    Procedural,
    /// Exact padded slot grid for an edited chunk.
    Raw(BoundedVec<BlockSlot, CHUNK_PADDED_CELLS>),
    /// Run-length encoded padded grid. The decoded run counts must sum
    /// to exactly [`CHUNK_PADDED_CELLS`].
    Rle(BoundedVec<BlockRun, CHUNK_PADDED_CELLS>),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BlockRun {
    pub slot: BlockSlot,
    pub count: u32,
}

/// Marker component for "thing with a body that can move and interact" —
/// the shared DNA between player avatars and NPCs. Carries the same
/// physics state (`AvatarPose`, `AvatarVelocity`, `AvatarOnGround`,
/// `MovementMode`) and consumes the same `MovementIntent` regardless of
/// whether the intent comes from a connected client or a brain.
///
/// Replicated so the client side can render and (future) interact with
/// any actor uniformly. Specialised markers like `Avatar` (player) and
/// `Npc` (mob) ride alongside to disambiguate when needed — "give every
/// actor a name tag" wants `Actor`, "attach a camera to my own avatar"
/// wants `Avatar` + `Predicted`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Actor;

/// Marker for "this actor's body is currently being driven by a
/// dedicated use-slot, not by the physics tick." Inserted when an NPC
/// enters a goal that snaps them onto a block's [`UseSlot`] (sleeping
/// in a bed, eventually sitting in a chair, striking at a forge);
/// removed on goal exit. While present:
///
/// - The NPC physics step skips them (no gravity, no walk_step sweep,
///   so the snapped pose translation isn't pulled back to the floor or
///   nudged by the AABB sweep).
/// - The server-side soft-actor-separation pass skips them (a
///   passer-by who shoulder-bumps the bed shouldn't slide the
///   sleeping body off it).
///
/// Server-only state — not replicated. Clients infer the equivalent
/// "don't touch this body" behaviour from their existing filters
/// (the client soft-separate pass only mutates `Predicted` actors,
/// and interpolated NPCs were never client-pushable in the first
/// place). When a player ever enters a use-slot (chair, vehicle) we
/// will need to flip this to a replicated marker so the predicted
/// owner skips physics on their end too.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct KinematicLock;

/// Marker component on the server-side player-avatar entity. Replicated to
/// every client so they can render a body (or, on the owner side, attach a
/// camera). Paired with the predicted state components below. Coexists
/// with `Actor` — every Avatar is also an Actor, but not every Actor is
/// an Avatar (NPCs are Actors without `Avatar`).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Avatar;

/// Server-set animation clip override for an NPC. Replicated so every
/// client renders the same clip.
///
/// `None` ⇒ no override: the client picks idle vs walk via velocity
/// hysteresis against the NPC kind's default clips. This is the
/// common case — every NPC sits in this state outside of explicit
/// stationary actions.
///
/// `Some(id)` ⇒ play this clip until cleared. Server sets this on
/// transitions into stationary states (Working, Interacting with a
/// use-slot animation) and clears it on transitions back out.
/// `id` is an [`AnimationId`](block_junk_mod_api::animations::AnimationId)
/// the client resolves through its cached registry to an
/// `AnimationNodeIndex`.
#[derive(Component, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Reflect)]
pub struct NpcAnimOverride(pub Option<String>);

/// Per-avatar movement mode. Server-authoritative — the server decides
/// when a creative-mode toggle is allowed; today the request is granted
/// unconditionally. Replicated + predicted so the owner client stays in
/// sync without needing to wait a round-trip after pressing F1.
#[derive(
    Component, Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Reflect,
)]
pub enum MovementMode {
    /// Walking: gravity, jump, AABB collision against the world.
    #[default]
    Walk,
    /// Creative-mode flight: 6-dof, no gravity, no collision.
    Fly,
}

/// Horizontal + vertical velocity. Predicted state — needs to roll back
/// with the rest of the simulation so the owner restarts from the
/// authoritative velocity after a server correction. Not interpolated:
/// remote viewers don't need it (they render position only).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
pub struct AvatarVelocity(pub Vec3);

/// True if the controller's last sweep ended on a downward Y contact.
/// Read by the controller to gate jumps and ground friction. Predicted
/// state for the same reason as `AvatarVelocity`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
pub struct AvatarOnGround(pub bool);

/// Authoritative pose of an `Avatar`, written on the server from `PlayerPose`
/// ingests and replicated out as state. Sixteen bytes (Vec3 + yaw f32) vs
/// the forty a full `Transform` would cost — rotation+scale of the full
/// transform are dead weight when all we render is a yaw-rotated cuboid.
/// Quantize to i16/u16 fixed-point if avatar bandwidth ever shows up in
/// profiles; the precision needed (~cm of position, ~tenth of a degree of
/// yaw) fits comfortably.
///
/// Registered with `.add_prediction().add_linear_interpolation()` (see
/// network.rs) so the owner's copy is predicted-with-rollback and remote
/// copies are interpolated between server samples — `Ease` below defines
/// the lerp.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
pub struct AvatarPose {
    pub translation: Vec3,
    pub yaw: f32,
}

impl Ease for AvatarPose {
    fn interpolating_curve_unbounded(start: Self, end: Self) -> impl Curve<Self> {
        FunctionCurve::new(Interval::UNIT, move |t| {
            // Yaw lerp via shortest arc: wrap the delta to [-π, π] before
            // scaling so a yaw going from +175° to -175° interpolates the
            // 10° short way, not the 350° long way around.
            let two_pi = std::f32::consts::TAU;
            let mut d = (end.yaw - start.yaw) % two_pi;
            if d > std::f32::consts::PI {
                d -= two_pi;
            } else if d < -std::f32::consts::PI {
                d += two_pi;
            }
            AvatarPose {
                translation: Vec3::lerp(start.translation, end.translation, t),
                yaw: start.yaw + d * t,
            }
        })
    }
}

/// Server → client: this chunk has left your AoI; despawn your local copy.
/// The server may still hold its data (we don't evict the master record yet),
/// but you don't need it anymore.
/// Ordered server facts for chunk staging and all client-visible spatial
/// datasets. Deltas received between begin and commit are buffered by the
/// client and applied atomically with the staged terrain.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub enum SpatialMessage {
    BeginChunk {
        transaction: u64,
        coord: ChunkCoord,
        terrain: ChunkData,
        entities: BoundedVec<EntityEntry, CHUNK_PADDED_CELLS>,
    },
    PartitionPage {
        transaction: u64,
        dataset: crate::spatial::DatasetId,
        schema_fingerprint: u64,
        /// Dataset-defined sequence of bounded records. The entire page,
        /// including record framing, is capped rather than only each record.
        payload: BoundedVec<u8, MAX_SPATIAL_PAGE_BYTES>,
    },
    Delta {
        dataset: crate::spatial::DatasetId,
        schema_fingerprint: u64,
        payload: BoundedVec<u8, MAX_SPATIAL_PAGE_BYTES>,
    },
    CommitChunk {
        transaction: u64,
        coord: ChunkCoord,
    },
    LeaveChunk {
        coord: ChunkCoord,
    },
    Toast {
        cell: IVec3,
        text: BoundedString<256>,
    },
}

/// Single ordered reliable server-to-client lane for [`SpatialMessage`].
pub struct SpatialChannel;

/// What an `Actor` (player or NPC) is currently carrying. The whole
/// inventory: a single stack of one item kind, or nothing. See the
/// `ephemeral-single-stack-carry` memory for why this is intentionally
/// minimal — the constraint is the gameplay.
///
/// Invariant: `count == 0` ⇒ `item == None`. The `pickup` /
/// `drop_all` helpers maintain it; direct field writes shouldn't
/// violate it.
///
/// Server-authoritative; replicated to all clients without prediction.
/// The owner reads their own carry off their `Predicted` avatar copy,
/// HUD lag = one server round-trip. Remote players' carry isn't
/// rendered yet — float-above-head visualisation lands when NPC
/// haul-state visuals do (Phase 4).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Carrying {
    pub item: Option<ItemSlot>,
    pub count: u32,
}

impl Carrying {
    pub fn is_empty(&self) -> bool {
        self.count == 0 || self.item.is_none()
    }

    /// Whether `item` can be added to this stack given the carry cap.
    /// Empty hand always accepts a single unit; a partial matching stack
    /// accepts up to `cap - count` more.
    pub fn can_accept(&self, item: ItemSlot, cap: u32) -> bool {
        if self.is_empty() {
            cap > 0
        } else {
            self.item == Some(item) && self.count < cap
        }
    }

    /// Add up to `want` units of `item` in one go — for withdrawing from
    /// a multi-unit pile. Capped by the carry limit and the single-kind
    /// rule. Returns how many units were actually added (0 if the hand
    /// holds a different item or is already full).
    pub fn pickup_many(&mut self, item: ItemSlot, want: u32, cap: u32) -> u32 {
        let room = if self.is_empty() {
            cap
        } else if self.item == Some(item) {
            cap.saturating_sub(self.count)
        } else {
            0
        };
        let take = want.min(room);
        if take == 0 {
            return 0;
        }
        self.item = Some(item);
        self.count += take;
        take
    }

    /// Empty the stack. Returns what was being held (if anything) so the
    /// caller can spawn the corresponding `WorldItem`s.
    pub fn drop_all(&mut self) -> Option<(ItemSlot, u32)> {
        let item = self.item.take()?;
        let count = self.count;
        self.count = 0;
        if count == 0 {
            None
        } else {
            Some((item, count))
        }
    }
}

/// Single-slot tool an actor is wielding. Distinct from
/// [`Carrying`] — the carry stack is for resources (logs, ore) that
/// get hauled and consumed; the tool slot is for items that *enable*
/// actions (axe, pickaxe, hammer) and live on the actor until they're
/// swapped or dropped. Splitting the two means a hauler full of logs
/// still has their axe equipped, and picking up an axe doesn't bump
/// your log stack.
///
/// `item == None` is the canonical empty state. Set via the server's
/// pickup-routing branch (any picked-up [`crate::items::ItemSlot`]
/// whose def has non-empty
/// [`ItemDef::tool_tags`](block_junk_mod_api::items::ItemDef::tool_tags)
/// goes here instead of into Carrying), and persists in v9 saves.
/// Replicated to every client so HUDs render the local player's tool
/// chip + future "NPC holds axe" visuals can read it; no prediction
/// because pickup is a server-authoritative discrete event.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EquippedTool {
    pub item: Option<ItemSlot>,
}

/// A loose item sitting in the world — what a destroyed block leaves
/// behind, what an actor sets down when they drop their carry stack,
/// and (Phase 4) what an NPC walks past and picks up to deliver to a
/// plan. Server-authoritative entity; replicated to every client.
///
/// `item` is the registry slot (compact wire format, like `BlockSlot`
/// for chunk storage). `translation` is the entity's world position and
/// the server-side source of truth (haul/pickup proximity and the save
/// read it, not `Transform`). It is set on spawn and then mutated only
/// when the item settles: drops fall to solid ground when spawned or
/// when the block under them is mined, and rise when a block is built
/// into their cell (see `settle_item_cell`). Each change is one
/// replicated delta — items are otherwise static, so there is no
/// per-tick traffic. `translation.y` is always the resting cell's base
/// plus a tiny lift, so `translation.floor()` recovers the owning cell.
/// Yaw is omitted for now (items are tumbled visually with a per-entity
/// random offset derived from spawn position; no facing direction to
/// track).
///
/// `count` (S2) is how many units this entity represents. Loose drops
/// are `count = 1` — one entity per unit, as before. The NPC tidy job is
/// the only thing that mints `count > 1`: it sweeps loose units into a
/// single "pile" entity snapped to a storage cell, which the client
/// renders with a stack-tier mesh (`ItemDef::pile_mesh`).
///
/// INVARIANT: `count` is immutable for a live entity. A withdrawal or
/// tidy-merge that changes a stack's size **despawns and respawns** the
/// entity with the new count rather than mutating in place. This keeps
/// the client render trivial — the tier mesh is chosen once at spawn
/// (`attach_world_item_visuals`), never swapped — and is safe because a
/// stack is only ever mutated while reserved by a single NPC. Bevy's
/// `WorldAssetRoot` does not reliably tear down the old scene on an
/// in-place handle change, so we sidestep in-place mesh swaps entirely.
#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldItem {
    pub item: ItemSlot,
    pub translation: Vec3,
    pub count: u32,
}

/// What a player has tagged a cell to become. Lives in the shared [`Plans`]
/// resource (server-authoritative, mirrored on each client). Tagged cells
/// aren't world state — they're work orders for NPCs to consume.
///
/// `Remove` means "I want whatever is here to be gone." `Build` carries
/// the slot + orientation so an NPC working the plan knows what to
/// construct and how to rotate it. Multi-cell footprints are recorded
/// at the anchor cell only — the NPC expands the footprint at work time
/// against the live registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlanKind {
    Remove,
    Build {
        slot: BlockSlot,
        orientation: Cardinal,
    },
}

impl PlanKind {
    /// The [`WorkVerb`] this plan's work runs under — Build plans place
    /// a block, Remove plans break one. Tool gates are per-verb
    /// ([`WorkAction::tool_for`](block_junk_mod_api::blocks::WorkAction::tool_for));
    /// every gate site derives the verb through here.
    pub fn work_verb(&self) -> block_junk_mod_api::blocks::WorkVerb {
        match self {
            PlanKind::Build { .. } => block_junk_mod_api::blocks::WorkVerb::Build,
            PlanKind::Remove => block_junk_mod_api::blocks::WorkVerb::Destroy,
        }
    }
}

/// Full state of one tagged cell: what it should become *plus* the
/// progress of material delivery for Build plans. Remove plans have an
/// empty `materials` vec (nothing needs to be delivered to break a
/// block); Build plans carry one entry per item kind required, with
/// `present` rising toward `needed` as the player or NPCs deposit
/// resources.
///
/// Replicated to every client so each can render the right outline
/// colour (desaturated green when materials still pending, full green
/// when ready) and decide self-work eligibility locally.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanState {
    pub kind: PlanKind,
    #[serde(default)]
    pub materials: Vec<MaterialEntry>,
}

impl PlanState {
    pub fn new(kind: PlanKind, materials: Vec<MaterialEntry>) -> Self {
        Self { kind, materials }
    }

    /// True when every material entry has its full count delivered, or
    /// when the plan needs no materials at all (every Remove plan).
    pub fn is_satisfied(&self) -> bool {
        self.materials.iter().all(|m| m.present >= m.needed)
    }

    /// How many more units of `item` can still be deposited before
    /// this plan is satisfied for that material. `0` means the plan
    /// doesn't accept this item kind (either not needed, or already
    /// fully delivered).
    pub fn remaining_for(&self, item: ItemSlot) -> u32 {
        self.materials
            .iter()
            .find(|m| m.item == item)
            .map(|m| m.needed.saturating_sub(m.present))
            .unwrap_or(0)
    }
}

/// One material requirement on a [`PlanState`]: which item, how many
/// needed in total, how many delivered so far. Capped at `needed` on
/// deposit so a deposit-too-big call doesn't overshoot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialEntry {
    pub item: ItemSlot,
    pub needed: u32,
    pub present: u32,
}

/// Client → server: tag (`kind = Some`) or untag (`kind = None`) a cell.
/// Server → client: the canonical applied edit, broadcast to everyone in
/// the world. Same bidirectional shape as [`BlockEdit`] for symmetry.
///
/// Server validation:
///   - `Some(Remove)` rejected if the cell is currently empty.
///   - `Some(Build {..})` rejected if the cell is currently solid.
///   - `None` succeeds even if no tag exists (idempotent untag).
///
/// `materials` is **server-set only**. Client requests leave it empty
/// (the field defaults via `serde(default)`); the server populates it
/// from [`BlockDef::materials`](block_junk_mod_api::blocks::BlockDef::materials)
/// on a Build tag and rebroadcasts. Subsequent deposits / fills also
/// fire `PlanEdit` broadcasts so clients see the updated `present`
/// counts in their mirrors without a separate message type.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct PlanEdit {
    pub cell: IVec3,
    pub kind: Option<PlanKind>,
    #[serde(default)]
    pub materials: Vec<MaterialEntry>,
}

/// Server → client on connect: the current state of the [`Plans`] map.
/// Sparse — only tagged cells. Each entry carries the full PlanState
/// (kind + materials progress) so a fresh-connecting client renders
/// the right outline state immediately.
/// Wire snapshot of one *matched* room. Detection stays server-side;
/// clients mirror just enough to surface feedback: the toast on
/// recognition, the inspect-panel "Room: …" line, debug outlines.
/// `pattern` is the pattern id — the client resolves the display name
/// through its own `RoomPatternRegistry` (the mod-set gate guarantees
/// both sides registered identical patterns).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoomSummary {
    pub room_id: u32,
    pub pattern: String,
    /// Floor cell nearest the room's centroid — walkable by
    /// construction; where recognition toasts anchor.
    pub anchor: IVec3,
    pub bbox_min: IVec3,
    pub bbox_max: IVec3,
    pub floor_area: u32,
}

/// Server → clients when a room gains or changes a matched pattern
/// (upsert by `room_id`). Rooms that lose their match arrive as
/// [`RoomRemove`] instead.
/// Bulk version of [`PlanEdit`]. All cells in `cells` are tagged with
/// the same `kind` (or cleared if `kind` is `None`). Server validates
/// each cell against the same rules as `PlanEdit` and drops the ones
/// that fail — partial application is OK; the user sees the diff in
/// the broadcast that comes back. Bidirectional shape mirrors
/// [`PlanEdit`].
///
/// Plan rectangles can get large; the client caps the per-message cell
/// count at [`PLAN_EDIT_BATCH_MAX`] and splits bigger selections into
/// multiple messages.
///
/// `materials` is shared across the whole batch: every cell tagged
/// Build by this batch uses the same block (the request's `kind`
/// carries one slot), so the materials_needed list is uniform.
/// Server-set on broadcast, defaulted empty on client request.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct PlanEditBatch {
    pub kind: Option<PlanKind>,
    pub cells: BoundedVec<IVec3, PLAN_EDIT_BATCH_MAX>,
    #[serde(default)]
    pub materials: Vec<MaterialEntry>,
}

/// Client → server: deposit one or more units of the player's
/// [`Carrying`] stack into the Build plan at `cell`. Server reads the
/// requesting player's carry, looks up the plan's outstanding need
/// for the carried item, decrements the player's carry by that
/// amount, increments `materials.present` on the plan, and broadcasts
/// the updated state via `PlanEdit`.
///
/// Empty-handed clicks, mismatched item types, and plans that don't
/// need anything more silently no-op — same degradation pattern as
/// pickup.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DepositRequest {
    pub cell: IVec3,
}

/// Max cells per [`PlanEditBatch`] message. Chosen to keep a single
/// message comfortably under the lightyear reliable-channel fragment
/// budget; 4096 IVec3 cells = ~48 KB raw. A 64×64 face drag is 4096
/// cells — at that size we split into two messages.
pub const PLAN_EDIT_BATCH_MAX: usize = 4096;

/// Bidirectional storage-zone designation (Storage mode). Client →
/// server as a request; server validates (reach + batch cap), applies
/// to its `StorageZones`, and broadcasts the accepted set back in the
/// same shape. `add = true` marks the cells as storage, `false`
/// clears them. Accepted state returns through the ordered spatial lane.
///
/// Zone cells are the *air* cells items sit in (solid floor below),
/// not the floor blocks — the same cell a pile or container occupies.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct StorageEditBatch {
    pub add: bool,
    pub cells: BoundedVec<IVec3, PLAN_EDIT_BATCH_MAX>,
}

/// Server → client on connect: every storage-zone cell. Shares the
/// batch cap with [`PLAN_EDIT_BATCH_MAX`]; bigger zone sets split
/// across multiple messages (ordering within the channel keeps them
/// coherent, and add-only application makes splits commutative).
/// Small critical world-command lane. Ordered reliable so edit *requests*,
/// targeted rejections, request/response UX, and dev commands preserve
/// the order a player expects without waiting behind bulk state sync.
pub struct WorldChannel;

/// Reliable lane for typed client requests whose latency profile is less
/// critical than direct world commands. Authoritative spatial facts use
/// [`SpatialChannel`] instead.
pub struct StateSyncChannel;

/// Low-priority latest-wins periodic sync lane. Dropping an older sample
/// is acceptable because the next one supersedes it.
pub struct PeriodicSyncChannel;

/// Per-tick movement intent. The unified input vocabulary for *anything*
/// with a body — players via lightyear's input pipeline (`input_native`,
/// sequence-numbered redundancy so a dropped UDP packet doesn't drop a
/// tick), NPCs via their brain writing the component directly. Both server
/// (authority) and the owning client (prediction) run the same controller
/// against this in `FixedUpdate`.
///
/// Wishdir is encoded as three i8s (-1/0/+1 per axis) — fits in 3 bytes
/// where a Vec3 would take 12. `dyaw` is the *change* in yaw since the
/// last tick (radians), not the absolute yaw — the actor's pose owns the
/// running yaw, the source (player input or brain steering) just reports
/// motion. Pitch isn't here yet (no head/torso split, the avatar is a
/// single yaw-rotated cuboid).
///
/// `Default` means "no movement, no rotation this tick." The lightyear
/// input buffer treats a missing per-tick input as "use the previous
/// one"; with delta-yaw a duplicated tick over-rotates, but the buffer
/// only duplicates when packets drop entirely, which is rare and a 10 ms
/// slice of mouse motion at the wrist isn't catastrophic.
///
/// Field set is the union of "player keys" and "NPC brain output." NPCs
/// leave `toggle_mode` alone (no fly mode for them) and use `wishdir[0]`
/// + `wishdir[2]` only — the y axis is for player fly mode.
#[derive(Component, Clone, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
pub struct MovementIntent {
    /// Per-axis -1, 0, or +1. X is strafe (right/left), Y is fly up/down,
    /// Z is forward/back. Controller interprets these per `MovementMode`.
    pub wishdir: [i8; 3],
    /// Held this tick — jump in walk mode, ascend in fly mode (redundant
    /// with `wishdir.y` but kept separate so the controller can tell
    /// "jump just-pressed" from "fly-up held").
    pub jump: bool,
    /// Just-pressed this tick — server flips `MovementMode` on the actor.
    /// Player only (NPCs don't toggle fly). Later gated on creative-mode
    /// permissions.
    pub toggle_mode: bool,
    /// "Use the thing in front of me" this tick. Players will get this
    /// from a key (E or similar); NPC brains set it when their goal
    /// requires interacting with a block-entity (use bed, open door).
    /// Currently inert — the controller doesn't act on it yet.
    pub interact: bool,
    /// Yaw delta in radians since the last tick (accumulated mouse motion
    /// for players, brain steering delta for NPCs). The controller does
    /// `pose.yaw += dyaw` — pose.yaw is the truth, and a default
    /// intent naturally leaves the pose alone.
    pub dyaw: f32,
}

impl MapEntities for MovementIntent {
    fn map_entities<M: EntityMapper>(&mut self, _: &mut M) {}
}

/// Game-wide schedule ordering. Plugins assign their systems to one of these
/// sets so input → simulation → re-mesh runs in one frame in the right order,
/// even across plugin boundaries.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum GameSet {
    Input,
    Simulation,
    SpatialSync,
    PostSimulation,
}

/// One in-game day in real seconds. Picked short enough that a session
/// always sees several day/night transitions, long enough that the
/// transition itself doesn't feel like a flicker. Tune to taste.
pub const DAY_LENGTH_SECS: f32 = 600.0;

/// Server-authoritative day clock. `time_of_day` is the fraction of the
/// current day elapsed: 0.0 = midnight, 0.25 = sunrise, 0.5 = noon,
/// 0.75 = sunset. `day` counts completed days since session start.
/// Lives as a `Resource` on both sides — server ticks it forward,
/// client snaps it from `WorldClockSync` messages and locally
/// extrapolates between syncs so the sun doesn't visibly tick once
/// a second. Persisted in the save file (`SaveFile::world_clock`)
/// so a reload picks up where the world left off.
#[derive(Resource, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct WorldClock {
    pub day: u32,
    pub time_of_day: f32,
}

impl WorldClock {
    /// True during the half of the day when the sun is below the horizon.
    /// Mirrors the sun-rotation math in the client lighting system.
    pub fn is_night(self) -> bool {
        // Sun-up window is sunrise (0.25) → sunset (0.75). Anything else
        // is night. Phrasing in terms of "below 0.25 or above 0.75" keeps
        // the planner snapshot and the visuals reading the same truth.
        self.time_of_day < 0.25 || self.time_of_day >= 0.75
    }

    /// Advance the clock by `dt` real-time seconds, scaled by
    /// `DAY_LENGTH_SECS`. Server uses this every fixed tick; client uses
    /// it during render frames to extrapolate between sync messages.
    pub fn advance(&mut self, dt: f32) {
        self.time_of_day += dt / DAY_LENGTH_SECS;
        while self.time_of_day >= 1.0 {
            self.time_of_day -= 1.0;
            self.day = self.day.wrapping_add(1);
        }
    }
}

/// Server → client periodic sync of the world clock. Tiny (5 bytes)
/// and sent at low cadence — the client extrapolates locally between
/// messages. Lives on [`PeriodicSyncChannel`]: newer samples supersede
/// older ones, so dropping or skipping an old sample is acceptable.
#[derive(Message, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct WorldClockSync {
    pub day: u32,
    pub time_of_day: f32,
}

/// Client → server: debug request to fast-forward the world by
/// `secs` real-time seconds. Intended for the dev debug panel only;
/// no permission check today (see the wider note on debug messages).
///
/// "Fast-forward" means two things atomically: the [`WorldClock`]
/// rolls forward by `secs / DAY_LENGTH_SECS` (wrapping into the
/// next day), and every NPC's needs decay by `decay_per_sec * secs`
/// — i.e. the world experiences `secs` of time without the player
/// having to wait. Negative values are clamped to 0 (going backward
/// is weird — the existing-deficits don't ungrow; if you want to
/// rewind the clock, just keep advancing through the next cycle).
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DebugAdvanceTime {
    pub secs: f32,
}

/// Client → server: debug request to bump every NPC's value for the
/// named need by `delta` (typically positive — "make everyone more
/// tired" or "more hungry" to trigger behaviour without waiting
/// minutes for natural decay). Server clamps the resulting per-NPC
/// value to [0, 1] so a runaway delta can't break the math, and
/// silently ignores `need` ids the registry doesn't know about
/// (e.g. a typo from the UI).
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct DebugBumpNeed {
    pub need: BoundedString<MAX_WIRE_ID_BYTES>,
    pub delta: f32,
}

/// Client → server: instantly fill the materials of the player's
/// nearest unsatisfied Build plan. Phase-4 testing prerequisite — lets
/// us verify NPC pickup of fully-materialled plans without hauling
/// each unit by hand. Server picks the nearest plan to the requesting
/// player's avatar; no-op if no pending plans exist within range.
#[derive(Message, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct DebugFillNearestPlan;

/// Client → server: drop the player's currently-equipped tool back
/// to the world as a `WorldItem`. Lands at the
/// `drop_target_position` (one tile ahead of the player when that
/// cell is standable, else at the player's feet) — same helper the
/// carry-drop uses. No-op when the tool slot is empty.
#[derive(Message, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct DropToolRequest;

/// Client → server: place a `vanilla:workbench` block one tile ahead
/// of the requesting player. Phase 6a testing prerequisite — until
/// the player learns the plan-build flow well enough to deliberately
/// craft a workbench, the debug button bypasses the chop-wood +
/// tag-plan + deliver-materials loop and just drops a workbench so
/// the crafting flow itself can be exercised. Silent skip if the
/// target cell is occupied or if the engine's `vanilla:workbench`
/// id isn't registered.
#[derive(Message, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct DebugSpawnWorkbench;

/// Client → server: spawn one of each vanilla tool item
/// (axe / hammer / pickaxe) as `WorldItem`s near the requesting
/// player's feet. Phase-5a testing prerequisite — until Phase 5b
/// adds NPC tool fetch (or a recipe system surfaces tools as
/// craftable), the starter axe is the only way to get a tool, which
/// blocks swap + tool-mismatch testing. Server resolves the ids via
/// the live `ItemRegistry`; unknown ids are skipped silently so a
/// mod that drops one of the three doesn't crash the button.
#[derive(Message, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct DebugSpawnTools;

/// Client → server: ask the server to dump the current authoritative
/// state of one NPC so the requesting client's inspection panel can
/// show needs, current goal, and goal target. `npc_id` mirrors
/// [`crate::npc::NpcId`]'s inner u64 — kept as a plain u64 in the
/// protocol so `protocol.rs` doesn't have to import `npc.rs` and
/// invert the existing dependency.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct RequestNpcDetails {
    pub npc_id: u64,
}

/// Client → server: pick up the loose item closest to `target` in world
/// space. The client raycast resolves which `WorldItem` is under the
/// cursor and sends its translation; the server does a fuzzy spatial
/// match (`PICKUP_MATCH_RADIUS`) to find the actual entity. Entity ids
/// don't cross the wire here — `WorldItem` doesn't carry a stable
/// network id, and a fuzzy translation match is enough since loose
/// items don't move between when the client clicks and when the
/// server receives.
///
/// Server validates: the player has carry capacity for the item kind,
/// the player isn't unreasonably far from `target`. Failure is
/// silent — the HUD just doesn't update.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PickupRequest {
    pub target: Vec3,
}

/// Client → server: drop the player's entire carry stack at their
/// feet. No payload — the server reads the player's Carrying to
/// know what to drop. No-op when the player is empty-handed.
#[derive(Message, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct DropRequest;

/// Server → client: targeted reply to a [`RequestNpcDetails`]. Sent on
/// `WorldChannel` to the requesting connection only — clients don't
/// see each other's inspection traffic. `current_goal` is a
/// pre-formatted human string ("sleeping (12.4s)", "moving to 14
/// cells, on_arrive: work") so the client UI doesn't need to mirror
/// the engine's full Goal enum to render it.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct NpcDetails {
    pub npc_id: u64,
    pub kind: String,
    pub needs: std::collections::HashMap<String, f32>,
    /// Rolled per-NPC stats (laziness etc.) — fixed at spawn, shown in
    /// the inspect panel alongside the live needs.
    #[serde(default)]
    pub stats: std::collections::HashMap<String, f32>,
    pub current_goal: String,
    pub target_cell: Option<IVec3>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::ItemSlot;
    use proptest::prelude::*;

    /// `pickup_many` is the withdrawal primitive for both NPC and player
    /// pile grabs — it must never exceed the cap, mix kinds, or miscount.
    #[test]
    fn pickup_many_clamps_and_respects_kind() {
        let log = ItemSlot(1);
        let stone = ItemSlot(2);
        let cap = 5;

        // Empty hand takes up to the cap.
        let mut c = Carrying::default();
        assert_eq!(c.pickup_many(log, 3, cap), 3);
        assert_eq!((c.item, c.count), (Some(log), 3));

        // Partial matching stack tops up only to the cap; returns the
        // amount actually added, not what was requested.
        assert_eq!(c.pickup_many(log, 10, cap), 2);
        assert_eq!(c.count, cap);

        // Full stack accepts nothing more.
        assert_eq!(c.pickup_many(log, 1, cap), 0);
        assert_eq!(c.count, cap);

        // A different kind is refused outright (single-kind carry).
        let mut c2 = Carrying::default();
        assert_eq!(c2.pickup_many(log, 2, cap), 2);
        assert_eq!(c2.pickup_many(stone, 2, cap), 0);
        assert_eq!((c2.item, c2.count), (Some(log), 2));

        // want == 0 is a no-op.
        assert_eq!(c2.pickup_many(log, 0, cap), 0);
    }

    #[test]
    fn bounded_wire_values_reject_oversize_payloads() {
        let oversized = vec![7_u16; 9];
        let bytes = bincode::serde::encode_to_vec(&oversized, bincode::config::standard()).unwrap();
        let decoded = bincode::serde::decode_from_slice::<BoundedVec<u16, 8>, _>(
            &bytes,
            bincode::config::standard(),
        );
        assert!(decoded.is_err());

        let oversized = "x".repeat(129);
        let bytes = bincode::serde::encode_to_vec(&oversized, bincode::config::standard()).unwrap();
        let decoded = bincode::serde::decode_from_slice::<BoundedString<128>, _>(
            &bytes,
            bincode::config::standard(),
        );
        assert!(decoded.is_err());
    }

    proptest! {
        #[test]
        fn bounded_vec_decode_never_accepts_more_than_limit(values in proptest::collection::vec(any::<u16>(), 0..256)) {
            let bytes = bincode::serde::encode_to_vec(&values, bincode::config::standard()).unwrap();
            let result = bincode::serde::decode_from_slice::<BoundedVec<u16, 64>, _>(
                &bytes,
                bincode::config::standard(),
            );
            prop_assert_eq!(result.is_ok(), values.len() <= 64);
        }

        #[test]
        fn arbitrary_bytes_do_not_panic_bounded_decoders(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = bincode::serde::decode_from_slice::<BoundedVec<u32, 64>, _>(
                &bytes,
                bincode::config::standard(),
            );
            let _ = bincode::serde::decode_from_slice::<BoundedString<128>, _>(
                &bytes,
                bincode::config::standard(),
            );
            let _ = bincode::serde::decode_from_slice::<SpatialMessage, _>(
                &bytes,
                bincode::config::standard().with_limit::<MAX_REASSEMBLED_MESSAGE_BYTES>(),
            );
        }
    }
}
