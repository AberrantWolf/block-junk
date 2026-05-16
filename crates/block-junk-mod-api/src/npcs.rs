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
    /// Up to K nearest consumable cells reachable from `foot`, sorted
    /// by distance (closest first). Each entry says *which* need the
    /// block restores and by how much, so a planner that's hungry
    /// (or thirsty, or low on mana) can pick the nearest entry that
    /// addresses *its* deficit rather than walking to a fountain when
    /// what it needs is bread. "Consumable" is deliberately broad —
    /// food, drink, potions, scrolls, anything a mod declared with
    /// [`block_junk_mod_api::blocks::Consumable`] metadata.
    ///
    /// Empty when no consumable blocks exist in the NPC's neighbourhood
    /// (early game, or the player hasn't placed any yet). The engine
    /// snapshot builder bounds the scan radius, so a basket on the
    /// other side of the map won't show up; the planner is making a
    /// "what's nearby right now" decision, not a global search.
    #[serde(default)]
    pub nearby_consumables: Vec<NearbyConsumable>,
    /// Up to K nearest unclaimed sleeper blocks, sorted by distance
    /// (closest first). Beds today, but anything a mod has tagged
    /// with [`Sleeper`](crate::blocks::Sleeper) shows up here.
    /// **Already filtered**: a sleeper currently claimed by a
    /// different NPC is excluded from this list — the planner sees
    /// only what *it* could plausibly use this tick. Race-on-claim
    /// is still possible (two planners tick the same instant and
    /// both see the same free bed), but the brain's `try_claim`
    /// step resolves that atomically by failing the second arrival.
    #[serde(default)]
    pub nearby_sleepers: Vec<NearbySleeper>,
    /// Up to K nearest unclaimed player-tagged plan cells, sorted by
    /// distance (closest first). Each carries the verb (remove vs.
    /// build) so a planner can pick "the closest plan I'm willing to
    /// work on" — e.g. a villager kind that only builds, not demolishes,
    /// can filter by kind. **Already filtered**: claims held by a
    /// different NPC are excluded; race-on-claim still possible but
    /// resolved by the brain's atomic try_claim on goal commit.
    #[serde(default)]
    pub nearby_plans: Vec<NearbyPlan>,
    /// True when the world clock currently reads as night. Mirrors
    /// the engine's `WorldClock::is_night`. Lets a planner gate
    /// nocturnal-only actions (sleep, hunt) without having to read
    /// raw `time_of_day` and re-derive the threshold.
    #[serde(default)]
    pub is_night: bool,
}

/// One entry in [`NpcSnapshot::nearby_plans`]. Carries the cell and a
/// kind *hint* — not the full engine-side `PlanKind` (which would couple
/// the mod API to block-slot internals). Planners only need the verb to
/// decide whether to take the plan; the engine looks up the full
/// PlanKind from its server-side `Plans` resource at goal commit time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NearbyPlan {
    /// World cell of the plan. For Remove plans the cell is currently
    /// solid (the block to break); for Build plans the cell is empty
    /// (where the new block should land).
    pub cell: BlockPos,
    pub kind: PlanKindHint,
    /// Manhattan distance from the NPC's foot, same metric as
    /// `nearby_sleepers.distance` / `nearby_consumables.distance`.
    pub distance: u32,
}

/// Verb of a [`NearbyPlan`]. Cut down from the engine's full PlanKind:
/// the planner doesn't need the BlockSlot + orientation that a Build
/// plan carries — those are engine concerns resolved at completion.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanKindHint {
    Remove,
    Build,
}

/// One entry in [`NpcSnapshot::nearby_sleepers`]. Same shape as
/// [`NearbyConsumable`] but for sleepers; the engine pre-computes
/// `need` and `restores` from the block's
/// [`Sleeper`](crate::blocks::Sleeper) def so the planner doesn't
/// need to know which block-id is a bed today. The engine
/// re-resolves the block + claim state on arrival so a snapshot
/// that's gone stale (bed broken, bed claimed first by another NPC)
/// degrades to "completes silently" rather than producing wrong
/// behaviour.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NearbySleeper {
    /// World cell of the sleeper block. May be the foot or the head
    /// of a multi-cell bed; the engine resolves the anchor at goal
    /// commit time.
    pub cell: BlockPos,
    /// Need this sleep reduces. Mirrored from
    /// [`Sleeper::need`](crate::blocks::Sleeper).
    pub need: String,
    /// Pre-clamp magnitude of the deficit reduction.
    pub restores: f32,
    /// Manhattan distance from the NPC's foot, same metric used by
    /// `nearby_consumables.distance`.
    pub distance: u32,
}

