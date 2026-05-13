//! NPCs — server-authoritative actors driven by a needs-based brain.
//!
//! Starting slice (validates the puppeteer model before adding the Lua
//! planner surface): one trivial NPC kind, one need (hunger), one goal
//! (wander toward a random nearby target). The brain writes a
//! `MovementIntent` each tick and the same `apply_walk_step` players use
//! consumes it — single physics path for any actor.
//!
//! Not yet present: pathfinding (target-blocked NPCs just time out and
//! pick a new target), the Lua planner (the planner here is hardcoded
//! Rust), error isolation, save/load. See the design memo's future-work
//! backlog.

use bevy::prelude::*;
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::blocks::BlockRegistry;
use crate::collision::WorldCollision;
use crate::physics::apply_walk_step;
use crate::protocol::{
    Actor, AvatarOnGround, AvatarPose, AvatarVelocity, MovementIntent, MovementMode,
};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap};

/// Replicated marker — "this entity is an NPC, not a player avatar."
/// Lets clients render it differently and lets server systems narrow
/// queries to AI-controlled actors. Sibling to [`crate::protocol::Avatar`];
/// both ride alongside the shared [`Actor`] marker.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Npc;

/// Stable identifier for an NPC across save/load. Distinct from Bevy
/// `Entity` because Entity values aren't preserved across reboots.
/// Server-only today; allocated from a monotonic counter.
#[derive(Component, Clone, Copy, Debug)]
#[allow(dead_code, reason = "consumed by save/load layer (future work)")]
pub struct NpcId(pub u64);

/// Mod-namespaced kind, e.g. `vanilla:wanderer`. Determines which need
/// table and planner the brain runs. Currently inert (only one kind
/// exists and the brain is hardcoded); becomes load-bearing when the
/// Lua planner surface lands.
#[derive(Component, Clone, Debug)]
#[allow(dead_code, reason = "selects the brain/need table once Lua planners exist")]
pub struct NpcKind(pub String);

/// Floating-point need state. 0.0 = fully satisfied, 1.0 = critical.
/// Goal selection (future) will score actions by deficit reduction.
/// Starting slice has only hunger; this becomes a registry-backed map
/// (per the design memo, "needs registered in `vanilla` mod, not engine")
/// once the mod-API surface for needs lands.
#[derive(Component, Clone, Debug, Default)]
pub struct Needs {
    pub hunger: f32,
}

/// Per-second decay applied to each need. 1/300 ⇒ ~5 minutes of game
/// time from 0 to critical. Slow on purpose: until food + sleep systems
/// exist, an NPC visibly starving would just be confusing.
const HUNGER_DECAY_PER_SEC: f32 = 1.0 / 300.0;

/// Current goal the brain is executing. Only `Wander` exists in the
/// starting slice — once the planner has more to chew on this becomes
/// a richer plan tree.
#[derive(Clone, Debug)]
pub enum Goal {
    /// No active goal — brain picks one next tick. Newly-spawned NPCs
    /// start here.
    Idle,
    /// Walk toward `target` in world space. Completes when the NPC is
    /// within `WANDER_ARRIVE_RADIUS` of the target horizontally OR when
    /// `deadline_secs` has decayed to zero (the brain falls back to a
    /// fresh wander when the path is blocked — pathfinding is future
    /// work, so today the only "blocked" recovery is the timeout).
    Wander { target: Vec3, deadline_secs: f32 },
}

/// Native-side brain state. Holds the current goal + a tiny PRNG seed
/// for reproducible target selection.
#[derive(Component, Clone, Debug)]
pub struct Brain {
    pub goal: Goal,
    /// splitmix-seeded PRNG state. Per-NPC so two NPCs spawned the same
    /// tick don't pick identical wander targets.
    pub rng: u64,
}

const WANDER_RADIUS: f32 = 12.0;
const WANDER_TIMEOUT_SECS: f32 = 8.0;
const WANDER_ARRIVE_RADIUS: f32 = 1.5;

/// Maximum yaw rotation per tick for NPC steering, radians/sec. ~344°/s
/// — fast enough that the body doesn't lag visibly behind the chosen
/// direction, slow enough that you can see the turn.
const NPC_TURN_RATE: f32 = 6.0;

pub struct NpcServerPlugin;

impl Plugin for NpcServerPlugin {
    fn build(&self, app: &mut App) {
        // Spawn deferred to first client connect rather than Startup —
        // chunks aren't loaded until a client's AoI requests them, and
        // an NPC spawned into an empty world falls past unloaded chunks
        // forever (no candidates to collide against).
        app.add_observer(spawn_initial_npc_on_first_connect);
        // Brain → physics order matters: physics consumes the intent
        // the brain writes this tick. Both run in FixedUpdate alongside
        // the player physics so all actors advance together.
        app.add_systems(FixedUpdate, (npc_brain_tick, npc_physics_step).chain());
    }
}

