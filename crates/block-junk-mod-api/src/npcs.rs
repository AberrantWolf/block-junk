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

use crate::blocks::NeedRestore;
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

/// Engine-wide fallback for the work-action pipeline (player-tagged
/// build/remove plans). Mods set this via
/// `engine.npcs.set_work_defaults({ need_restore = {...}, duration_secs = ... })`
/// in `data.lua`; the engine consults it whenever the targeted block's
/// own [`BlockDef::work_action`](crate::blocks::BlockDef::work_action)
/// is `None`.
///
/// `need_restore` is optional — leaving it unset means a work completion
/// only mutates world state and frees the plan claim, with no need
/// delta. `duration_secs` defaults to 4.0 (an NPC visibly stands at
/// the target for a few seconds).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkDefaults {
    #[serde(default)]
    pub need_restore: Option<NeedRestore>,
    #[serde(default = "default_work_duration_secs")]
    pub duration_secs: f32,
}

impl Default for WorkDefaults {
    fn default() -> Self {
        Self {
            need_restore: None,
            duration_secs: default_work_duration_secs(),
        }
    }
}

fn default_work_duration_secs() -> f32 {
    4.0
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
    /// Maximum stack size this kind can carry. Hauling NPCs (Phase 4)
    /// read this when the assignment scheduler picks them up for a
    /// build with multi-unit material costs — a higher cap means
    /// fewer round-trips. Defaults to 3 (vanilla villager baseline)
    /// so existing mod data registers unchanged.
    #[serde(default = "default_carry_capacity")]
    pub carry_capacity: u32,
    /// Default animation clips for this NPC kind. Required so a
    /// freshly-spawned NPC has *something* to render — the client's
    /// drive-animation system uses these as the velocity-hysteresis
    /// fallback (`idle` when stationary, `walk` when moving) and as
    /// the goal-driven defaults (`work` while pursuing a player
    /// plan). Each value is an [`AnimationId`](crate::animations::AnimationId)
    /// string registered via `engine.animations.register`; the
    /// engine validates that all three resolve at boot.
    ///
    /// Use-slot interactions override these by setting
    /// [`UseSlot.animation`](crate::blocks::UseSlot::animation).
    pub animations: NpcKindAnimations,
}

/// Per-kind default animation set. All three slots are mandatory —
/// every NPC kind needs *some* clip to play in the three core states
/// (stationary, moving, working a plan). When more states show up
/// (jumping, falling, dying) they get their own slots here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NpcKindAnimations {
    pub idle: String,
    pub walk: String,
    pub work: String,
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
    /// Up to K nearest reachable interactable blocks, sorted by
    /// distance (closest first). One entry per
    /// [`Interactable`](crate::blocks::Interactable)-bearing cell in
    /// the NPC's neighbourhood, regardless of *which* action the
    /// interactable represents (eat, sleep, enchant, sit). Each
    /// entry exposes the optional `need` + `restores` the engine
    /// pre-computed from the block def + the `exclusive` flag, so
    /// planners pick targets by matching the need they want to
    /// reduce — "this NPC's `hunger` is high, pick the nearest
    /// entry whose `need == hunger` and whose `restores` covers
    /// it" — without needing parallel block-def lookups on the
    /// Lua side. **Already filtered**: exclusive entries currently
    /// claimed by a different NPC are excluded. Race-on-claim is
    /// still possible if two planners tick the same instant; the
    /// brain's atomic `try_claim` resolves that at goal commit.
    ///
    /// Empty when no interactables exist in range (early game, or
    /// the player hasn't placed any yet). The engine snapshot
    /// builder bounds the scan radius, so a basket on the other
    /// side of the map won't show up; the planner makes a "what's
    /// nearby right now" decision, not a global search.
    #[serde(default)]
    pub nearby_interactions: Vec<NearbyInteraction>,
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
    /// Engine-assigned haul work this NPC is already committed to.
    /// Populated by the haul scheduler before the planner runs;
    /// planners see these for *visibility* only — they cannot
    /// claim, alter, or hand back an assignment. When this field is
    /// non-empty the engine bypasses the planner entirely for the
    /// tick and drives the NPC through the haul cycle directly, so
    /// any [`PlannerGoal`] a planner *does* return for an assigned
    /// NPC is silently ignored. Future planner versions can use this
    /// to score "should I do my own thing on top of hauling?" decisions
    /// (today the answer is "you can't").
    #[serde(default)]
    pub pending_assignments: Vec<PendingAssignment>,
}

