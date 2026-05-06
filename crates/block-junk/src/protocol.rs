use bevy::prelude::*;
use serde::{Deserialize, Serialize};

pub const CHUNK_SIZE: u32 = 32;
pub const CHUNK_PADDED: u32 = CHUNK_SIZE + 2;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Block {
    Empty,
    Solid,
}

/// Client → server intent: set a block in a chunk to a particular state.
/// Eventually this will be replicated by lightyear; today it's an in-process
/// message handed off between the client and server plugins.
#[derive(Message, Clone, Copy, Debug)]
pub struct BlockEdit {
    pub chunk: Entity,
    pub pos: IVec3,
    pub block: Block,
}

/// Game-wide schedule ordering. Plugins assign their systems to one of these
/// sets so that input → simulation → re-mesh runs in a single frame in the
/// correct order, even when the systems live in different plugins.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum GameSet {
    Input,
    Simulation,
    PostSimulation,
}
