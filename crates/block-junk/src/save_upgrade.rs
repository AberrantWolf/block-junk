//! Compatible content upgrades run on a private save image, before state
//! migrations or runtime hooks. Geometry uses the ordinary terrain transaction;
//! no Lua callback receives mutable chunks, inventories or reservation tables.
use crate::{
    blocks::{BlockRegistry, BlockSlot, TerrainSlots},
    content_contracts::{ContentContracts, contract_digest, generation_contract},
    protocol::{BlockEdit, ChunkCoord},
    save::{SaveFile, SavedChunk},
    voxel::{Chunk, ChunkEntities, EntryKind, chunk_local_to_world, world_to_chunk},
};
use bevy::prelude::*;
use block_junk_mod_api::{
    blocks::Cardinal,
    upgrades::{BlockUpgrade, FootprintUpgrade},
};
use std::collections::{BTreeMap, BTreeSet};

fn previous_registry(save: &SaveFile, current: &BlockRegistry) -> Result<BlockRegistry, String> {
    let world = save.world.as_ref().ok_or("save has no world contract")?;
    let mut sidecars = BTreeSet::new();
    for chunk in &save.edited_chunks {
        for entry in &chunk.entities.entries {
            if matches!(entry.kind, EntryKind::Anchor { .. }) {
                let slot = chunk.chunk.get(world_to_chunk(entry.cell).1);
                let id = save
                    .block_slots
                    .get(usize::from(slot.0))
                    .ok_or("old sidecar slot outside table")?;
                sidecars.insert(id.clone());
            }
        }
    }
    sidecars.extend(
        world
            .placements
            .records
            .values()
            .filter(|p| p.active)
            .map(|p| p.definition.as_str().to_owned()),
    );
    let mut definitions = Vec::new();
    for (slot, def) in current.iter() {
        if slot.is_empty() {
            continue;
        }
        let mut def = def.clone();
        if let Some(contract) = world.contracts.blocks.get(def.id.as_str()) {
            let value: serde_json::Value = serde_json::from_str(contract)
                .map_err(|error| format!("saved {} contract: {error}", def.id))?;
            def.footprint = serde_json::from_value(value["footprint"].clone())
                .map_err(|error| format!("saved {} footprint: {error}", def.id))?;
            def.structure = serde_json::from_value(value["structure"].clone())
                .map_err(|error| format!("saved {} structure: {error}", def.id))?;
            def.entity_aabb = serde_json::from_value(value["aabb"].clone())
                .map_err(|error| format!("saved {} bounds: {error}", def.id))?;
            for (name, flag) in [
                ("solid", &mut def.flags.solid),
                ("nav_passable", &mut def.flags.nav_passable),
                ("support_below", &mut def.flags.support_below),
                ("support_in_cell", &mut def.flags.support_in_cell),
                ("room_boundary", &mut def.flags.room_boundary),
                ("walkable_boundary", &mut def.flags.walkable_boundary),
            ] {
                *flag = value[name]
                    .as_bool()
                    .ok_or_else(|| format!("saved {} contract missing {name}", def.id))?;
            }
            def.mesh = sidecars
                .contains(def.id.as_str())
                .then(|| "saved-geometry-only".into());
            def.settlement = Default::default();
            def.portal_visuals.clear();
            def.facility_slots.clear();
            def.facility_actions.clear();
            def.use_slot = None;
            def.interactable = None;
        }
        definitions.push(def);
    }
    let previous = BlockRegistry::build(definitions)
        .map_err(|error| format!("saved geometry contract invalid: {error}"))?
        .0;
    for (slot, def) in current.iter() {
        if previous.slot_of(&def.id) != Some(slot) {
            return Err("upgrade registry slot alignment failed".into());
        }
    }
    Ok(previous)
}