/// Latches on the first `Connected` so subsequent reconnects don't
/// re-spawn the smoke-test NPC. `AtomicBool` rather than `Local<bool>`
/// because observers don't take `Local`.
static SMOKE_TEST_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Smoke-test spawn — one NPC near the player's default landing spot
/// (player spawn = (0, 32, 60)). 4 m offset puts them in view but out
/// of collision range. Replicated to all clients with interpolation
/// (no client predicts NPCs — there's no per-client "owner" of one).
fn spawn_initial_npc_on_first_connect(_: On<Add, Connected>, mut commands: Commands) {
    if SMOKE_TEST_SPAWNED.swap(true, Ordering::SeqCst) {
        return;
    }
    let id: u64 = 1;
    commands.spawn((
        Actor,
        Npc,
        NpcId(id),
        NpcKind("vanilla:wanderer".into()),
        Needs::default(),
        Brain {
            goal: Goal::Idle,
            rng: 0xDEAD_BEEF_CAFE_F00D ^ id,
        },
        AvatarPose {
            translation: Vec3::new(4.0, 32.0, 60.0),
            yaw: 0.0,
        },
        AvatarVelocity::default(),
        AvatarOnGround::default(),
        MovementMode::Walk,
        MovementIntent::default(),
        Replicate::to_clients(NetworkTarget::All),
        InterpolationTarget::to_clients(NetworkTarget::All),
        Name::new(format!("npc:{id}")),
    ));
    info!("spawned smoke-test NPC #{id}");
}

/// Per fixed-tick brain. Three phases:
///   1. Decay needs.
///   2. Goal lifecycle: tick down the deadline; when the current goal
///      completes (arrived OR timed out) reset to Idle.
///   3. Planner: if Idle, pick a fresh wander target. Then steer the
///      MovementIntent toward the active target.
///
/// Pure server-side. Clients only see the result via replicated
/// `AvatarPose`.
fn npc_brain_tick(
    time: Res<Time>,
    mut npcs: Query<(&AvatarPose, &mut Needs, &mut Brain, &mut MovementIntent), With<Npc>>,
) {
    let dt = time.delta_secs();
    for (pose, mut needs, mut brain, mut intent) in npcs.iter_mut() {
        needs.hunger = (needs.hunger + HUNGER_DECAY_PER_SEC * dt).min(1.0);

        // Goal completion check. Wander completes on horizontal
        // arrival OR deadline expiry — vertical drift (NPC standing on
        // a different elevation than the target) shouldn't trap them.
        let mut completed = false;
        if let Goal::Wander { target, deadline_secs } = &mut brain.goal {
            *deadline_secs -= dt;
            let dx = target.x - pose.translation.x;
            let dz = target.z - pose.translation.z;
            if (dx * dx + dz * dz).sqrt() < WANDER_ARRIVE_RADIUS || *deadline_secs <= 0.0 {
                completed = true;
            }
        }
        if matches!(brain.goal, Goal::Idle) || completed {
            let dx = (rand_unit(&mut brain.rng) * 2.0 - 1.0) * WANDER_RADIUS;
            let dz = (rand_unit(&mut brain.rng) * 2.0 - 1.0) * WANDER_RADIUS;
            brain.goal = Goal::Wander {
                target: Vec3::new(
                    pose.translation.x + dx,
                    pose.translation.y,
                    pose.translation.z + dz,
                ),
                deadline_secs: WANDER_TIMEOUT_SECS,
            };
        }

        // Steer the intent. The body's `forward` in apply_walk_step is
        // `(-sin(yaw), 0, -cos(yaw))`, so the yaw that points toward
        // (dx, 0, dz) is `atan2(-dx, -dz)`.
        let Goal::Wander { target, .. } = brain.goal else {
            *intent = MovementIntent::default();
            continue;
        };
        let dx = target.x - pose.translation.x;
        let dz = target.z - pose.translation.z;
        if dx * dx + dz * dz < f32::EPSILON {
            *intent = MovementIntent::default();
            continue;
        }
        let desired_yaw = (-dx).atan2(-dz);
        let mut delta = (desired_yaw - pose.yaw) % core::f32::consts::TAU;
        if delta > core::f32::consts::PI {
            delta -= core::f32::consts::TAU;
        } else if delta < -core::f32::consts::PI {
            delta += core::f32::consts::TAU;
        }
        let max_step = NPC_TURN_RATE * dt;
        let dyaw = delta.clamp(-max_step, max_step);
        // wishdir[2] = -1 means "forward" in the controller's coords
        // (Bevy yaw=0 → forward = -Z). NPCs only ever push forward;
        // turning happens through `dyaw`.
        *intent = MovementIntent {
            wishdir: [0, 0, -1],
            jump: false,
            toggle_mode: false,
            interact: false,
            dyaw,
        };
    }
}

/// Run the same physics controller players use, against the brain-
/// written `MovementIntent`. Mirrors `server_player_step` in server.rs
/// modulo the source of the intent — NPC brain vs replicated player
/// input.
fn npc_physics_step(
    time: Res<Time>,
    chunks: Query<(&'static Chunk, &'static ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    mut npcs: Query<
        (
            &mut AvatarPose,
            &mut AvatarVelocity,
            &mut AvatarOnGround,
            &mut MovementMode,
            &MovementIntent,
        ),
        With<Npc>,
    >,
) {
    let dt = time.delta_secs();
    let world = WorldCollision {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &registry,
    };
    for (mut pose, mut vel, mut on_ground, mut mode, intent) in npcs.iter_mut() {
        apply_walk_step(&mut pose, &mut vel, &mut on_ground, &mut mode, intent, dt, &world);
    }
}

/// splitmix64-style PRNG. Returns a uniform float in [0, 1). Quality
/// only has to fool human eyes scanning wander patterns.
fn rand_unit(state: &mut u64) -> f32 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    let bits = (z ^ (z >> 31)) as u32;
    bits as f32 / (u32::MAX as f32 + 1.0)
}