/// One entry in [`NpcSnapshot::pending_assignments`]. Coarse view of
/// what the engine has assigned this NPC — enough for a planner to
/// understand "you're hauling to that build over there" without
/// leaking item-slot internals or reservation-table internals.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingAssignment {
    /// The Build plan cell this haul is delivering to.
    pub plan_cell: BlockPos,
    /// How many items remain in the NPC's haul queue (not counting
    /// anything already in carry). Drops to 0 on the final pickup
    /// leg; the NPC then walks to `plan_cell` to deposit.
    pub items_remaining: u32,
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
    /// Need id this plan reduces on completion (e.g. `"work"`).
    /// `None` ⇒ neither the targeted block's
    /// [`BlockDef::work_action`](crate::blocks::BlockDef::work_action) nor
    /// the engine-wide [`WorkDefaults`] declare a need restore for this
    /// plan — completing it only mutates world state. Lets planners
    /// score plans by which need they'd satisfy ("my villager's `work`
    /// is high, take the nearest plan whose `need == work`").
    #[serde(default)]
    pub need: Option<String>,
    /// Pre-clamp magnitude of the deficit reduction. Meaningful only
    /// when `need.is_some()`; `0.0` when the plan has no need effect.
    /// Mirrors how
    /// [`NearbyInteraction::restores`](crate::npcs::NearbyInteraction::restores)
    /// surfaces the same value for interactables.
    #[serde(default)]
    pub restores: f32,
    /// Manhattan distance from the NPC's foot, same metric as
    /// `nearby_interactions.distance`.
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

/// One entry in [`NpcSnapshot::nearby_interactions`]. Pre-computed
/// `need` + `restores` mirror the originating block's
/// [`Interactable::need_restore`](crate::blocks::Interactable::need_restore)
/// so the planner doesn't need a parallel block-def lookup table on
/// the Lua side. The engine re-resolves the block on arrival, so if
/// the block was changed or removed between snapshot and arrival,
/// the NPC just completes the goal with no effect (and no need
/// change).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NearbyInteraction {
    /// World cell of the interactable block. May be any cell of a
    /// multi-cell block; the engine resolves the anchor at goal
    /// commit time. Planners hand this back as
    /// [`PlannerGoal::Interact::cell`](crate::npcs::PlannerGoal::Interact).
    pub cell: BlockPos,
    /// Need id this interaction reduces (e.g. `"hunger"`, `"sleep"`).
    /// `None` ⇒ purely positional interaction (sit in a chair, no
    /// need change). When present, mirrors `need_restore.need` from
    /// the block's [`Interactable`](crate::blocks::Interactable) def.
    pub need: Option<String>,
    /// Pre-clamp magnitude of the deficit reduction. Meaningful
    /// only when `need.is_some()`; `0.0` when the interaction is
    /// purely positional.
    pub restores: f32,
    /// `true` ⇒ the block enforces single-user exclusivity (bed,
    /// altar). Planners use this to score "is anyone else going
    /// to take it before I get there"; non-exclusive entries
    /// (water well, food shelf) can be ignored when the planner
    /// just wants any-anyone-eat-here.
    pub exclusive: bool,
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
    /// Walk to an interactable block, optionally claim it (if its
    /// def declares `exclusive`), snap onto its [`UseSlot`] (if it
    /// declares one), wait the block's `duration_secs`, then apply
    /// the optional `need_restore` and release any held claim. One
    /// primitive covers every action variant the engine knows how
    /// to drive: eat at a basket, sleep in a bed, enchant at an
    /// altar, sit in a chair. The block def — not this enum — is
    /// what makes the action distinct.
    ///
    /// `cell` may be any cell of a multi-cell interactable (foot or
    /// head of a bed); the engine resolves the anchor via the chunk
    /// sidecar so two planners that picked different ends of the
    /// same block contend for the same claim.
    ///
    /// If by the time the NPC arrives the block has been broken,
    /// replaced with a non-interactable, or claimed by someone
    /// else (for an exclusive interactable), the goal completes
    /// silently — no effect, no error. The planner is expected to
    /// read [`NpcSnapshot::nearby_interactions`] to discover
    /// available targets.
    ///
    /// Replaces the previous `Consume` + `Sleep` variants — the
    /// distinction now lives entirely in the block def's
    /// `Interactable::exclusive` flag and `duration_secs` value.
    Interact {
        cell: BlockPos,
        #[serde(default = "default_interact_timeout")]
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

fn default_carry_capacity() -> u32 {
    3
}

fn default_wander_timeout() -> f32 {
    12.0
}

fn default_goto_timeout() -> f32 {
    30.0
}

fn default_interact_timeout() -> f32 {
    // Covers both quick eats and full sleeps. Upper end is generous
    // because exclusive interactables (beds, altars) can run for
    // tens of seconds before completing; the brain's per-action
    // clamp catches misbehaving mods.
    60.0
}

fn default_work_timeout() -> f32 {
    60.0
}
