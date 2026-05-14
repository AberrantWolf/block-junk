//! NPC kind + need registry types.
//!
//! Mods declare what kinds of NPCs exist (`NpcKindDef`) and what needs
//! drive them (`NeedDef`). At runtime the engine periodically calls a
//! mod-registered planner with an [`NpcSnapshot`] and uses the returned
//! [`PlannerGoal`] to drive that NPC for the next few seconds. Adding a
//! new `PlannerGoal` variant requires engine support — mods cannot teach
//! the engine new primitive actions through the registry alone.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::shared::BlockPos;

/// Stable string identifier for a need, "namespace:name" by convention.
/// Per the design memo, needs are registered in the `vanilla` mod, not
/// the engine — the engine doesn't know what "hunger" means, only that
/// some need decays at a given rate and the planner reads its value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NeedId(pub String);

impl NeedId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for NeedId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for NeedId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl core::fmt::Display for NeedId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable string identifier for an NPC kind, "namespace:name" by
/// convention. The kind selects which planner the engine invokes and
/// which need table is initialised on spawn.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NpcKindId(pub String);

impl NpcKindId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for NpcKindId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for NpcKindId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl core::fmt::Display for NpcKindId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Registered need definition. The engine decays each need value by
/// `decay_per_sec` every fixed tick and clamps to `[0, 1]`. Today every
/// NPC carrying the matching need experiences the same decay rate;
/// per-personality multipliers come later with the Personality system.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NeedDef {
    pub id: NeedId,
    pub display_name: String,
    /// Decay applied each second (`Δ = decay_per_sec · dt`). 0 disables
    /// passive decay (the need only changes from explicit actions).
    pub decay_per_sec: f32,
}

/// Registered NPC kind. Mods construct these and pass them to
/// `engine.npcs.register`; the engine builds a parallel kind registry on
/// each side, agreeing on slot ordering for any future wire-friendly
/// `NpcKindSlot` type (today the wire still carries the full string).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NpcKindDef {
    pub id: NpcKindId,
    pub display_name: String,
    /// Initial value for each need this kind cares about. Keys are
    /// [`NeedId`] strings; values are clamped to `[0, 1]` on spawn. A
    /// kind that doesn't list a need simply doesn't carry it — the
    /// planner sees an absent entry, not a zero.
    #[serde(default)]
    pub default_needs: HashMap<String, f32>,
}

/// Live state handed to the planner. Today carries id, kind, position,
/// current need values, and a list of nearby matched rooms; future
/// fields (opinions, world clock, nearby actors) will be added behind
/// serde defaults so old planners keep working as the surface grows.
///
/// Built on the engine side at planner-call time, serialised once into
/// the target Lua state, and discarded. Planners read it as a Lua table
/// (e.g. `snapshot.needs.hunger`, `snapshot.nearby_rooms[1]`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NpcSnapshot {
    /// Stable per-NPC handle — same value across every call for the
    /// same NPC, distinct between NPCs. Planners use this to key any
    /// Lua-side state they want to carry between calls (last action,
    /// cooldown timers). Matches the engine's `NpcId(u64)` so the
    /// value also survives save/load.
    pub id: u64,
    pub kind: NpcKindId,
    /// The NPC's current foot cell — useful for "go home" style logic
    /// once landmarks exist. For now the planner mostly ignores this.
    pub foot: BlockPos,
    pub needs: HashMap<String, f32>,
    /// Up to K nearest detected rooms with a matched pattern, sorted by
    /// distance from `foot` (closest first). Empty when no matched
    /// rooms exist (early game, or the world detector hasn't run yet).
    /// Planners filter by `pattern` to pick a target — e.g. "head for
    /// the nearest `vanilla:small_house`."
    #[serde(default)]
    pub nearby_rooms: Vec<NearbyRoom>,
}

/// One entry in [`NpcSnapshot::nearby_rooms`]. Carries enough info to
/// decide whether the room is interesting and where to walk to. The
/// engine recomputes this fresh per planner call (the snapshot is
/// transient), so planners shouldn't cache anchors across calls — the
/// player may have demolished the floor in the meantime.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NearbyRoom {
    /// Engine-side stable id for the region. Plain u32, mirroring the
    /// engine's `RoomId` so the planner can compare equality across
    /// calls (same `id` ⇒ same room).
    pub id: u32,
    /// Deepest matching pattern id. A planner that wants "any enclosed
    /// space" today has to check the full set (`enclosed_space`,
    /// `walled_yard`, `small_house`); an `is_a` checker on the engine
    /// surface comes later.
    pub pattern: String,
    /// Floor cell to use as the "go to this room" target — chosen by
    /// the engine as whichever floor cell is closest to the room's
    /// geometric centroid, so it's guaranteed walkable.
    pub anchor: BlockPos,
    /// Manhattan distance (cells) from the NPC's foot to the anchor.
    /// Cheap to compute server-side and ranks correctly for "nearer is
    /// better." Planners that need euclidean can compute it from
    /// `foot` + `anchor` themselves.
    pub distance: u32,
}

/// What a planner returns. Mirrors the goals the engine knows how to
/// execute — a planner can't invent new primitives, only choose between
/// them. The native brain converts each variant into its internal
/// `Goal` (Wander picks an A* path inside the engine; Rest holds a
/// timer; Idle parks the brain until the next planner call).
///
/// Tagged-enum serialization (`{ kind = "wander", radius_cells = 12 }`)
/// matches how mods write goals in Lua. Adding a variant here means
/// teaching the engine a new action.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlannerGoal {
    /// Do nothing this slice. The engine will ask again on the next
    /// planner tick. Used by planners that decide they don't have a
    /// useful action right now (every need satisfied, no nearby
    /// attractions). Distinct from `Rest` — `Idle` says "ask me again
    /// soon"; `Rest` says "I have decided to wait this long."
    Idle,
    /// Pick a random reachable target within `radius_cells` of the
    /// NPC's current foot cell and walk there. The engine's pathfinder
    /// owns target selection and route planning; planners only get to
    /// shape the search radius and the abandon-timeout.
    Wander {
        #[serde(default = "default_wander_radius")]
        radius_cells: i32,
        #[serde(default = "default_wander_timeout")]
        timeout_secs: f32,
    },
    /// Stand still for this many seconds. The engine clamps to a
    /// sensible upper bound so a misbehaving planner can't park an NPC
    /// for an hour.
    Rest { duration_secs: f32 },
    /// Walk to a specific target cell. The engine runs A* from the
    /// NPC's current foot to `cell`; if no path exists within budget
    /// the NPC parks briefly (the same fallback Wander uses). Use this
    /// for "head to a room I saw in the snapshot" decisions; for
    /// "explore my surroundings" use [`PlannerGoal::Wander`] which
    /// picks a random reachable target itself.
    Goto {
        cell: BlockPos,
        #[serde(default = "default_goto_timeout")]
        timeout_secs: f32,
    },
}

fn default_wander_radius() -> i32 {
    12
}

fn default_wander_timeout() -> f32 {
    12.0
}

fn default_goto_timeout() -> f32 {
    30.0
}