struct Grid {
    chunks: BTreeMap<[i32; 3], SavedChunk>,
    terrain: TerrainSlots,
}
impl Grid {
    fn ensure(&mut self, cell: IVec3) {
        let coord = world_to_chunk(cell).0;
        self.chunks
            .entry(coord.0.to_array())
            .or_insert_with(|| SavedChunk {
                coord,
                chunk: Chunk::from_terrain(coord, &self.terrain),
                entities: ChunkEntities::default(),
            });
    }
    fn read(&self, cell: IVec3) -> Option<(BlockSlot, Option<EntryKind>, u64)> {
        let (coord, local) = world_to_chunk(cell);
        let chunk = self.chunks.get(&coord.0.to_array())?;
        Some((
            chunk.chunk.get(local),
            chunk.entities.get(cell),
            chunk.chunk.revision,
        ))
    }
    fn sidecar(&mut self, cell: IVec3, entry: Option<EntryKind>) {
        self.ensure(cell);
        let coord = world_to_chunk(cell).0;
        let sidecar = &mut self
            .chunks
            .get_mut(&coord.0.to_array())
            .expect("ensured chunk")
            .entities;
        sidecar.entries.retain(|old| old.cell != cell);
        if let Some(entry) = entry {
            sidecar.insert(cell, entry);
        }
    }
    fn commit(&mut self, transaction: &crate::terrain_transaction::TerrainTransaction) {
        for (cell, slot, entry) in transaction.changes() {
            self.ensure(cell);
            let (coord, local) = world_to_chunk(cell);
            let chunk = &mut self
                .chunks
                .get_mut(&coord.0.to_array())
                .expect("ensured")
                .chunk;
            chunk.apply_committed_cell(local, slot);
            self.sidecar(cell, entry);
        }
        for (coord, revision) in &transaction.revisions {
            self.chunks
                .get_mut(coord)
                .expect("staged chunk")
                .chunk
                .revision = *revision;
        }
    }
}

fn changed_blocks<'a>(
    save: &SaveFile,
    current: &'a ContentContracts,
) -> Result<BTreeMap<String, &'a BlockUpgrade>, String> {
    let world = save.world.as_ref().ok_or("save has no world")?;
    let mut referenced = BTreeSet::new();
    referenced.extend(
        world
            .placements
            .records
            .values()
            .map(|p| p.definition.as_str().to_owned()),
    );
    referenced.extend(
        world
            .preset
            .terrain
            .entries()
            .map(|(_, id)| id.as_str().to_owned()),
    );
    for chunk in &save.edited_chunks {
        for slot in &chunk.chunk.blocks {
            referenced.insert(
                save.block_slots
                    .get(usize::from(slot.0))
                    .ok_or("saved slot outside table")?
                    .clone(),
            );
        }
    }
    for (_, project) in &save.plans {
        if let crate::protocol::PlanKind::Build { slot, .. } = project.kind {
            referenced.insert(
                save.block_slots
                    .get(usize::from(slot.0))
                    .ok_or("saved project slot outside table")?
                    .clone(),
            );
        }
    }
    let mut changes = BTreeMap::new();
    for id in referenced {
        let old = world
            .contracts
            .blocks
            .get(&id)
            .ok_or_else(|| format!("saved contract missing for {id}"))?;
        let new = current
            .blocks
            .get(&id)
            .ok_or_else(|| format!("missing referenced block {id}"))?;
        if old == new {
            continue;
        }
        let from = contract_digest(old);
        let to = contract_digest(new);
        let change = current
            .upgrades
            .iter()
            .flat_map(|upgrade| &upgrade.blocks)
            .find(|change| {
                change.block.as_str() == id
                    && change.from_contract == from
                    && change.to_contract == to
            })
            .ok_or_else(|| {
                format!("block {id} requires a compatible migration from {from} to {to}")
            })?;
        changes.insert(id, change);
    }
    Ok(changes)
}