/// One entry in [`NpcSnapshot::nearby_consumables`]. Pre-computed
/// `need` + `restores` mirror the originating block's
/// [`Consumable`](crate::blocks::Consumable) so the planner doesn't
/// need a parallel block-def lookup table on the Lua side. The engine
/// re-resolves the block on actual consumption, so if the block was
/// changed or removed between snapshot and arrival, the NPC just
/// completes the goal with no effect.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NearbyConsumable {
    /// World-cell of the consumable block. Planners hand this back as
    /// `PlannerGoal::Consume::cell`. Adjacency to a standable cell is
    /// the engine's problem — the planner just names the target.
    pub cell: BlockPos,
    /// Need id this consumption reduces (e.g. `"hunger"`). Mirrored
    /// from the block's [`Consumable::need`](crate::blocks::Consumable).
    pub need: String,
    /// Pre-clamp magnitude of the deficit reduction. A planner can
    /// compare against `snapshot.needs[need]` to score "this restores
    /// enough to be worth walking to."
    pub restores: f32,
    /// Manhattan distance from the NPC's foot, same metric used by
    /// `nearby_rooms.distance`. Cheap, ranks correctly for "nearer is
    /// better." Planners that need euclidean derive it from `foot` +
    /// `cell` themselves.
    pub distance: u32,
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
    /// Walk to a consumable block, stand still for its declared
    /// duration, then subtract its `restores` from the matching need.
    /// `cell` is the consumable block itself — the engine paths to a
    /// standable neighbour, since the block itself is solid. If by
    /// the time the NPC arrives the block has been broken or replaced
    /// with something non-consumable, the goal completes silently
    /// (no effect, no error).
    ///
    /// This is the *only* primitive that mutates need state; pairs
    /// with planner picks that read `snapshot.nearby_consumables`.
    Consume {
        cell: BlockPos,
        #[serde(default = "default_consume_timeout")]
        timeout_secs: f32,
    },
    /// Walk to a sleeper block (a bed today), claim it for the
    /// duration of the sleep, stand still for its declared
    /// `duration_secs`, then subtract its `restores` from the
    /// matching need and release the claim. Differs from `Consume`
    /// in two ways: the engine maintains a per-bed claim table so
    /// only one NPC sleeps in a given bed at a time, and the brain
    /// allows much longer durations (sleep is intended to feel like
    /// real seconds, not a brief pause).
    ///
    /// `cell` may be any cell of a multi-cell sleeper (foot or head
    /// of a bed); the engine resolves the anchor via the chunk
    /// sidecar so two planners that picked different ends of the
    /// same bed contend for the same claim.
    ///
    /// If the bed is gone, replaced, or claimed by someone else by
    /// the time the NPC arrives, the goal completes silently. The
    /// planner is expected to read `snapshot.nearby_sleepers` to
    /// see what's available.
    Sleep {
        cell: BlockPos,
        #[serde(default = "default_sleep_timeout")]
        timeout_secs: f32,
    },
    /// Walk to a player-tagged plan cell, claim it, work for a fixed
    /// duration, then apply the underlying world mutation (break the
    /// block for a Remove tag, place the recorded block for a Build
    /// tag), clear the tag, release the claim, and subtract from the
    /// NPC's `work` need.
    ///
    /// `cell` is the plan target as it appears in `nearby_plans`.
    /// If by arrival the plan has been cancelled or the world state
    /// no longer matches the plan's intent (the block has been broken
    /// by something else for a Remove plan, or filled for a Build
    /// plan), the goal completes silently — same degradation pattern
    /// as `Consume` and `Sleep`.
    WorkPlan {
        cell: BlockPos,
        #[serde(default = "default_work_timeout")]
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

fn default_consume_timeout() -> f32 {
    30.0
}

fn default_sleep_timeout() -> f32 {
    60.0
}

fn default_work_timeout() -> f32 {
    60.0
}
