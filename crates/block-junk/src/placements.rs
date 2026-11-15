//! Durable furnishing identities shared by commands and the spatial model.
//! Cell indexes are derived; replacing a destroyed object never reuses its ID.
use crate::{
    blocks::{BlockRegistry, BlockSlot},
    protocol::TerrainEditRecord,
};
use bevy::prelude::*;
use block_junk_mod_api::{
    architecture::PlacementId,
    blocks::{BlockId, Cardinal},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlacementRecord {
    pub id: PlacementId,
    pub definition: BlockId,
    pub anchor: [i32; 3],
    pub orientation: Cardinal,
    pub revision: u64,
    pub active: bool,
    pub authored_state: String,
}
impl PlacementRecord {
    pub fn portal_open(&self, registry: &BlockRegistry) -> Option<u64> {
        let state: serde_json::Value = serde_json::from_str(&self.authored_state).ok()?;
        let overrides = state.get("portals")?.as_object()?;
        let def = registry.def(registry.slot_of(&self.definition)?);
        Some(
            def.structure
                .portals
                .iter()
                .enumerate()
                .fold(0, |mask, (index, portal)| {
                    let open = overrides
                        .get(&portal.key)
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(portal.initially_open);
                    mask | (u64::from(open) << index)
                }),
        )
    }
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct Placements {
    pub next_id: u64,
    pub records: BTreeMap<PlacementId, PlacementRecord>,
    #[serde(skip)]
    cells: BTreeMap<[i32; 3], PlacementId>,
}
impl Default for Placements {
    fn default() -> Self {
        Self {
            next_id: 1,
            records: Default::default(),
            cells: Default::default(),
        }
    }
}
impl Placements {
    /// Change an authored portal without replacing the furnishing or its slots.
    /// The caller stages this alongside terrain replication and causal effects.
    pub fn set_portal(
        &mut self,
        id: PlacementId,
        revision: u64,
        key: &str,
        open: bool,
        registry: &BlockRegistry,
    ) -> Result<bool, String> {
        let record = self
            .records
            .get_mut(&id)
            .filter(|record| record.active && record.revision == revision)
            .ok_or("placement missing, retired or revision stale")?;
        let def = registry.def(
            registry
                .slot_of(&record.definition)
                .ok_or("placement definition missing")?,
        );
        let portal = def
            .structure
            .portals
            .iter()
            .find(|portal| portal.key == key)
            .ok_or("placement has no portal with this key")?;
        let mut state: serde_json::Value =
            serde_json::from_str(&record.authored_state).map_err(|error| error.to_string())?;
        let object = state
            .as_object_mut()
            .ok_or("placement state must be an object")?;
        let states = object
            .entry("portals")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or("portal state must be an object")?;
        let previous = states
            .get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(portal.initially_open);
        if previous == open {
            return Ok(false);
        }
        let next_revision = record
            .revision
            .checked_add(1)
            .ok_or("placement revision exhausted")?;
        states.insert(key.into(), serde_json::Value::Bool(open));
        let encoded = serde_json::to_string(&state).map_err(|error| error.to_string())?;
        if encoded.len() > 64 * 1024 {
            return Err("placement state exceeds byte limit".into());
        }
        record.authored_state = encoded;
        record.revision = next_revision;
        Ok(true)
    }
    pub fn relocate(
        &mut self,
        id: PlacementId,
        revision: u64,
        anchor: IVec3,
        orientation: Cardinal,
        registry: &BlockRegistry,
    ) -> Result<(), String> {
        let mut staged = self.clone();
        let record = staged
            .records
            .get_mut(&id)
            .filter(|record| record.active && record.revision == revision)
            .ok_or("placement missing, retired or revision stale")?;
        if record.anchor == anchor.to_array() && record.orientation == orientation {
            return Ok(());
        }
        record.anchor = anchor.to_array();
        record.orientation = orientation;
        record.revision = record
            .revision
            .checked_add(1)
            .ok_or("placement revision exhausted")?;
        staged.rebuild(registry)?;
        *self = staged;
        Ok(())
    }
    pub fn validate_saved(
        &self,
        save: &crate::save::SaveFile,
        registry: &BlockRegistry,
        preset: &block_junk_mod_api::worlds::WorldPreset,
    ) -> Result<(), String> {
        let mut restored = self.clone();
        restored.rebuild(registry)?;
        let edited: std::collections::HashMap<_, _> = save
            .edited_chunks
            .iter()
            .map(|chunk| (chunk.coord, &chunk.chunk))
            .collect();
        let generated = std::cell::RefCell::new(std::collections::HashMap::new());
        let slots = crate::blocks::TerrainSlots::from_registry(registry, preset);
        restored.validate_geometry(registry, |cell| {
            let (coord, local) = crate::voxel::world_to_chunk(cell);
            if let Some(chunk) = edited.get(&coord) {
                return Some(chunk.get(local));
            }
            Some(
                generated
                    .borrow_mut()
                    .entry(coord)
                    .or_insert_with(|| crate::voxel::Chunk::from_terrain(coord, &slots))
                    .get(local),
            )
        })
    }
    /// Rebuild derived chunk sidecars from durable placement ownership. The
    /// persisted copy is never an independent authority for orientation/anchors.
    pub fn sidecar_for(
        &self,
        coord: crate::protocol::ChunkCoord,
        registry: &BlockRegistry,
    ) -> crate::voxel::ChunkEntities {
        let mut result = crate::voxel::ChunkEntities::default();
        let size = crate::protocol::CHUNK_SIZE as i32;
        let min = coord.0 * size;
        let max = min + IVec3::splat(size - 1);
        for (cell, id) in self
            .cells
            .range([min.x, i32::MIN, i32::MIN]..=[max.x, i32::MAX, i32::MAX])
        {
            if cell[1] < min.y || cell[1] > max.y || cell[2] < min.z || cell[2] > max.z {
                continue;
            }
            let record = &self.records[id];
            let anchor = IVec3::from_array(record.anchor);
            let cell = IVec3::from_array(*cell);
            result.insert(
                cell,
                if cell == anchor {
                    crate::voxel::EntryKind::Anchor {
                        orientation: record.orientation,
                        portal_open: record.portal_open(registry),
                    }
                } else {
                    crate::voxel::EntryKind::Ghost { anchor }
                },
            );
        }
        result
    }
    pub fn at(&self, cell: IVec3) -> Option<&PlacementRecord> {
        self.cells
            .get(&cell.to_array())
            .and_then(|id| self.records.get(id))
    }
    pub fn contains(&self, id: u64) -> bool {
        self.records.contains_key(&PlacementId(id))
    }
    /// Materialize a procedural furnishing only when an authoritative consumer
    /// requests it. This never scans unrelated chunks or counts occupied cells
    /// as separate objects.
    pub fn index_existing(
        &mut self,
        anchor: IVec3,
        slot: BlockSlot,
        orientation: Cardinal,
        registry: &BlockRegistry,
        read: impl Fn(IVec3) -> Option<BlockSlot>,
    ) -> Result<(), String> {
        if self.at(anchor).is_some() {
            return Ok(());
        }
        let def = registry.def(slot);
        if !Self::tracked(def) {
            return Ok(());
        }
        let cells: Vec<_> = def
            .footprint
            .iter()
            .map(|offset| anchor + IVec3::from_array(orientation.rotate_offset(*offset)))
            .collect();
        if cells
            .iter()
            .any(|cell| read(*cell) != Some(slot) || self.cells.contains_key(&cell.to_array()))
        {
            return Err("procedural furnishing footprint is unavailable or inconsistent".into());
        }
        let next = self
            .next_id
            .checked_add(1)
            .ok_or("placement identity space exhausted")?;
        let id = PlacementId(self.next_id);
        self.next_id = next;
        self.records.insert(
            id,
            PlacementRecord {
                id,
                definition: def.id.clone(),
                anchor: anchor.to_array(),
                orientation,
                revision: 1,
                active: true,
                authored_state: "{}".into(),
            },
        );
        for cell in cells {
            self.cells.insert(cell.to_array(), id);
        }
        Ok(())
    }
    pub(crate) fn tracked(def: &block_junk_mod_api::blocks::BlockDef) -> bool {
        !def.environmental_attributes.is_empty()
            || def.settlement != block_junk_mod_api::architecture::SettlementParticipation::None
            || def.mesh.is_some()
            || def.footprint.len() > 1
            || !def.structure.partitions.is_empty()
            || !def.structure.stairs.is_empty()
            || !def.facility_slots.is_empty()
            || !def.structure.portals.is_empty()
            || def.interactable.is_some()
            || def.container.is_some()
            || def.station_tag.is_some()
    }
    fn retire(
        &mut self,
        cell: IVec3,
        old: BlockSlot,
        registry: &BlockRegistry,
    ) -> Result<(), String> {
        let Some(id) = self.cells.get(&cell.to_array()).copied() else {
            return Ok(());
        };
        let record = self.records.get_mut(&id).expect("indexed identity");
        if record.anchor != cell.to_array() || &record.definition != registry.id_of(old) {
            return Err(format!(
                "placement {} disagrees with authoritative geometry",
                id.0
            ));
        }
        record.revision = record
            .revision
            .checked_add(1)
            .ok_or("placement revision exhausted")?;
        record.active = false;
        self.cells.retain(|_, owner| *owner != id);
        Ok(())
    }
    pub fn apply_effects(
        &mut self,
        effects: &[TerrainEditRecord],
        registry: &BlockRegistry,
    ) -> Result<(), String> {
        let mut staged = self.clone();
        for effect in effects {
            if !effect.old_slot.is_empty() {
                staged.retire(effect.anchor, effect.old_slot, registry)?;
            }
            let Some(cell) = effect.cells.first().filter(|cell| !cell.slot.is_empty()) else {
                continue;
            };
            let def = registry.def(cell.slot);
            if !Self::tracked(def) {
                continue;
            }
            if effect
                .cells
                .iter()
                .any(|cell| staged.cells.contains_key(&cell.world.to_array()))
            {
                return Err("placement identity footprint is occupied".into());
            }
            let next = staged
                .next_id
                .checked_add(1)
                .ok_or("placement identity space exhausted")?;
            let id = PlacementId(staged.next_id);
            staged.next_id = next;
            staged.records.insert(
                id,
                PlacementRecord {
                    id,
                    definition: def.id.clone(),
                    anchor: effect.anchor.to_array(),
                    orientation: effect.orientation,
                    revision: 1,
                    active: true,
                    authored_state: "{}".into(),
                },
            );
            for cell in &effect.cells {
                staged.cells.insert(cell.world.to_array(), id);
            }
        }
        *self = staged;
        Ok(())
    }
    /// Restore the derived index only after validating every durable record.
    pub fn rebuild(&mut self, registry: &BlockRegistry) -> Result<(), String> {
        let mut cells = BTreeMap::new();
        for (id, record) in &self.records {
            if id.0 == 0
                || *id != record.id
                || id.0 >= self.next_id
                || record.revision == 0
                || record.anchor.iter().any(|v| v.unsigned_abs() > 1_000_000)
                || record.authored_state.len() > 64 * 1024
            {
                return Err(format!("invalid placement identity {}", id.0));
            }
            let state: serde_json::Value =
                serde_json::from_str(&record.authored_state).map_err(|e| e.to_string())?;
            let object = state
                .as_object()
                .ok_or("placement state must be an object")?;
            let slot = registry.slot_of(&record.definition).ok_or_else(|| {
                format!(
                    "placement {} references missing {}",
                    id.0, record.definition
                )
            })?;
            if !record.active {
                continue;
            }
            let def = registry.def(slot);
            if let Some(portals) = object.get("portals") {
                let portals = portals
                    .as_object()
                    .ok_or("portal state must be an object")?;
                if portals.iter().any(|(key, value)| {
                    !value.is_boolean()
                        || !def
                            .structure
                            .portals
                            .iter()
                            .any(|portal| &portal.key == key)
                }) {
                    return Err(format!(
                        "placement {} has an unknown portal key or non-Boolean state",
                        id.0
                    ));
                }
            }
            // Once allocated, identity survives a compatible content change
            // that removes facility/render metadata. Retiring it would orphan
            // mod state. Only destruction/replacement retires the record.
            if slot.is_empty() {
                return Err(format!("active placement {} cannot refer to air", id.0));
            }
            for offset in &def.footprint {
                let offset = record.orientation.rotate_offset(*offset);
                let cell = std::array::from_fn(|axis| record.anchor[axis] + offset[axis]);
                if cells.insert(cell, *id).is_some() {
                    return Err("saved placement footprints overlap".into());
                }
            }
        }
        if self.next_id == 0 {
            return Err("invalid placement allocator".into());
        }
        self.cells = cells;
        Ok(())
    }
    pub fn validate_geometry(
        &self,
        registry: &BlockRegistry,
        read: impl Fn(IVec3) -> Option<BlockSlot>,
    ) -> Result<(), String> {
        for (cell, id) in &self.cells {
            let record = &self.records[id];
            let expected = registry
                .slot_of(&record.definition)
                .ok_or("missing placement definition")?;
            if read(IVec3::from_array(*cell)) != Some(expected) {
                return Err(format!(
                    "placement {} footprint disagrees with saved chunks at {cell:?}",
                    id.0
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn portal_fixture_registry() -> BlockRegistry {
    let defs = serde_json::from_value(serde_json::json!([
            {"id":"test:door","display_name":"Door","flags":{"solid":true},"color":[1,1,1],
             "footprint":[[0,0,0],[0,1,0],[0,2,0]],
             "portal_visuals":[{"portal":"passage","hidden_cells_when_open":[[0,0,0],[0,1,0]]}],
             "structure":{"portals":[{"key":"passage","faces":[{"cell":[0,0,0],"normal":[1,0,0],"window":false}],"threshold":[[0,0,0]],"volume":[[0,0,0],[0,1,0]],"initially_open":true}]}}
        ])).unwrap();
    BlockRegistry::build(defs).unwrap().0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{protocol::BlockEdit, terrain_transaction::TerrainTransaction};
    #[test]
    fn portal_state_preserves_identity_and_restores_collision_for_every_rotation() {
        let registry = portal_fixture_registry();
        let slot = registry.require("test:door");
        for orientation in [
            Cardinal::North,
            Cardinal::East,
            Cardinal::South,
            Cardinal::West,
        ] {
            let mut terrain = TerrainTransaction::default();
            let base = |_| Some((BlockSlot::EMPTY, None, 0));
            terrain
                .stage(
                    BlockEdit {
                        anchor: IVec3::ZERO,
                        slot: registry.require("test:door"),
                        orientation,
                    },
                    &registry,
                    &base,
                )
                .unwrap();
            let mut placements = Placements::default();
            placements
                .apply_effects(&terrain.effects, &registry)
                .unwrap();
            let id = placements.at(IVec3::ZERO).unwrap().id;
            assert!(
                !placements
                    .set_portal(id, 1, "passage", true, &registry)
                    .unwrap()
            );
            assert!(
                placements
                    .set_portal(id, 1, "missing", false, &registry)
                    .is_err()
            );
            assert!(
                placements
                    .set_portal(id, 1, "passage", false, &registry)
                    .unwrap()
            );
            assert!(
                placements
                    .set_portal(id, 1, "passage", true, &registry)
                    .is_err()
            );
            let record = &placements.records[&id];
            assert_eq!(record.revision, 2);
            terrain.portal_state(record, &registry, &base).unwrap();
            assert!(!terrain.yields_drops(1));
            assert!(matches!(
                terrain.read(IVec3::ZERO, &base).unwrap().1,
                Some(crate::voxel::EntryKind::Anchor {
                    portal_open: Some(0),
                    ..
                })
            ));
            for y in 0..3 {
                let cell = IVec3::new(0, y, 0);
                let read = |at| {
                    terrain
                        .read(at, &base)
                        .map(|(slot, entry, _)| (slot, entry))
                };
                assert!(!crate::voxel::portal_cell_hidden(
                    cell, slot, &registry, &read
                ));
                let read_open = |at| {
                    read(at).map(|(slot, entry)| {
                        (
                            slot,
                            match entry {
                                Some(crate::voxel::EntryKind::Anchor { orientation, .. }) => {
                                    Some(crate::voxel::EntryKind::Anchor {
                                        orientation,
                                        portal_open: Some(1),
                                    })
                                }
                                other => other,
                            },
                        )
                    })
                };
                assert_eq!(
                    crate::voxel::portal_cell_hidden(cell, slot, &registry, &read_open),
                    y < 2,
                    "the lintel remains visible"
                );
            }
            let shapes = &registry.collision[slot.0 as usize][orientation as usize];
            let closed = shapes.at(IVec3::ZERO, record.portal_open(&registry));
            let open = shapes.at(IVec3::ZERO, None);
            let volume = |boxes: &[crate::collision::Aabb]| {
                boxes
                    .iter()
                    .map(|b| (b.max - b.min).element_product())
                    .sum::<f32>()
            };
            assert_eq!(volume(&open), 1.0, "the lintel remains solid");
            assert_eq!(
                volume(&closed),
                3.0,
                "the closed leaf restores only passage cells"
            );
            let bytes =
                bincode::serde::encode_to_vec(&placements, bincode::config::standard()).unwrap();
            let (mut restored, _): (Placements, usize) =
                bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
            restored.rebuild(&registry).unwrap();
            assert_eq!(restored.records[&id], *record);
            assert!(matches!(
                restored
                    .sidecar_for(crate::protocol::ChunkCoord(IVec3::ZERO), &registry)
                    .get(IVec3::ZERO),
                Some(crate::voxel::EntryKind::Anchor {
                    portal_open: Some(0),
                    ..
                })
            ));
        }
    }
    fn fixture() -> (BlockRegistry, TerrainTransaction) {
        let defs = serde_json::from_value(serde_json::json!([
            {"id":"test:bed","display_name":"Bed","flags":{},"color":[1,1,1],"mesh":"unused.gltf","footprint":[[0,0,0],[1,0,0]]}
        ])).unwrap();
        let registry = BlockRegistry::build(defs).unwrap().0;
        let mut terrain = TerrainTransaction::default();
        terrain
            .stage(
                BlockEdit {
                    anchor: IVec3::ZERO,
                    slot: registry.require("test:bed"),
                    orientation: Cardinal::East,
                },
                &registry,
                &|_| Some((BlockSlot::EMPTY, None, 0)),
            )
            .unwrap();
        (registry, terrain)
    }
    #[test]
    fn one_identity_spans_footprint_and_survives_index_rebuild() {
        let (registry, terrain) = fixture();
        let mut placements = Placements::default();
        placements
            .apply_effects(&terrain.effects, &registry)
            .unwrap();
        assert_eq!(
            placements.at(IVec3::ZERO).unwrap().id,
            placements.at(IVec3::X).unwrap().id
        );
        let json = serde_json::to_string(&placements).unwrap();
        let mut restored: Placements = serde_json::from_str(&json).unwrap();
        assert!(restored.at(IVec3::ZERO).is_none());
        restored.rebuild(&registry).unwrap();
        assert_eq!(placements.at(IVec3::X), restored.at(IVec3::X));
        restored
            .validate_geometry(&registry, |_| Some(registry.require("test:bed")))
            .unwrap();
        assert!(
            restored
                .validate_geometry(&registry, |_| Some(BlockSlot::EMPTY))
                .is_err()
        );
    }
    #[test]
    fn destruction_retires_identity_and_replacement_gets_a_new_one() {
        let (registry, terrain) = fixture();
        let mut placements = Placements::default();
        placements
            .apply_effects(&terrain.effects, &registry)
            .unwrap();
        let old = placements.at(IVec3::ZERO).unwrap().id;
        let mut removal = TerrainTransaction::default();
        removal
            .stage(
                BlockEdit {
                    anchor: IVec3::X,
                    slot: BlockSlot::EMPTY,
                    orientation: Cardinal::East,
                },
                &registry,
                &|world| {
                    terrain.effects[0]
                        .cells
                        .iter()
                        .find(|cell| cell.world == world)
                        .map(|cell| (cell.slot, cell.entity, 1))
                },
            )
            .unwrap();
        placements
            .apply_effects(&removal.effects, &registry)
            .unwrap();
        assert!(placements.at(IVec3::ZERO).is_none());
        assert!(!placements.records[&old].active);
        placements
            .apply_effects(&terrain.effects, &registry)
            .unwrap();
        assert_ne!(placements.at(IVec3::ZERO).unwrap().id, old);
        assert!(!placements.records[&old].active);
    }
    #[test]
    fn sidecars_rebuild_from_durable_identity_and_orientation() {
        let (registry, terrain) = fixture();
        let mut placements = Placements::default();
        placements
            .apply_effects(&terrain.effects, &registry)
            .unwrap();
        let id = placements.at(IVec3::ZERO).unwrap().id;
        placements
            .relocate(id, 1, IVec3::new(31, 0, 0), Cardinal::East, &registry)
            .unwrap();
        let bytes =
            bincode::serde::encode_to_vec(&placements, bincode::config::standard()).unwrap();
        let (mut restored, _): (Placements, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        restored.rebuild(&registry).unwrap();
        let a = restored.sidecar_for(crate::protocol::ChunkCoord(IVec3::ZERO), &registry);
        let b = restored.sidecar_for(crate::protocol::ChunkCoord(IVec3::X), &registry);
        assert_eq!(
            a.get(IVec3::new(31, 0, 0)),
            Some(crate::voxel::EntryKind::Anchor {
                orientation: Cardinal::East,
                portal_open: None
            })
        );
        assert_eq!(
            b.get(IVec3::new(32, 0, 0)),
            Some(crate::voxel::EntryKind::Ghost {
                anchor: IVec3::new(31, 0, 0)
            })
        );
        assert_eq!(restored.at(IVec3::new(32, 0, 0)).unwrap().id, id);
    }
    #[test]
    fn identity_exhaustion_rejects_without_partial_index_updates() {
        let (registry, terrain) = fixture();
        let mut placements = Placements {
            next_id: u64::MAX,
            ..Default::default()
        };
        assert!(
            placements
                .apply_effects(&terrain.effects, &registry)
                .is_err()
        );
        assert!(placements.records.is_empty());
        assert!(placements.at(IVec3::ZERO).is_none());
    }
}