pub(crate) fn migrate(
    save: &SaveFile,
    current: &ContentContracts,
    blocks: &BlockRegistry,
    preset: &block_junk_mod_api::worlds::WorldPreset,
) -> Result<SaveFile, String> {
    crate::save::preflight_save("upgrade source", save).map_err(|error| error.to_string())?;
    preset.validate()?;
    for (_, id) in preset.terrain.entries() {
        if blocks.slot_of(id).is_none() {
            return Err(format!("upgrade target terrain references missing {id}"));
        }
    }
    save.world
        .as_ref()
        .ok_or("save has no world preset")?
        .preset
        .validate()?;
    if current.validate_save(save, preset).is_ok() {
        return Ok(save.clone());
    }
    let original = save.world.as_ref().ok_or("save has no world preset")?;
    let changes = changed_blocks(save, current)?;
    let old_generation = generation_contract(&original.preset);
    let new_generation = generation_contract(preset);
    let generation_changed = old_generation != new_generation;
    if generation_changed {
        let from = contract_digest(&old_generation);
        let to = contract_digest(&new_generation);
        if !current
            .upgrades
            .iter()
            .filter_map(|upgrade| upgrade.generation.as_ref())
            .any(|change| {
                change.preset == preset.id
                    && change.from_contract == from
                    && change.to_contract == to
            })
        {
            return Err(format!(
                "preset {} requires a compatible migration from {from} to {to}",
                preset.id
            ));
        }
    }
    let old_movement = original
        .contracts
        .movement
        .get(&original.preset.movement_profile)
        .ok_or("saved movement contract missing")?;
    let new_movement = current
        .movement
        .get(&preset.movement_profile)
        .ok_or("current movement contract missing")?;
    if old_movement != new_movement {
        let from = contract_digest(old_movement);
        let to = contract_digest(new_movement);
        if !current
            .upgrades
            .iter()
            .flat_map(|upgrade| &upgrade.movement)
            .any(|change| {
                change.profile == preset.movement_profile
                    && change.from_contract == from
                    && change.to_contract == to
            })
        {
            return Err(format!(
                "movement {} requires a compatible migration from {from} to {to}",
                preset.movement_profile
            ));
        }
    }

    for pose in save
        .npcs
        .iter()
        .map(|npc| &npc.pose)
        .chain(save.players.iter().map(|player| &player.pose))
    {
        if !pose.translation.is_finite() || pose.translation.abs().max_element() > 1_000_000. {
            return Err("upgrade source actor position outside supported world".into());
        }
    }
    for (anchor, project) in &save.plans {
        if anchor
            .to_array()
            .iter()
            .any(|v| v.unsigned_abs() > 1_000_000)
            || project
                .footprint
                .iter()
                .any(|cell| cell.to_array().iter().any(|v| v.unsigned_abs() > 1_000_064))
        {
            return Err("upgrade source project footprint outside supported world".into());
        }
    }
    let previous = previous_registry(save, blocks)?;
    let mut old_placements = original.placements.clone();
    old_placements.rebuild(&previous)?;
    for (id, change) in &changes {
        let slot = blocks
            .slot_of(&id.clone().into())
            .ok_or("upgrade block missing")?;
        if change.footprint == FootprintUpgrade::Preserve
            && previous.def(slot).footprint != blocks.def(slot).footprint
        {
            return Err(format!(
                "upgrade {id}: preserve requires an unchanged footprint; use rebuild_at_anchor"
            ));
        }
        validate_slot_map(id, change, &original.contracts.blocks[id], blocks.def(slot))?;
    }
    let mut staged = save.clone();
    crate::save::remap_block_slots(&mut staged, |id| blocks.slot_of(&id.into()))
        .map_err(|error| error.to_string())?;
    staged.block_slots = blocks
        .iter()
        .map(|(_, def)| def.id.as_str().to_owned())
        .collect();
    let mut grid = Grid {
        chunks: staged
            .edited_chunks
            .iter()
            .cloned()
            .map(|chunk| (chunk.coord.0.to_array(), chunk))
            .collect(),
        terrain: TerrainSlots::from_registry(&previous, &original.preset),
    };
    // Durable procedural facilities and their dependencies must exist in the old
    // view even when their chunks were never edited or serialized.
    for record in original
        .placements
        .records
        .values()
        .filter(|record| record.active)
    {
        let slot = blocks
            .slot_of(&record.definition)
            .ok_or("upgrade placement definition missing")?;
        let anchor = IVec3::from_array(record.anchor);
        for offset in &previous.def(slot).footprint {
            let cell = anchor + IVec3::from_array(record.orientation.rotate_offset(*offset));
            grid.ensure(cell);
            if grid.read(cell).map(|entry| entry.0) != Some(slot) {
                return Err(format!(
                    "placement {} disagrees with old saved geometry",
                    record.id.0
                ));
            }
            if previous.def(slot).mesh.is_some() || previous.def(slot).footprint.len() > 1 {
                grid.sidecar(
                    cell,
                    Some(if cell == anchor {
                        EntryKind::Anchor {
                            orientation: record.orientation,
                            portal_open: None,
                        }
                    } else {
                        EntryKind::Ghost { anchor }
                    }),
                );
            }
        }
        for offset in &blocks.def(slot).footprint {
            grid.ensure(anchor + IVec3::from_array(record.orientation.rotate_offset(*offset)));
        }
    }
    if generation_changed {
        for pose in staged
            .npcs
            .iter()
            .map(|npc| &npc.pose)
            .chain(staged.players.iter().map(|player| &player.pose))
        {
            let centre = world_to_chunk(pose.translation.floor().as_ivec3()).0;
            for x in -1..=1 {
                for y in -1..=1 {
                    for z in -1..=1 {
                        let coord = ChunkCoord(centre.0 + IVec3::new(x, y, z));
                        grid.ensure(chunk_local_to_world(coord, IVec3::ONE));
                    }
                }
            }
        }
        for item in &staged.world_items {
            if item.translation.abs().max_element() > 1_000_000. {
                return Err("upgrade source item outside supported world".into());
            }
            grid.ensure(item.translation.floor().as_ivec3());
        }
        for (_, project) in &staged.plans {
            for cell in &project.footprint {
                grid.ensure(*cell);
            }
        }
    }
    let mut anchors = BTreeMap::new();
    for chunk in grid.chunks.values() {
        for x in 1..crate::protocol::CHUNK_PADDED as i32 - 1 {
            for y in 1..crate::protocol::CHUNK_PADDED as i32 - 1 {
                for z in 1..crate::protocol::CHUNK_PADDED as i32 - 1 {
                    let local = IVec3::new(x, y, z);
                    let slot = chunk.chunk.get(local);
                    if !changes.contains_key(blocks.id_of(slot).as_str())
                        || previous.def(slot).footprint == blocks.def(slot).footprint
                    {
                        continue;
                    }
                    let cell = chunk_local_to_world(chunk.coord, local);
                    match chunk.entities.get(cell) {
                        Some(EntryKind::Ghost { .. }) => {}
                        Some(EntryKind::Anchor { orientation, .. }) => {
                            anchors.insert(
                                cell.to_array(),
                                BlockEdit {
                                    anchor: cell,
                                    slot,
                                    orientation,
                                },
                            );
                        }
                        None => {
                            anchors.insert(
                                cell.to_array(),
                                BlockEdit {
                                    anchor: cell,
                                    slot,
                                    orientation: Cardinal::East,
                                },
                            );
                        }
                    }
                }
            }
        }
    }
    for edit in anchors.values() {
        for offset in &blocks.def(edit.slot).footprint {
            grid.ensure(edit.anchor + IVec3::from_array(edit.orientation.rotate_offset(*offset)));
        }
    }
    let mut transaction = crate::terrain_transaction::TerrainTransaction::default();
    transaction.rewrite_definitions(
        &anchors.into_values().collect::<Vec<_>>(),
        &previous,
        blocks,
        &|cell| grid.read(cell),
    )?;
    grid.commit(&transaction);
    let world = staged.world.as_mut().expect("validated world");
    for record in world
        .placements
        .records
        .values_mut()
        .filter(|record| record.active)
    {
        let slot = blocks
            .slot_of(&record.definition)
            .ok_or("upgrade placement missing")?;
        let anchor = IVec3::from_array(record.anchor);
        // Replace derived sidecar entries while retaining placement identity.
        for offset in &previous.def(slot).footprint {
            let cell = anchor + IVec3::from_array(record.orientation.rotate_offset(*offset));
            if grid
                .read(cell)
                .and_then(|entry| entry.1)
                .is_some_and(|entry| match entry {
                    EntryKind::Anchor { .. } => cell == anchor,
                    EntryKind::Ghost { anchor: owner } => owner == anchor,
                })
            {
                grid.sidecar(cell, None);
            }
        }
        if blocks.def(slot).mesh.is_some() || blocks.def(slot).footprint.len() > 1 {
            for offset in &blocks.def(slot).footprint {
                let cell = anchor + IVec3::from_array(record.orientation.rotate_offset(*offset));
                grid.sidecar(
                    cell,
                    Some(if cell == anchor {
                        EntryKind::Anchor {
                            orientation: record.orientation,
                            portal_open: None,
                        }
                    } else {
                        EntryKind::Ghost { anchor }
                    }),
                );
            }
        }
        if changes.contains_key(record.definition.as_str()) {
            record.revision = record
                .revision
                .checked_add(1)
                .ok_or("placement revision exhausted during upgrade")?;
        }
    }
    upgrade_interactions(world, &mut staged.npcs, &changes, blocks, &mut grid)?;
    upgrade_projects(
        &mut staged.plans,
        &mut staged.construction.jobs,
        &changes,
        blocks,
        &mut grid,
    )?;
    world.placements.rebuild(blocks)?;
    world
        .placements
        .validate_geometry(blocks, |cell| grid.read(cell).map(|entry| entry.0))?;
    validate_actors(&staged, blocks, new_movement, &mut grid)?;
    grid.repad(blocks, preset);
    staged.edited_chunks = grid.chunks.into_values().collect();
    let world = staged.world.as_mut().expect("validated world");
    world.contracts = current.clone();
    world.preset = preset.clone();
    current.validate_save(&staged, preset)?;
    crate::save::preflight_save("upgraded save", &staged).map_err(|error| error.to_string())?;
    Ok(staged)
}

