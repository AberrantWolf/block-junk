//! Civilization clusters — connected groups of matched rooms NPCs treat
//! as a single "village" for the purpose of bounded wander.
//!
//! Lifecycle is driven entirely by [`RoomEventMsg`] from
//! [`crate::rooms::process_dirty`]:
//!
//! - A room newly matches a pattern (`Created` with `Some(pattern)`, or
//!   `Changed` from `None` to `Some(_)`). Scan nearby clusters: any
//!   cluster owning a room whose bbox sits within
//!   [`CivilizationParams::max_room_distance_cells`] (Chebyshev) of the
//!   new room joins it. **First hit wins** — no merging. If no nearby
//!   cluster, the room seeds a new one.
//! - A room stops matching (`Destroyed`, or `Changed` to `None`). Remove
//!   it from its cluster. If the cluster is now empty, prune it and any
//!   NPC claims pointing at it lazy-clear on next read (the brain
//!   handles the dangling-id case directly).
//! - A room's match changes between two patterns (`Changed` Some→Some).
//!   Recompute its cluster's bbox from current member bboxes — the
//!   member list doesn't change but the room's own bbox may have moved.
//!
//! Bboxes stored on each cluster are *unflated* — the inflation by
//! [`CivilizationParams::buffer_cells`] happens at read time
//! ([`Civilization::cluster_bbox_inflated`]) so a runtime tune of the
//! buffer is observable without rebuilding clusters.

use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use block_junk_mod_api::civilization::CivilizationParams;
use block_junk_mod_api::rooms::{RoomEvent, RoomId};
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};

use crate::protocol::GameSet;
use crate::rooms::{RoomEventMsg, RoomMap, process_dirty};

/// Replicated cluster bounding box. One entity per live cluster spawned
/// by the server; the client's gizmo overlay reads `Query<&ClusterBboxReplica>`
/// to draw the wander box. `min`/`max` are *already inflated* by
/// `CivilizationParams::buffer_cells` so the client doesn't need to know
/// the buffer.
#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClusterBboxReplica {
    pub min: IVec3,
    pub max: IVec3,
}

/// Stable opaque cluster handle. Allocated by [`Civilization`] from a
/// monotonic counter; never reused inside one session. Brains stamp
/// this on `home_cluster` after sleeping in the cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClusterId(pub u32);

#[derive(Debug)]
struct Cluster {
    /// Inclusive AABB of the cluster's *union of room bboxes*. Refreshed
    /// whenever a member is added, removed, or changes shape.
    bbox_min: IVec3,
    bbox_max: IVec3,
    /// Members of the cluster. Used to recompute the bbox after a
    /// member leaves and for the neighbour scan when a new room asks
    /// "is anyone in this cluster close to me?".
    rooms: HashSet<RoomId>,
}

/// Resource holding the live cluster map plus the reverse room→cluster
/// index. Read by the NPC brain at wander-target selection.
#[derive(Resource, Default)]
pub struct Civilization {
    clusters: HashMap<ClusterId, Cluster>,
    room_to_cluster: HashMap<RoomId, ClusterId>,
    next_id: u32,
    /// One Bevy entity per live cluster, carrying a replicated
    /// [`ClusterBboxReplica`] so the client overlay can draw the wander
    /// box. Maintained by [`sync_cluster_entities`] each tick.
    cluster_entities: HashMap<ClusterId, Entity>,
}

