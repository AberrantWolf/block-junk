//! Stable furnishing identities use exact footprint subscriptions on the shared lane.
use crate::{
    blocks::{BlockRegistry, BlockSlot},
    protocol::{BoundedVec, ChunkCoord, GameSet, MAX_BLOCK_FOOTPRINT_CELLS},
    spatial::{SpatialDataset, SpatialError},
};
use bevy::prelude::*;
use block_junk_mod_api::blocks::Cardinal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlacementView {
    pub id: u64,
    pub revision: u64,
    pub slot: BlockSlot,
    pub anchor: IVec3,
    pub orientation: Cardinal,
    pub portal_open: Option<u64>,
    pub footprint: BoundedVec<IVec3, MAX_BLOCK_FOOTPRINT_CELLS>,
}
pub struct PlacementDataset;
impl SpatialDataset for PlacementDataset {
    type Key = u64;
    type Value = PlacementView;
    type Wire = PlacementView;
    type Persistence = crate::spatial::EphemeralDataset;
    const ID: crate::spatial::DatasetId = crate::spatial::DatasetId(7);
    const SCHEMA_FINGERPRINT: u64 = 0x706c616300000001;
    const MEMBERSHIP: crate::spatial::MembershipPolicy =
        crate::spatial::MembershipPolicy::ExactCells;
    const REPLICATION: crate::spatial::ReplicationPolicy =
        crate::spatial::ReplicationPolicy::Immediate;
    const MAX_RECORD_BYTES: usize = 8192;
    fn revision(value: &PlacementView) -> Option<u64> {
        Some(value.revision)
    }
    fn chunks(_: &u64, value: &PlacementView) -> Vec<ChunkCoord> {
        value
            .footprint
            .iter()
            .map(|cell| crate::voxel::world_to_chunk(*cell).0)
            .collect()
    }
    fn to_wire(_: &u64, value: &PlacementView) -> PlacementView {
        value.clone()
    }
    fn from_wire(
        value: PlacementView,
        references: &crate::spatial::SpatialDecodeRegistry,
    ) -> Result<(u64, PlacementView), SpatialError> {
        references.require("block", value.slot.0 as u32)?;
        let cells: BTreeSet<_> = value.footprint.iter().map(|cell| cell.to_array()).collect();
        if value.id == 0
            || value.revision == 0
            || value.slot.is_empty()
            || cells.len() != value.footprint.len()
            || !cells.contains(&value.anchor.to_array())
            || cells.iter().flatten().any(|n| n.unsigned_abs() > 1_000_000)
        {
            return Err(SpatialError::Decode(
                "invalid placement identity or footprint".into(),
            ));
        }
        Ok((value.id, value))
    }
}
pub type ClientPlacements = crate::spatial::PartitionedStore<PlacementDataset>;
impl ClientPlacements {
    pub fn at(&self, cell: IVec3) -> Option<&PlacementView> {
        self.keys_in_chunk(crate::voxel::world_to_chunk(cell).0)
            .filter_map(|id| self.lookup(id))
            .find(|placement| placement.footprint.contains(&cell))
    }
}
pub struct PlacementServerPlugin;
impl Plugin for PlacementServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::spatial::SpatialFeaturePlugin::<PlacementDataset>::server())
            .add_systems(Update, project.in_set(GameSet::PostSimulation));
    }
}
pub struct PlacementClientPlugin;
impl Plugin for PlacementClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::spatial::SpatialFeaturePlugin::<PlacementDataset>::client());
    }
}
fn project(
    placements: Res<crate::placements::Placements>,
    blocks: Res<BlockRegistry>,
    mut store: ResMut<ClientPlacements>,
) {
    if !placements.is_changed() {
        return;
    }
    let mut active = BTreeSet::new();
    for record in placements.records.values().filter(|record| record.active) {
        let slot = blocks
            .slot_of(&record.definition)
            .expect("validated placement definition");
        let anchor = IVec3::from_array(record.anchor);
        let value = PlacementView {
            id: record.id.0,
            revision: record.revision,
            slot,
            anchor,
            orientation: record.orientation,
            portal_open: record.portal_open(&blocks),
            footprint: BoundedVec::new(
                blocks
                    .def(slot)
                    .footprint
                    .iter()
                    .map(|offset| {
                        anchor + IVec3::from_array(record.orientation.rotate_offset(*offset))
                    })
                    .collect(),
            )
            .expect("validated footprint"),
        };
        active.insert(value.id);
        if store.lookup(&value.id) != Some(&value) {
            store.upsert(value.id, value, 0);
        }
    }
    let removed: Vec<_> = store
        .entries()
        .filter_map(|(id, _)| (!active.contains(id)).then_some(*id))
        .collect();
    for id in removed {
        store.delete(&id, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn view() -> PlacementView {
        PlacementView {
            id: 7,
            revision: 1,
            slot: BlockSlot(1),
            anchor: IVec3::ZERO,
            orientation: Cardinal::East,
            portal_open: None,
            footprint: BoundedVec::new(vec![IVec3::ZERO, IVec3::new(64, 0, 0)]).unwrap(),
        }
    }
    #[test]
    fn exact_membership_excludes_holes_and_preserves_identity_until_last_subscription_leaves() {
        let value = view();
        let chunks = PlacementDataset::chunks(&value.id, &value);
        assert_eq!(
            chunks,
            vec![ChunkCoord(IVec3::ZERO), ChunkCoord(IVec3::new(2, 0, 0))]
        );
        let bytes = crate::spatial::encode_record::<PlacementDataset>(&value).unwrap();
        let mut references = crate::spatial::SpatialDecodeRegistry::default();
        references.register_reference("block", 1);
        let mut replica = ClientPlacements::default();
        for chunk in &chunks {
            replica
                .apply_replica_record(*chunk, &bytes, &references)
                .unwrap();
        }
        assert_eq!(replica.at(IVec3::ZERO).unwrap().id, 7);
        assert!(replica.at(IVec3::X).is_none());
        assert!(replica.at(IVec3::new(32, 0, 0)).is_none());
        replica.unload_replica_chunk(chunks[0]);
        assert_eq!(replica.at(IVec3::new(64, 0, 0)).unwrap().id, 7);
        replica.unload_replica_chunk(chunks[1]);
        assert!(replica.entries().next().is_none());
    }
    #[test]
    fn older_overlapping_snapshot_cannot_restore_a_previous_portal_state() {
        let old = view();
        let mut new = old.clone();
        new.revision = 2;
        new.portal_open = Some(0);
        let mut references = crate::spatial::SpatialDecodeRegistry::default();
        references.register_reference("block", 1);
        let mut replica = ClientPlacements::default();
        replica
            .apply_replica_record(
                ChunkCoord(IVec3::ZERO),
                &crate::spatial::encode_record::<PlacementDataset>(&new).unwrap(),
                &references,
            )
            .unwrap();
        replica
            .apply_replica_record(
                ChunkCoord(IVec3::new(2, 0, 0)),
                &crate::spatial::encode_record::<PlacementDataset>(&old).unwrap(),
                &references,
            )
            .unwrap();
        assert_eq!(replica.lookup(&7), Some(&new));
        replica.unload_replica_chunk(ChunkCoord(IVec3::ZERO));
        assert_eq!(replica.lookup(&7), Some(&new));
        assert!(
            replica
                .apply_replica_record(
                    ChunkCoord(IVec3::Y),
                    &crate::spatial::encode_record::<PlacementDataset>(&new).unwrap(),
                    &references
                )
                .is_err()
        );
    }

    #[test]
    fn malformed_identity_footprint_and_unknown_content_reject() {
        let mut references = crate::spatial::SpatialDecodeRegistry::default();
        assert!(PlacementDataset::from_wire(view(), &references).is_err());
        references.register_reference("block", 1);
        for case in 0..5 {
            let mut value = view();
            match case {
                0 => value.id = 0,
                1 => value.revision = 0,
                2 => value.footprint = BoundedVec::new(vec![IVec3::ZERO, IVec3::ZERO]).unwrap(),
                3 => value.anchor = IVec3::Y,
                _ => {
                    value.footprint =
                        BoundedVec::new(vec![IVec3::ZERO, IVec3::splat(i32::MAX)]).unwrap()
                }
            }
            assert!(PlacementDataset::from_wire(value, &references).is_err());
        }
    }
}