fn validate_slot_map(
    id: &str,
    change: &BlockUpgrade,
    old: &str,
    new: &block_junk_mod_api::blocks::BlockDef,
) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(old).map_err(|error| error.to_string())?;
    let old_slots: Vec<block_junk_mod_api::architecture::FacilitySlot> =
        serde_json::from_value(value["slots"].clone())
            .map_err(|error| format!("saved {id} slots: {error}"))?;
    for key in change.slot_renames.keys() {
        if !old_slots.iter().any(|slot| &slot.key == key) {
            return Err(format!(
                "upgrade {id}: renamed slot {key} did not exist in the source contract"
            ));
        }
    }
    let mut destinations = BTreeSet::new();
    for slot in old_slots {
        let key = change.slot_renames.get(&slot.key).unwrap_or(&slot.key);
        if new.facility_slots.iter().any(|slot| &slot.key == key)
            && !destinations.insert(key.clone())
        {
            return Err(format!(
                "upgrade {id}: slot renames merge distinct use identities at {key}"
            ));
        }
    }
    Ok(())
}

fn upgrade_interactions(
    world: &mut crate::save::SavedWorld,
    npcs: &mut [crate::save::SavedNpc],
    changes: &BTreeMap<String, &BlockUpgrade>,
    blocks: &BlockRegistry,
    grid: &mut Grid,
) -> Result<(), String> {
    use block_junk_mod_api::authority::{JobTarget, ReservationOwner};
    for job in world.interactions.jobs.records.values_mut() {
        if job.state.terminal() && !world.interactions.progress.contains_key(&job.id) {
            continue;
        }
        let JobTarget::Interaction {
            slot,
            placement_revision,
            action,
            exclusive,
        } = &mut job.target
        else {
            return Err("interaction ledger contains a different job class".into());
        };
        let record = world
            .placements
            .records
            .get(&slot.placement)
            .ok_or("upgraded job placement missing")?;
        if let Some(change) = changes.get(record.definition.as_str()) {
            if let Some(new) = change.slot_renames.get(&slot.key) {
                slot.key = new.clone();
            }
            *placement_revision = record.revision;
        }
        let (facility, _) =
            crate::facility_interactions::definition(record, &slot.key, action, blocks)?;
        *exclusive = facility.exclusive;
        let progress = world
            .interactions
            .progress
            .get_mut(&job.id)
            .ok_or("upgraded interaction progress missing")?;
        let anchor = IVec3::from_array(record.anchor);
        let mut approaches: Vec<_> = facility
            .approaches
            .iter()
            .map(|offset| anchor + IVec3::from_array(*offset))
            .collect();
        approaches.sort_by_key(|cell| {
            let d = *cell - IVec3::from_array(progress.approach);
            (d.abs().element_sum(), cell.to_array())
        });
        let mut destination = None;
        for cell in approaches {
            grid.ensure(cell);
            grid.ensure(cell + IVec3::Y);
            grid.ensure(cell - IVec3::Y);
            if standing_cell(grid, blocks, cell) {
                destination = Some(cell);
                break;
            }
        }
        let destination = destination
            .ok_or_else(|| format!("upgraded slot {} has no clear supported approach", slot.key))?;
        progress.approach = destination.to_array();
        if !job.state.terminal()
            && let ReservationOwner::Npc { id } = job.actor
        {
            let npc = npcs
                .iter_mut()
                .find(|npc| npc.id == id)
                .ok_or("upgraded interaction actor missing")?;
            // An active use ends at its declared approach before its transient
            // pose lock disappears. It does not retain a pose inside new geometry.
            npc.pose.translation = crate::physics::standing_pose_translation(destination);
        }
    }
    world.interactions.suspend_for_load();
    Ok(())
}

