//! A staged terrain view shared by direct edits and multi-command transactions.
//! Geometry validation never writes live chunks, queues drops or sends packets.
use crate::{
    blocks::{BlockRegistry, BlockSlot},
    protocol::{AuthoritativeCellState, BlockEdit, BoundedVec, TerrainEditRecord},
    voxel::EntryKind,
};
use bevy::prelude::*;
use block_junk_mod_api::blocks::Cardinal;
use std::collections::{BTreeMap, BTreeSet};

type CellState = (BlockSlot, Option<EntryKind>, u64);
#[derive(Clone, Default)]
pub(crate) struct TerrainTransaction {
    cells: BTreeMap<[i32; 3], CellState>,
    pub effects: Vec<TerrainEditRecord>,
    pub revisions: BTreeMap<[i32; 3], u64>,
    suppressed_drops: BTreeSet<usize>,
}
impl TerrainTransaction {
    pub fn portal_state(
        &mut self,
        record: &crate::placements::PlacementRecord,
        registry: &BlockRegistry,
        base: &impl Fn(IVec3) -> Option<CellState>,
    ) -> Result<(), String> {
        let mut staged = self.clone();
        let slot = registry
            .slot_of(&record.definition)
            .ok_or("portal definition missing")?;
        let anchor = IVec3::from_array(record.anchor);
        let footprint = Self::footprint(anchor, registry.def(slot), record.orientation)?;
        let mut cells = Vec::new();
        for cell in &footprint {
            let (actual, entry, _) = staged
                .read(*cell, base)
                .ok_or("portal footprint unavailable")?;
            if actual != slot {
                return Err("portal footprint changed".into());
            }
            let expected = if *cell == anchor {
                if !matches!(entry, Some(EntryKind::Anchor{orientation,..}) if orientation == record.orientation)
                {
                    return Err("portal anchor changed".into());
                }
                EntryKind::Anchor {
                    orientation: record.orientation,
                    portal_open: record.portal_open(registry),
                }
            } else {
                if entry != Some(EntryKind::Ghost { anchor }) {
                    return Err("portal ghost changed".into());
                }
                EntryKind::Ghost { anchor }
            };
            cells.push(AuthoritativeCellState {
                world: *cell,
                slot,
                entity: Some(expected),
            });
        }
        let revisions = staged.advance_revisions(&footprint, base)?;
        for cell in &cells {
            staged
                .cells
                .insert(cell.world.to_array(), (cell.slot, cell.entity, 0));
        }
        staged.suppressed_drops.insert(staged.effects.len());
        staged.effects.push(TerrainEditRecord {
            revisions: BoundedVec::new(revisions).map_err(|error| error.to_string())?,
            anchor,
            old_slot: slot,
            orientation: record.orientation,
            cells: BoundedVec::new(cells).map_err(|error| error.to_string())?,
        });
        *self = staged;
        Ok(())
    }
    pub fn yields_drops(&self, effect: usize) -> bool {
        !self.suppressed_drops.contains(&effect)
    }
    pub fn relocate(
        &mut self,
        from: IVec3,
        edit: BlockEdit,
        registry: &BlockRegistry,
        base: &impl Fn(IVec3) -> Option<CellState>,
    ) -> Result<(), String> {
        let mut next = self.clone();
        let start = next.effects.len();
        next.remove(from, registry, base, false)?;
        next.place(edit, registry, base)?;
        next.suppressed_drops.extend(start..next.effects.len());
        *self = next;
        Ok(())
    }
    pub fn changes(&self) -> impl Iterator<Item = (IVec3, BlockSlot, Option<EntryKind>)> + '_ {
        self.cells
            .iter()
            .map(|(cell, (slot, entry, _))| (IVec3::from_array(*cell), *slot, *entry))
    }
    pub fn read(
        &self,
        cell: IVec3,
        base: &impl Fn(IVec3) -> Option<CellState>,
    ) -> Option<CellState> {
        let (slot, entry, revision) = self
            .cells
            .get(&cell.to_array())
            .copied()
            .or_else(|| base(cell))?;
        let coord = crate::voxel::world_to_chunk(cell).0.0.to_array();
        Some((
            slot,
            entry,
            self.revisions.get(&coord).copied().unwrap_or(revision),
        ))
    }
    fn advance_revisions(
        &mut self,
        cells: &[IVec3],
        base: &impl Fn(IVec3) -> Option<CellState>,
    ) -> Result<Vec<(crate::protocol::ChunkCoord, u64)>, String> {
        let mut chunks = BTreeMap::new();
        for cell in cells {
            let coord = crate::voxel::world_to_chunk(*cell).0;
            let current = self.read(*cell, base).ok_or("missing chunk revision")?.2;
            chunks.insert(
                coord.0.to_array(),
                current.checked_add(1).ok_or("chunk revision exhausted")?,
            );
        }
        for (coord, revision) in &chunks {
            self.revisions.insert(*coord, *revision);
        }
        Ok(chunks
            .into_iter()
            .map(|(coord, revision)| {
                (
                    crate::protocol::ChunkCoord(IVec3::from_array(coord)),
                    revision,
                )
            })
            .collect())
    }

    pub fn stage(
        &mut self,
        edit: BlockEdit,
        registry: &BlockRegistry,
        base: &impl Fn(IVec3) -> Option<CellState>,
    ) -> Result<(), String> {
        let mut next = self.clone();
        if edit.slot.is_empty() {
            next.remove(edit.anchor, registry, base, true)?;
        } else {
            next.place(edit, registry, base)?;
        }
        *self = next;
        Ok(())
    }
    /// Upgrade geometry without destroying object identity or producing drops.
    /// The caller owns one staged save; remove all old footprints before placing
    /// replacements so shrinking one object can make room for another. Any
    /// collision, missing dependency or revision failure discards the whole edit.
    pub fn rewrite_definitions(
        &mut self,
        edits: &[BlockEdit],
        previous: &BlockRegistry,
        current: &BlockRegistry,
        base: &impl Fn(IVec3) -> Option<CellState>,
    ) -> Result<(), String> {
        let mut next = self.clone();
        for edit in edits {
            next.remove(edit.anchor, previous, base, false)?;
        }
        for edit in edits {
            next.place(*edit, current, base)?;
        }
        *self = next;
        Ok(())
    }
    /// Authored self-transition; unlike harvesting, replacing a block does not
    /// first create its depleted form. Both footprints validate before commit.
    pub fn replace(
        &mut self,
        edit: BlockEdit,
        registry: &BlockRegistry,
        base: &impl Fn(IVec3) -> Option<CellState>,
    ) -> Result<(), String> {
        let mut next = self.clone();
        next.remove(edit.anchor, registry, base, false)?;
        next.place(edit, registry, base)?;
        *self = next;
        Ok(())
    }
    fn footprint(
        anchor: IVec3,
        def: &block_junk_mod_api::blocks::BlockDef,
        orientation: Cardinal,
    ) -> Result<Vec<IVec3>, String> {
        if anchor
            .to_array()
            .iter()
            .any(|v| v.unsigned_abs() > 1_000_000)
        {
            return Err("edit coordinate outside supported bounds".into());
        }
        let mut cells: Vec<_> = def
            .footprint
            .iter()
            .map(|offset| anchor + IVec3::from_array(orientation.rotate_offset(*offset)))
            .collect();
        cells.sort_by_key(|v| v.to_array());
        cells.dedup();
        if cells.is_empty()
            || cells.len() > crate::protocol::MAX_BLOCK_FOOTPRINT_CELLS
            || !cells.contains(&anchor)
        {
            return Err("invalid edit footprint".into());
        }
        Ok(cells)
    }
    fn place(
        &mut self,
        edit: BlockEdit,
        registry: &BlockRegistry,
        base: &impl Fn(IVec3) -> Option<CellState>,
    ) -> Result<(), String> {
        let def = registry
            .try_def(edit.slot)
            .ok_or("unknown placement definition")?;
        let footprint = Self::footprint(edit.anchor, def, edit.orientation)?;
        for cell in &footprint {
            let (slot, sidecar, _) = self
                .read(*cell, base)
                .ok_or("placement depends on an unavailable chunk")?;
            if !slot.is_empty() || sidecar.is_some() {
                return Err("placement footprint occupied".into());
            }
        }
        let needs_sidecar = crate::placements::Placements::tracked(def);
        let revisions = self.advance_revisions(&footprint, base)?;
        let cells = footprint
            .into_iter()
            .map(|world| {
                let entity = needs_sidecar.then_some(if world == edit.anchor {
                    EntryKind::Anchor {
                        orientation: edit.orientation,
                        portal_open: None,
                    }
                } else {
                    EntryKind::Ghost {
                        anchor: edit.anchor,
                    }
                });
                self.cells.insert(world.to_array(), (edit.slot, entity, 0));
                AuthoritativeCellState {
                    world,
                    slot: edit.slot,
                    entity,
                }
            })
            .collect();
        self.effects.push(TerrainEditRecord {
            revisions: BoundedVec::new(revisions).map_err(|e| e.to_string())?,
            anchor: edit.anchor,
            old_slot: BlockSlot::EMPTY,
            orientation: edit.orientation,
            cells: BoundedVec::new(cells).map_err(|e| e.to_string())?,
        });
        Ok(())
    }
    fn remove(
        &mut self,
        clicked: IVec3,
        registry: &BlockRegistry,
        base: &impl Fn(IVec3) -> Option<CellState>,
        deplete: bool,
    ) -> Result<(), String> {
        let (clicked_slot, entry, _) = self
            .read(clicked, base)
            .ok_or("removal depends on unavailable chunk")?;
        if clicked_slot.is_empty() {
            return Err("removal target is empty".into());
        }
        let (anchor, slot, orientation) = match entry {
            Some(EntryKind::Anchor { orientation, .. }) => (clicked, clicked_slot, orientation),
            Some(EntryKind::Ghost { anchor }) => {
                let (slot, entry, _) = self
                    .read(anchor, base)
                    .ok_or("removal anchor is unavailable")?;
                let Some(EntryKind::Anchor { orientation, .. }) = entry else {
                    return Err("broken ghost/anchor relationship".into());
                };
                if slot != clicked_slot {
                    return Err("ghost and anchor definitions disagree".into());
                }
                (anchor, slot, orientation)
            }
            None => (clicked, clicked_slot, Cardinal::default()),
        };
        let def = registry.try_def(slot).ok_or("unknown removed definition")?;
        let footprint = Self::footprint(anchor, def, orientation)?;
        if !footprint.contains(&clicked) {
            return Err("clicked ghost is outside authored footprint".into());
        }
        let needs_sidecar = crate::placements::Placements::tracked(def);
        for cell in &footprint {
            let (current, entry, _) = self
                .read(*cell, base)
                .ok_or("removal footprint unavailable")?;
            if current != slot {
                return Err("removal footprint changed".into());
            }
            if needs_sidecar {
                let consistent = match entry {
                    Some(EntryKind::Anchor {
                        orientation: actual,
                        ..
                    }) => *cell == anchor && actual == orientation,
                    Some(EntryKind::Ghost { anchor: actual }) => {
                        *cell != anchor && actual == anchor
                    }
                    None => {
                        footprint.len() == 1
                            && *cell == anchor
                            && orientation == Cardinal::default()
                    }
                };
                if !consistent {
                    return Err("removal footprint has inconsistent sidecar ownership".into());
                }
            }
        }
        let revisions = self.advance_revisions(&footprint, base)?;
        let cells = footprint
            .into_iter()
            .map(|world| {
                self.cells
                    .insert(world.to_array(), (BlockSlot::EMPTY, None, 0));
                AuthoritativeCellState {
                    world,
                    slot: BlockSlot::EMPTY,
                    entity: None,
                }
            })
            .collect();
        self.effects.push(TerrainEditRecord {
            revisions: BoundedVec::new(revisions).map_err(|e| e.to_string())?,
            anchor,
            old_slot: slot,
            orientation,
            cells: BoundedVec::new(cells).map_err(|e| e.to_string())?,
        });
        if deplete && let Some(depleted) = &def.depleted_block {
            let slot = registry
                .slot_of(depleted)
                .ok_or("missing depleted definition")?;
            self.place(
                BlockEdit {
                    anchor,
                    slot,
                    orientation: Cardinal::default(),
                },
                registry,
                base,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn registry() -> BlockRegistry {
        let defs=serde_json::from_value(serde_json::json!([
            {"id":"test:wide","display_name":"Wide","flags":{},"color":[1,1,1],"footprint":[[0,0,0],[1,0,0]]},
            {"id":"test:crop","display_name":"Crop","flags":{},"color":[1,1,1],"depleted_block":"test:wide"}
        ])).unwrap();
        BlockRegistry::build(defs).unwrap().0
    }
    #[test]
    fn staged_edits_see_prior_commands_without_mutating_live_geometry() {
        let registry = registry();
        let wide = registry.require("test:wide");
        let base = |_| Some((BlockSlot::EMPTY, None, 0));
        let mut tx = TerrainTransaction::default();
        tx.stage(
            BlockEdit {
                anchor: IVec3::ZERO,
                slot: wide,
                orientation: Cardinal::East,
            },
            &registry,
            &base,
        )
        .unwrap();
        assert!(
            tx.stage(
                BlockEdit {
                    anchor: IVec3::X,
                    slot: wide,
                    orientation: Cardinal::East
                },
                &registry,
                &base
            )
            .is_err()
        );
        assert_eq!(tx.effects.len(), 1);
        assert_eq!(base(IVec3::ZERO).unwrap().0, BlockSlot::EMPTY);
        tx.stage(
            BlockEdit {
                anchor: IVec3::X,
                slot: BlockSlot::EMPTY,
                orientation: Cardinal::East,
            },
            &registry,
            &base,
        )
        .unwrap();
        assert!(
            tx.changes()
                .all(|(_, slot, entry)| slot.is_empty() && entry.is_none())
        );
        assert_eq!(tx.effects.len(), 2);
    }
    #[test]
    fn failed_depletion_and_unknown_footprint_leave_no_effects() {
        let registry = registry();
        let crop = registry.require("test:crop");
        let wide = registry.require("test:wide");
        let base = |cell| {
            Some((
                if cell == IVec3::ZERO {
                    crop
                } else if cell == IVec3::X {
                    wide
                } else {
                    BlockSlot::EMPTY
                },
                None,
                0,
            ))
        };
        let mut tx = TerrainTransaction::default();
        assert!(
            tx.stage(
                BlockEdit {
                    anchor: IVec3::ZERO,
                    slot: BlockSlot::EMPTY,
                    orientation: Cardinal::East
                },
                &registry,
                &base
            )
            .is_err()
        );
        assert!(tx.effects.is_empty());
        assert_eq!(tx.changes().count(), 0);
        let incomplete = |cell| {
            if cell == IVec3::X {
                None
            } else {
                Some((BlockSlot::EMPTY, None, 0))
            }
        };
        assert!(
            tx.stage(
                BlockEdit {
                    anchor: IVec3::ZERO,
                    slot: wide,
                    orientation: Cardinal::East
                },
                &registry,
                &incomplete
            )
            .is_err()
        );
        assert!(tx.effects.is_empty());
        assert_eq!(tx.changes().count(), 0);
    }
}