impl Civilization {
    fn alloc(&mut self) -> ClusterId {
        let id = ClusterId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Inflated AABB of the cluster's bbox, or `None` if the id is no
    /// longer live (the brain lazy-clears its home_cluster in that
    /// case). Inflation applied at read time so a hot-tuned buffer
    /// shows up the next tick without rebuilding.
    pub fn cluster_bbox_inflated(
        &self,
        id: ClusterId,
        buffer_cells: i32,
    ) -> Option<(IVec3, IVec3)> {
        self.clusters.get(&id).map(|c| {
            let pad = IVec3::splat(buffer_cells.max(0));
            (c.bbox_min - pad, c.bbox_max + pad)
        })
    }

    /// Cluster that owns the room whose bbox contains `cell`. First hit
    /// wins on overlap (rare; floor-fill rooms shouldn't have
    /// overlapping bboxes in practice). Used by the brain when an NPC
    /// finishes sleeping at an interactable cell — the cell isn't a
    /// floor cell so `RoomMap::cell_to_room` won't resolve it, but the
    /// bed sits inside its room's volumetric bbox.
    pub fn cluster_containing_cell(&self, cell: IVec3, rooms: &RoomMap) -> Option<ClusterId> {
        for (room_id, bmin, bmax) in rooms.iter_matched_with_bbox() {
            if bbox_contains(bmin, bmax, cell)
                && let Some(&cluster_id) = self.room_to_cluster.get(&room_id)
            {
                return Some(cluster_id);
            }
        }
        None
    }

    /// `true` if the cluster id still exists. Brains use this to lazy-
    /// clear a `home_cluster` that's been pruned out from under them.
    pub fn is_live(&self, id: ClusterId) -> bool {
        self.clusters.contains_key(&id)
    }

    fn refresh_cluster_bbox(&mut self, cluster_id: ClusterId, rooms: &RoomMap) {
        let Some(cluster) = self.clusters.get_mut(&cluster_id) else {
            return;
        };
        if cluster.rooms.is_empty() {
            return;
        }
        let mut iter = cluster.rooms.iter().filter_map(|&r| rooms.room_bbox(r));
        let Some((mut bmin, mut bmax)) = iter.next() else {
            return;
        };
        for (mn, mx) in iter {
            bmin = bmin.min(mn);
            bmax = bmax.max(mx);
        }
        cluster.bbox_min = bmin;
        cluster.bbox_max = bmax;
    }

    fn add_matched_room(
        &mut self,
        room_id: RoomId,
        rooms: &RoomMap,
        params: &CivilizationParams,
    ) {
        if self.room_to_cluster.contains_key(&room_id) {
            // Already clustered — caller hit a Changed Some→Some;
            // refresh the cluster bbox in case the room's own bbox
            // moved.
            let cluster_id = self.room_to_cluster[&room_id];
            self.refresh_cluster_bbox(cluster_id, rooms);
            return;
        }
        let Some((new_min, new_max)) = rooms.room_bbox(room_id) else {
            return;
        };
        let max_dist = params.max_room_distance_cells.max(0);

        // First-hit-wins scan. For each existing cluster, check whether
        // any of its rooms sits within max_dist (Chebyshev, bbox-to-
        // bbox) of the new room. Cluster pre-filter on bbox is the
        // outer guard so we skip far-away clusters cheaply; the inner
        // loop is a tighter per-room check (a cluster's bbox can
        // include empty space the new room is "near" without actually
        // being close to any member).
        let mut joined: Option<ClusterId> = None;
        for (&cluster_id, cluster) in self.clusters.iter() {
            if !bbox_within(
                cluster.bbox_min,
                cluster.bbox_max,
                new_min,
                new_max,
                max_dist,
            ) {
                continue;
            }
            let hit = cluster.rooms.iter().any(|&rid| {
                rooms
                    .room_bbox(rid)
                    .map(|(mn, mx)| bbox_within(mn, mx, new_min, new_max, max_dist))
                    .unwrap_or(false)
            });
            if hit {
                joined = Some(cluster_id);
                break;
            }
        }

        let cluster_id = joined.unwrap_or_else(|| {
            let id = self.alloc();
            self.clusters.insert(
                id,
                Cluster {
                    bbox_min: new_min,
                    bbox_max: new_max,
                    rooms: HashSet::new(),
                },
            );
            id
        });
        let cluster = self.clusters.get_mut(&cluster_id).expect("just inserted");
        cluster.rooms.insert(room_id);
        cluster.bbox_min = cluster.bbox_min.min(new_min);
        cluster.bbox_max = cluster.bbox_max.max(new_max);
        self.room_to_cluster.insert(room_id, cluster_id);
    }

    fn remove_room(&mut self, room_id: RoomId, rooms: &RoomMap) {
        let Some(cluster_id) = self.room_to_cluster.remove(&room_id) else {
            return;
        };
        let prune = {
            let Some(cluster) = self.clusters.get_mut(&cluster_id) else {
                return;
            };
            cluster.rooms.remove(&room_id);
            cluster.rooms.is_empty()
        };
        if prune {
            self.clusters.remove(&cluster_id);
        } else {
            self.refresh_cluster_bbox(cluster_id, rooms);
        }
    }
}

/// Chebyshev distance between two AABBs `<= max_dist`. Overlap reads as
/// distance 0.
fn bbox_within(
    a_min: IVec3,
    a_max: IVec3,
    b_min: IVec3,
    b_max: IVec3,
    max_dist: i32,
) -> bool {
    let dx = (a_min.x - b_max.x).max(b_min.x - a_max.x).max(0);
    let dy = (a_min.y - b_max.y).max(b_min.y - a_max.y).max(0);
    let dz = (a_min.z - b_max.z).max(b_min.z - a_max.z).max(0);
    dx.max(dy).max(dz) <= max_dist
}

fn bbox_contains(min: IVec3, max: IVec3, p: IVec3) -> bool {
    p.x >= min.x
        && p.x <= max.x
        && p.y >= min.y
        && p.y <= max.y
        && p.z >= min.z
        && p.z <= max.z
}

/// Resource wrapper around the Lua-supplied params. Default values fall
/// back when no mod called `engine.civilization.set_params`.
#[derive(Resource, Clone, Copy, Debug)]
pub struct CivilizationParamsRes(pub CivilizationParams);

impl Default for CivilizationParamsRes {
    fn default() -> Self {
        Self(CivilizationParams::default())
    }
}

pub struct CivilizationPlugin;

impl Plugin for CivilizationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Civilization>();
        app.init_resource::<CivilizationParamsRes>();
        // Runs after process_dirty so the RoomMap state we read matches
        // the events being delivered this tick. Entity sync chains
        // directly after — it consumes the same Civilization state and
        // doesn't need to wait for anything else.
        app.add_systems(
            Update,
            (update_civilization, sync_cluster_entities)
                .chain()
                .after(process_dirty)
                .in_set(GameSet::Simulation),
        );
    }
}