fn upgrade_projects(
    projects: &mut [(IVec3, crate::save::SavedPlanState)],
    jobs: &mut crate::action_jobs::JobLedger,
    changes: &BTreeMap<String, &BlockUpgrade>,
    blocks: &BlockRegistry,
    grid: &mut Grid,
) -> Result<(), String> {
    let mut reserved = BTreeSet::new();
    for (anchor, project) in projects {
        if let crate::protocol::PlanKind::Build { slot, orientation } = project.kind
            && changes.contains_key(blocks.id_of(slot).as_str())
        {
            project.footprint = blocks
                .def(slot)
                .footprint
                .iter()
                .map(|offset| *anchor + IVec3::from_array(orientation.rotate_offset(*offset)))
                .collect();
            project.revision = project
                .revision
                .checked_add(1)
                .ok_or("project revision exhausted during upgrade")?;
            for job in jobs
                .records
                .values_mut()
                .filter(|job| !job.state.terminal())
            {
                if let block_junk_mod_api::authority::JobTarget::Construction {
                    project_id,
                    project_revision,
                } = &mut job.target
                    && *project_id == project.project_id
                {
                    *project_revision = project.revision;
                }
            }
            for cell in &project.footprint {
                grid.ensure(*cell);
                if grid
                    .read(*cell)
                    .is_some_and(|entry| !entry.0.is_empty() || entry.1.is_some())
                {
                    return Err(format!(
                        "upgrade project {} footprint is occupied at {cell}",
                        project.project_id
                    ));
                }
            }
        }
        for cell in &project.footprint {
            if !reserved.insert(cell.to_array()) {
                return Err(format!("upgraded project footprints overlap at {cell}"));
            }
        }
    }
    Ok(())
}
fn standing_cell(grid: &Grid, blocks: &BlockRegistry, foot: IVec3) -> bool {
    let solid = |cell| {
        grid.read(cell).is_none_or(|entry| {
            !entry.0.is_empty()
                && blocks.def(entry.0).flags.solid
                && !blocks.def(entry.0).flags.walkable_boundary
        })
    };
    !solid(foot) && !solid(foot + IVec3::Y) && solid(foot - IVec3::Y)
}
fn validate_actors(
    save: &SaveFile,
    blocks: &BlockRegistry,
    movement: &str,
    grid: &mut Grid,
) -> Result<(), String> {
    let profile: block_junk_mod_api::architecture::MovementProfile =
        serde_json::from_str(movement).map_err(|error| format!("movement contract: {error}"))?;
    let half = Vec3::new(
        profile.radius.max(crate::physics::PLAYER_HALF_EXTENTS.x),
        (profile.height * 0.5).max(crate::physics::PLAYER_HALF_EXTENTS.y),
        profile.radius.max(crate::physics::PLAYER_HALF_EXTENTS.z),
    );
    for (id, pose) in save
        .npcs
        .iter()
        .map(|npc| (format!("NPC {}", npc.id), &npc.pose))
        .chain(
            save.players
                .iter()
                .map(|player| (format!("player {}", player.client_id), &player.pose)),
        )
    {
        if !pose.translation.is_finite() || pose.translation.abs().max_element() > 1_000_000. {
            return Err(format!("upgrade {id}: invalid position"));
        }
        let centre = pose.translation - Vec3::Y * crate::physics::EYE_OFFSET_FROM_CENTRE;
        let min = (centre - half + Vec3::splat(0.0001)).floor().as_ivec3();
        let max = (centre + half - Vec3::splat(0.0001)).floor().as_ivec3();
        for x in min.x..=max.x {
            for y in min.y..=max.y {
                for z in min.z..=max.z {
                    let cell = IVec3::new(x, y, z);
                    grid.ensure(cell);
                    let slot = grid.read(cell).expect("ensured actor chunk").0;
                    if !slot.is_empty()
                        && blocks.def(slot).flags.solid
                        && !blocks.def(slot).flags.walkable_boundary
                    {
                        return Err(format!(
                            "upgrade would obstruct {id} at {cell}; clear or move the actor before upgrading"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}
impl Grid {
    fn repad(&mut self, blocks: &BlockRegistry, preset: &block_junk_mod_api::worlds::WorldPreset) {
        let terrain = TerrainSlots::from_registry(blocks, preset);
        let mut generated = BTreeMap::new();
        let mut padding = Vec::new();
        for coord in self.chunks.keys() {
            let coord = ChunkCoord(IVec3::from_array(*coord));
            for x in 0..crate::protocol::CHUNK_PADDED as i32 {
                for y in 0..crate::protocol::CHUNK_PADDED as i32 {
                    for z in 0..crate::protocol::CHUNK_PADDED as i32 {
                        let local = IVec3::new(x, y, z);
                        if Chunk::is_interior(local) {
                            continue;
                        }
                        let cell = chunk_local_to_world(coord, local);
                        let slot = match self.read(cell) {
                            Some(entry) => entry.0,
                            None => {
                                let (neighbor, local) = world_to_chunk(cell);
                                generated
                                    .entry(neighbor.0.to_array())
                                    .or_insert_with(|| Chunk::from_terrain(neighbor, &terrain))
                                    .get(local)
                            }
                        };
                        padding.push((coord.0.to_array(), local, slot));
                    }
                }
            }
        }
        for (coord, local, slot) in padding {
            self.chunks
                .get_mut(&coord)
                .expect("padding owner")
                .chunk
                .set_padding(local, slot);
        }
    }
}
