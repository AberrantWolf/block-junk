//! Civilization clustering parameters. Mods tune these via
//! `engine.civilization.set_params({ max_room_distance_cells = N, buffer_cells = M })`
//! in `data.lua`; the engine reads them when joining a new matched room
//! into a cluster and when inflating the cluster's bounding box.
//!
//! A "civilization cluster" is a connected component of matched rooms
//! whose bounding boxes lie within `max_room_distance_cells` of each
//! other (Chebyshev). NPCs claim a cluster as their home (today by
//! sleeping there) and only wander inside the cluster's inflated bbox
//! — keeps villagers from drifting into the wilderness across an
//! arbitrary A* horizon.

use serde::{Deserialize, Serialize};

/// Knobs for how the engine groups buildings into civilizations.
/// Both are in world cells.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct CivilizationParams {
    /// Two matched rooms join the same cluster when the Chebyshev
    /// distance between their bounding boxes is `<= max_room_distance_cells`.
    /// A first-hit-wins join is run when a new matched room appears: scan
    /// existing clusters in order, take the first whose bbox sits within
    /// this distance of the new room. No merging happens if the new room
    /// would bridge two clusters — first hit wins.
    #[serde(default = "default_max_room_distance_cells")]
    pub max_room_distance_cells: i32,
    /// Inflation applied to each cluster's bounding box when NPCs sample
    /// wander targets inside it. Bigger ⇒ NPCs wander further outside
    /// the building footprint; smaller ⇒ they stay closer to walls.
    #[serde(default = "default_buffer_cells")]
    pub buffer_cells: i32,
}

impl Default for CivilizationParams {
    fn default() -> Self {
        Self {
            max_room_distance_cells: default_max_room_distance_cells(),
            buffer_cells: default_buffer_cells(),
        }
    }
}

fn default_max_room_distance_cells() -> i32 {
    32
}

fn default_buffer_cells() -> i32 {
    8
}
