//! Room-state replication: matched rooms mirrored to clients.
//!
//! Detection stays entirely server-side (`rooms.rs`); this module ships
//! the *results* so the player actually sees them — before this, room
//! events reached Lua mods and the server log and nothing else, which
//! read in play as "detection doesn't work."
//!
//! Only bounded summaries of matched rooms cross the ordered spatial lane;
//! unmatched regions remain detector bookkeeping.

use crate::protocol::{GameSet, RoomSummary};
use crate::rooms::{RoomEventMsg, RoomMap, RoomSummaryMutation};
use bevy::prelude::*;
use block_junk_mod_api::rooms::RoomEvent;

pub struct RoomSummaryDataset;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RoomSummaryWire {
    pub room_id: u32,
    pub pattern: crate::protocol::BoundedString<{ crate::protocol::MAX_WIRE_ID_BYTES }>,
    pub anchor: IVec3,
    pub bbox_min: IVec3,
    pub bbox_max: IVec3,
    pub floor_area: u32,
}

impl crate::spatial::SpatialDataset for RoomSummaryDataset {
    type Key = u32;
    type Value = RoomSummary;
    type Wire = RoomSummaryWire;
    type Persistence = crate::spatial::EphemeralDataset;
    const ID: crate::spatial::DatasetId = crate::spatial::DatasetId(5);
    const SCHEMA_FINGERPRINT: u64 = 0x726f_6f6d_0000_0001;
    const MEMBERSHIP: crate::spatial::MembershipPolicy = crate::spatial::MembershipPolicy::Bounds;
    const REPLICATION: crate::spatial::ReplicationPolicy =
        crate::spatial::ReplicationPolicy::Immediate;
    const MAX_RECORD_BYTES: usize = 1024;
    fn chunks(_: &Self::Key, value: &Self::Value) -> Vec<crate::protocol::ChunkCoord> {
        let min = crate::voxel::world_to_chunk(value.bbox_min).0.0;
        let max = crate::voxel::world_to_chunk(value.bbox_max).0.0;
        let mut chunks = Vec::new();
        for x in min.x..=max.x {
            for y in min.y..=max.y {
                for z in min.z..=max.z {
                    chunks.push(crate::protocol::ChunkCoord(IVec3::new(x, y, z)));
                }
            }
        }
        chunks
    }
    fn to_wire(_: &Self::Key, value: &Self::Value) -> Self::Wire {
        RoomSummaryWire {
            room_id: value.room_id,
            pattern: crate::protocol::BoundedString::new(value.pattern.clone())
                .expect("room patterns originate in the bounded registry"),
            anchor: value.anchor,
            bbox_min: value.bbox_min,
            bbox_max: value.bbox_max,
            floor_area: value.floor_area,
        }
    }
    fn from_wire(
        wire: Self::Wire,
        _: &crate::spatial::SpatialDecodeRegistry,
    ) -> Result<(Self::Key, Self::Value), crate::spatial::SpatialError> {
        if wire.bbox_min.cmpgt(wire.bbox_max).any() {
            return Err(crate::spatial::SpatialError::Decode(
                "invalid room summary".into(),
            ));
        }
        let min = crate::voxel::world_to_chunk(wire.bbox_min).0.0;
        let max = crate::voxel::world_to_chunk(wire.bbox_max).0.0;
        let span = max - min + IVec3::ONE;
        if span.x > 64 || span.y > 64 || span.z > 64 {
            return Err(crate::spatial::SpatialError::Decode(
                "room spans too many chunks".into(),
            ));
        }
        let summary = RoomSummary {
            room_id: wire.room_id,
            pattern: wire.pattern.to_string(),
            anchor: wire.anchor,
            bbox_min: wire.bbox_min,
            bbox_max: wire.bbox_max,
            floor_area: wire.floor_area,
        };
        Ok((summary.room_id, summary))
    }
}

// ---------- server ----------

pub struct RoomSyncServerPlugin;

impl Plugin for RoomSyncServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::spatial::SpatialFeaturePlugin::<RoomSummaryDataset>::server());
        // The detector writes summary mutations during Simulation, so the
        // spatial store observes same-tick geometry and recognition changes.
        app.add_systems(Update, sync_room_summaries.in_set(GameSet::PostSimulation));
    }
}

/// Project the detector's detailed state into the bounded client summary
/// dataset. The generic spatial framework handles targeting and delivery.
fn sync_room_summaries(
    mut reader: MessageReader<RoomEventMsg>,
    mut mutations: MessageReader<RoomSummaryMutation>,
    rooms: Res<RoomMap>,
    mut summaries: ResMut<ClientRooms>,
) {
    for RoomSummaryMutation(room) in mutations.read() {
        if let Some(summary) = rooms.summary_of(*room) {
            summaries.upsert(summary.room_id, summary, 0);
        }
    }
    for RoomEventMsg(event) in reader.read() {
        match event {
            RoomEvent::Created { .. } | RoomEvent::Changed { .. } => {}
            RoomEvent::Destroyed { room } => {
                summaries.delete(&room.0, 0);
            }
        }
    }
}

// ---------- client ----------

/// Client mirror of the server's matched rooms. Read by the inspect
/// panel ("Room: Small house") and whatever debug/overlay wants bboxes.
pub type ClientRooms = crate::spatial::PartitionedStore<RoomSummaryDataset>;

impl ClientRooms {
    /// Smallest matched room whose bbox contains `cell` — smallest so a
    /// bedroom inside a walled compound reports the bedroom, not the
    /// compound.
    pub fn room_at(&self, cell: IVec3) -> Option<&RoomSummary> {
        self.entries()
            .map(|(_, room)| room)
            .filter(|r| cell.cmpge(r.bbox_min).all() && cell.cmple(r.bbox_max).all())
            .min_by_key(|r| {
                let d = r.bbox_max - r.bbox_min + IVec3::ONE;
                d.x as i64 * d.y as i64 * d.z as i64
            })
    }
}

pub struct RoomSyncClientPlugin;

impl Plugin for RoomSyncClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::spatial::SpatialFeaturePlugin::<RoomSummaryDataset>::client());
    }
}
