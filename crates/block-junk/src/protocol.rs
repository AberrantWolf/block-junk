use bevy::prelude::*;
use block_junk_mod_api::blocks::BlockId;
use serde::{Deserialize, Serialize};

use crate::blocks::BlockSlot;

pub const CHUNK_SIZE: u32 = 32;
pub const CHUNK_PADDED: u32 = CHUNK_SIZE + 2;

/// Stable identifier for a chunk in the world grid. Both client and server
/// key their `ChunkMap` by this — see the networking-design skill for why
/// we avoid `Entity` in cross-side messages.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkCoord(pub IVec3);

/// Client → server: an edit request. Server → client (after validation):
/// the applied edit broadcast to everyone in the chunk's AoI.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BlockEdit {
    pub coord: ChunkCoord,
    pub pos: IVec3,
    pub block: BlockSlot,
}

/// Server → client on connect: the slot ↔ id table the server is using.
/// Client validates against its own registry; mismatched slot/id pairs
/// indicate a divergent mod set and the connection is rejected.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct BlockManifest {
    /// Slot index = position in this Vec. Slot 0 is always `vanilla:empty`.
    pub slots: Vec<BlockId>,
}

/// Server → client only: tells a client what to put in a chunk it just
/// entered AoI of. Two payload variants — see `ChunkData`. Subsequent
/// changes arrive as `BlockEdit` broadcasts; this message fires once
/// per (chunk, client) pair on AoI entry.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct ChunkSnapshot {
    pub coord: ChunkCoord,
    pub data: ChunkData,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ChunkData {
    /// The chunk has never been edited. The client generates it locally
    /// from the deterministic terrain function (`Chunk::from_terrain`).
    /// ~13 B on the wire.
    Procedural,
    /// The chunk has been edited; the client must use these blocks rather
    /// than regenerating. ~64 KB on the wire (RLE later).
    Edited(Vec<BlockSlot>),
}

/// Client → server: where this client's avatar is and which way they're
/// facing. Translation drives AoI; yaw drives the body orientation other
/// clients see. Sent at ≈10 Hz; server tick interpolates between updates.
/// Pitch isn't included — the visible avatar is a single block-shape with
/// no separate head, so head pitch buys nothing on the wire today.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PlayerPose {
    pub translation: Vec3,
    /// Body yaw in radians (rotation around +Y).
    pub yaw: f32,
}

/// Marker component on the server-side player-avatar entity. Replicated to
/// every *other* client so they get a visible body for that player. Paired
/// with `AvatarPose`, which carries the per-tick state.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Avatar;

/// Authoritative pose of an `Avatar`, written on the server from `PlayerPose`
/// ingests and replicated out as state. Sixteen bytes (Vec3 + yaw f32) vs
/// the forty a full `Transform` would cost — rotation+scale of the full
/// transform are dead weight when all we render is a yaw-rotated cuboid.
/// Quantize to i16/u16 fixed-point if avatar bandwidth ever shows up in
/// profiles; the precision needed (~cm of position, ~tenth of a degree of
/// yaw) fits comfortably.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AvatarPose {
    pub translation: Vec3,
    pub yaw: f32,
}

/// Server → client: this chunk has left your AoI; despawn your local copy.
/// The server may still hold its data (we don't evict the master record yet),
/// but you don't need it anymore.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ChunkUnload {
    pub coord: ChunkCoord,
}

/// Channel marker. One ordered-reliable channel for all world events
/// (BlockEdit, ChunkSnapshot, building events…). Future work may split
/// priorities; for now KISS.
pub struct WorldChannel;

/// Game-wide schedule ordering. Plugins assign their systems to one of these
/// sets so input → simulation → re-mesh runs in one frame in the right order,
/// even across plugin boundaries.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum GameSet {
    Input,
    Simulation,
    PostSimulation,
}
