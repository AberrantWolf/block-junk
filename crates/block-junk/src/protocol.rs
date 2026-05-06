use bevy::prelude::*;
use serde::{Deserialize, Serialize};

pub const CHUNK_SIZE: u32 = 32;
pub const CHUNK_PADDED: u32 = CHUNK_SIZE + 2;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Block {
    Empty,
    Solid,
}

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
    pub block: Block,
}

/// Server → client only: full snapshot of one chunk's blocks. Sent once
/// when a client first comes into AoI of that chunk. Subsequent changes
/// arrive as `BlockEdit` broadcasts.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct ChunkSnapshot {
    pub coord: ChunkCoord,
    pub blocks: Vec<Block>,
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