/// Maintain one Bevy entity per live cluster with a replicated
/// [`ClusterBboxReplica`] component. Idempotent — spawns entities for
/// new clusters, updates `ClusterBboxReplica` for existing ones, and
/// despawns entities of pruned clusters. Runs after
/// [`update_civilization`] every tick (cheap: hashmap walk and a
/// `set_if_neq` per cluster).
fn sync_cluster_entities(
    mut civilization: ResMut<Civilization>,
    params: Res<CivilizationParamsRes>,
    mut bboxes: Query<&mut ClusterBboxReplica>,
    mut commands: Commands,
) {
    let buffer = params.0.buffer_cells.max(0);
    // Despawn entities whose cluster is gone. Drain into a Vec first to
    // avoid mutating cluster_entities while iterating it.
    let stale: Vec<ClusterId> = civilization
        .cluster_entities
        .keys()
        .copied()
        .filter(|id| !civilization.clusters.contains_key(id))
        .collect();
    for id in stale {
        if let Some(entity) = civilization.cluster_entities.remove(&id) {
            commands.entity(entity).despawn();
        }
    }
    // Borrow-split: pull cluster_entities out so we can iterate clusters
    // and mutate cluster_entities in the same loop.
    let Civilization {
        clusters,
        cluster_entities,
        ..
    } = &mut *civilization;
    for (cluster_id, cluster) in clusters.iter() {
        let pad = IVec3::splat(buffer);
        let next = ClusterBboxReplica {
            min: cluster.bbox_min - pad,
            max: cluster.bbox_max + pad,
        };
        match cluster_entities.get(cluster_id) {
            Some(&entity) => {
                if let Ok(mut bbox) = bboxes.get_mut(entity) {
                    bbox.set_if_neq(next);
                }
            }
            None => {
                let entity = commands
                    .spawn((
                        next,
                        Replicate::to_clients(NetworkTarget::All),
                        Name::new(format!("ClusterBbox({})", cluster_id.0)),
                    ))
                    .id();
                cluster_entities.insert(*cluster_id, entity);
            }
        }
    }
}

fn update_civilization(
    mut reader: MessageReader<RoomEventMsg>,
    rooms: Res<RoomMap>,
    params: Res<CivilizationParamsRes>,
    mut civilization: ResMut<Civilization>,
) {
    for msg in reader.read() {
        match &msg.0 {
            RoomEvent::Created { room, pattern, .. } => {
                if pattern.is_some() {
                    civilization.add_matched_room(*room, &rooms, &params.0);
                }
            }
            RoomEvent::Changed { room, from, to, .. } => {
                match (from.is_some(), to.is_some()) {
                    (false, true) => civilization.add_matched_room(*room, &rooms, &params.0),
                    (true, false) => civilization.remove_room(*room, &rooms),
                    // Pattern swap within "still matched" — refresh bbox.
                    (true, true) => {
                        if let Some(&cid) = civilization.room_to_cluster.get(room) {
                            civilization.refresh_cluster_bbox(cid, &rooms);
                        } else {
                            // Defensive: we missed a prior Created
                            // (shouldn't happen, but if it does, treat
                            // this as the first time we see it).
                            civilization.add_matched_room(*room, &rooms, &params.0);
                        }
                    }
                    (false, false) => {}
                }
            }
            RoomEvent::Destroyed { room } => {
                civilization.remove_room(*room, &rooms);
            }
        }
    }
}
