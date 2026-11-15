use super::*;
use crate::{
    capabilities::{Capabilities, CapabilityWork},
    facility_access::{AccessWork, FacilityAccess},
    space_identity::SpaceId,
};
use block_junk_mod_api::{
    architecture::{PlacementId, UseSlotId},
    authority::ReservationOwner,
    blocks::Cardinal,
    capabilities::{CapabilityStatus, Value},
};
use block_junk_scripting::commands::{Command, CommandBatch, InventoryBank, TransferDirection};

#[cfg(unix)]
fn scene() -> (App, tempfile::TempDir, Entity) {
    let root = tempfile::tempdir().unwrap();
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::os::unix::fs::symlink(
        repo.join("examples/mods/independent/foundation"),
        root.path().join("foundation"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        repo.join("examples/mods/addons/area_comfort"),
        root.path().join("area_comfort"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        repo.join("examples/mods/addons/scenic_site"),
        root.path().join("scenic_site"),
    )
    .unwrap();
    let content = load_content(Side::Server, root.path(), Some("foundation:plain")).unwrap();
    let mut app = command_app_with_content(content);
    app.init_resource::<crate::server::SpatialSubscriptions>()
        .init_resource::<crate::server::ActiveSimulationChunks>()
        .init_resource::<crate::spatial_geometry::StructuralSummaries>()
        .init_resource::<crate::live_spaces::RoofEnvelope>()
        .init_resource::<crate::live_spaces::SpatialDependencies>()
        .init_resource::<crate::live_spaces::SpatialComputation>()
        .init_resource::<crate::live_spaces::PublishedSpaces>()
        .init_resource::<crate::space_identity::SpaceIdentities>()
        .init_resource::<crate::spatial_events::SpatialJournal>()
        .init_resource::<crate::settlements::SettlementDependencies>()
        .init_resource::<FacilityAccess>()
        .init_resource::<AccessWork>()
        .init_resource::<crate::facility_access::FacilityDependencies>()
        .init_resource::<Capabilities>()
        .init_resource::<CapabilityWork>()
        .init_resource::<crate::capabilities::CapabilityDependencies>()
        .add_systems(
            PostUpdate,
            (
                crate::spatial_geometry::refresh_structural_summaries,
                crate::spatial_geometry::materialize_facilities,
                crate::live_spaces::compute_spaces,
                crate::facility_access::refresh,
                crate::capabilities::refresh,
            )
                .chain(),
        );
    app.world_mut()
        .resource_mut::<AccessWork>()
        .operation_budget = 127;
    app.world_mut()
        .resource_mut::<CapabilityWork>()
        .operation_budget = 127;
    let stone = app
        .world()
        .resource::<BlockRegistry>()
        .require("foundation:stone");
    let mut chunk = crate::voxel::Chunk {
        revision: 0,
        blocks: vec![
            crate::blocks::BlockSlot::EMPTY;
            (crate::protocol::CHUNK_PADDED as usize).pow(3)
        ],
    };
    for x in 0..=9 {
        for z in 0..=9 {
            chunk.set(
                crate::voxel::world_to_chunk(IVec3::new(x + 8, 0, z + 8)).1,
                stone,
            );
            if x <= 9 {
                chunk.set(
                    crate::voxel::world_to_chunk(IVec3::new(x + 8, 4, z + 8)).1,
                    stone,
                );
                if x == 0 || x == 9 || z == 0 || z == 9 {
                    for y in 1..=3 {
                        chunk.set(
                            crate::voxel::world_to_chunk(IVec3::new(x + 8, y, z + 8)).1,
                            stone,
                        );
                    }
                }
            }
        }
    }
    let entity = app
        .world_mut()
        .spawn((chunk, crate::voxel::ChunkEntities::default()))
        .id();
    let coord = crate::protocol::ChunkCoord(IVec3::ZERO);
    app.world_mut()
        .resource_mut::<crate::voxel::ChunkMap>()
        .0
        .insert(coord, entity);
    app.world_mut()
        .resource_mut::<crate::server::ActiveSimulationChunks>()
        .0
        .insert(coord);
    (app, root, entity)
}
fn edits(app: &App, entity: Entity, batch: u64, edits: Vec<([i32; 3], Option<&str>)>) {
    let revision = app
        .world()
        .get::<crate::voxel::Chunk>(entity)
        .unwrap()
        .revision;
    let commands = edits
        .into_iter()
        .enumerate()
        .map(|(index, (cell, block))| Command::EditTerrain {
            cell: [cell[0] + 8, cell[1], cell[2] + 8],
            block: block.map(Into::into),
            orientation: Cardinal::East,
            expected_revision: revision + index as u64,
        })
        .collect();
    app.world()
        .resource::<ServerMods>()
        .0
        .commands
        .batches
        .lock()
        .unwrap()
        .push_back(CommandBatch {
            id: batch,
            mod_id: "foundation".into(),
            behavior: "test".into(),
            cause: None,
            commands,
        });
}
fn settle(app: &mut App) {
    for _ in 0..10_000 {
        app.update();
        assert!(app.world().resource::<AccessWork>().operations_last_tick <= 127);
        assert!(
            app.world()
                .resource::<CapabilityWork>()
                .operations_last_tick
                <= 127
        );
        let caps = app.world().resource::<Capabilities>();
        if !caps.pending
            && !caps.snapshot.spaces.is_empty()
            && app.world().resource::<FacilityAccess>().pending.is_empty()
            && app
                .world()
                .resource::<crate::live_spaces::PublishedSpaces>()
                .pending
                .is_empty()
        {
            return;
        }
    }
    panic!("finite mixed-room capability work failed to converge");
}
fn room(app: &App) -> SpaceId {
    *app.world()
        .resource::<crate::live_spaces::PublishedSpaces>()
        .geometry
        .iter()
        .find(|(_, space)| {
            space
                .floor
                .contains(&crate::architecture_solver::Cell(10, 1, 10))
        })
        .unwrap()
        .0
}
fn capacity(app: &App, id: SpaceId, capability: &str) -> f64 {
    let result =
        app.world().resource::<Capabilities>().snapshot.spaces[&id].profiles["foundation:humanoid"]
            .iter()
            .find(|result| result.id == capability)
            .unwrap();
    assert_eq!(result.status, CapabilityStatus::Supported, "{result:?}");
    match result.capacity.as_ref().unwrap().value {
        Some(Value::Number(value)) => value,
        ref other => panic!("unexpected capacity {other:?}"),
    }
}

fn command(app: &App, id: u64, commands: Vec<Command>) {
    app.world()
        .resource::<ServerMods>()
        .0
        .commands
        .batches
        .lock()
        .unwrap()
        .push_back(CommandBatch {
            id,
            mod_id: "foundation".into(),
            behavior: "test".into(),
            cause: None,
            commands,
        });
}

#[test]
#[cfg(unix)]
fn deterministic_colony_scenario_uses_transactions_geometry_capabilities_and_inspection() {
    let (mut app, _root, entity) = scene();
    let project_cell = [11, 1, 11];
    let pose =
        crate::physics::standing_pose_translation(IVec3::from_array(project_cell)) + Vec3::Y * 0.1;
    command(
        &app,
        100,
        vec![
            Command::SpawnNpc {
                npc_kind: "foundation:settler".into(),
                position: pose.to_array(),
            },
            Command::Designate {
                cell: project_cell,
                block: Some("foundation:bed".into()),
                orientation: Cardinal::East,
            },
        ],
    );
    app.update();
    let token = app
        .world()
        .resource::<crate::plans::Plans>()
        .get(IVec3::from_array(project_cell))
        .unwrap_or_else(|| {
            panic!(
                "scenario setup rejected: {:?}",
                app.world()
                    .resource::<ServerMods>()
                    .0
                    .commands
                    .results
                    .lock()
                    .unwrap()
            )
        })
        .token();
    let stone = app
        .world()
        .resource::<crate::items::ItemRegistry>()
        .slot_of(&"foundation:stone".into())
        .unwrap();
    let npc_id = {
        let world = app.world_mut();
        let mut query = world.query::<(&crate::npc::NpcId, &mut crate::protocol::Carrying)>();
        let (id, mut carrying) = query.iter_mut(world).next().unwrap();
        carrying.item = Some(stone);
        carrying.count = 1;
        id.0
    };
    command(
        &app,
        101,
        vec![Command::DeliverProject {
            project_id: token.0,
            revision: token.1,
            npc_id,
            item: "foundation:stone".into(),
            count: 1,
        }],
    );
    app.update();
    assert!(
        app.world()
            .resource::<crate::plans::Plans>()
            .get(IVec3::from_array(project_cell))
            .unwrap()
            .is_satisfied()
    );
    // Repainting is an atomic replacement. Its paid bundle becomes a durable
    // refund and the replacement starts with an empty escrow.
    command(
        &app,
        102,
        vec![Command::Designate {
            cell: project_cell,
            block: Some("foundation:chair".into()),
            orientation: Cardinal::North,
        }],
    );
    app.update();
    assert_eq!(
        app.world()
            .resource::<crate::construction::ConstructionLedger>()
            .pending_refunds
            .iter()
            .flat_map(|refund| &refund.items)
            .map(|(_, count)| count)
            .sum::<u32>(),
        1
    );
    let replacement = app
        .world()
        .resource::<crate::plans::Plans>()
        .get(IVec3::from_array(project_cell))
        .unwrap()
        .token();
    command(
        &app,
        103,
        vec![Command::CancelProject {
            project_id: replacement.0,
            revision: replacement.1,
        }],
    );
    app.update();
    assert!(
        app.world()
            .resource::<crate::plans::Plans>()
            .get(IVec3::from_array(project_cell))
            .is_none()
    );

    edits(
        &app,
        entity,
        104,
        vec![
            ([2, 1, 2], Some("foundation:bed")),
            ([4, 1, 2], Some("foundation:chair")),
            ([5, 1, 2], Some("foundation:table")),
            ([2, 1, 5], Some("foundation:storage")),
            ([6, 1, 5], Some("foundation:bench")),
        ],
    );
    settle(&mut app);
    let id = room(&app);
    for capability in [
        "foundation:sleeping",
        "foundation:dining",
        "foundation:crafting",
        "foundation:storage",
    ] {
        assert_eq!(capacity(&app, id, capability), 1.0);
    }
    let result = &app.world().resource::<Capabilities>().snapshot.spaces[&id];
    let mut inspection = String::new();
    crate::inspect_panel::append_space_capabilities(&mut inspection, result);
    let inspection = inspection.to_lowercase();
    assert!(inspection.contains("sleeping: supported · capacity 1"));
    assert!(inspection.contains("dining: supported · capacity 1"));
    assert!(inspection.contains("crafting: supported · capacity 1"));
    assert!(inspection.contains("storage: supported · capacity 1"));

    // Furnishings obstruct approaches, not room membership. The same evidence
    // explains the loss and recovery of usable sleeping capacity.
    let bed = app
        .world()
        .resource::<crate::placements::Placements>()
        .at(IVec3::new(10, 1, 10))
        .unwrap()
        .id;
    let blocked = [[1, 1, 2], [3, 1, 2], [2, 1, 1], [2, 1, 3]];
    edits(
        &app,
        entity,
        111,
        blocked
            .iter()
            .map(|cell| (*cell, Some("foundation:storage")))
            .collect(),
    );
    settle(&mut app);
    assert_eq!(room(&app), id);
    assert_eq!(
        app.world().resource::<FacilityAccess>().slots[&(
            UseSlotId {
                placement: bed,
                key: "primary".into(),
            },
            "foundation:humanoid".into(),
        )]
            .usable(),
        Some(false)
    );
    edits(
        &app,
        entity,
        112,
        blocked.iter().map(|cell| (*cell, None)).collect(),
    );
    settle(&mut app);
    assert_eq!(capacity(&app, id, "foundation:sleeping"), 1.);

    // A one-to-one extension keeps the space identity and changes the
    // add-on's area-derived effect without any exclusive room class change.
    let mut expansion = Vec::new();
    for x in 10..=13 {
        for z in 0..=9 {
            expansion.push(([x, 0, z], Some("foundation:stone")));
            expansion.push(([x, 4, z], Some("foundation:stone")));
        }
    }
    for x in 10..=13 {
        for z in [0, 9] {
            for y in 1..=3 {
                expansion.push(([x, y, z], Some("foundation:stone")));
            }
        }
    }
    for z in 1..=8 {
        for y in 1..=3 {
            expansion.push(([13, y, z], Some("foundation:stone")));
            expansion.push(([9, y, z], None));
        }
    }
    edits(&app, entity, 113, expansion);
    settle(&mut app);
    assert_eq!(room(&app), id);
    assert_eq!(
        app.world()
            .resource::<crate::live_spaces::PublishedSpaces>()
            .geometry[&id]
            .floor
            .len(),
        96
    );
    assert_eq!(capacity(&app, id, "area_comfort:sleeping"), 1.);

    // Commit crafting inputs through the same command service, then prove the
    // exact input bundle and job survive binary persistence mid-work.
    let bench = app
        .world()
        .resource::<crate::placements::Placements>()
        .at(IVec3::new(14, 1, 13))
        .unwrap()
        .clone();
    command(
        &app,
        114,
        vec![Command::SpawnNpc {
            npc_kind: "foundation:settler".into(),
            position: (IVec3::from_array(bench.anchor).as_vec3()
                + Vec3::new(1.5, crate::physics::EYE_OFFSET_FROM_CENTRE, 0.5))
            .to_array(),
        }],
    );
    app.update();
    let crafter = {
        let world = app.world_mut();
        let mut query = world.query::<(&crate::npc::NpcId, &mut crate::protocol::Carrying)>();
        let (id, mut carrying) = query.iter_mut(world).max_by_key(|(id, _)| id.0).unwrap();
        carrying.item = Some(stone);
        carrying.count = 1;
        id.0
    };
    command(
        &app,
        115,
        vec![
            Command::QueueCraft {
                placement_id: bench.id.0,
                revision: bench.revision,
                recipe: "foundation:brick".into(),
                quantity: 1,
            },
            Command::TransferFacility {
                placement_id: bench.id.0,
                revision: bench.revision,
                npc_id: crafter,
                item: "foundation:stone".into(),
                count: 1,
                bank: InventoryBank::Crafting,
                direction: TransferDirection::Deposit,
            },
        ],
    );
    app.update();
    command(
        &app,
        116,
        vec![Command::StartCraft {
            placement_id: bench.id.0,
            revision: bench.revision,
            npc_id: crafter,
            order_id: 1,
            order_revision: 1,
        }],
    );
    app.update();
    let station = app
        .world()
        .resource::<crate::craft_stations::CraftStations>()
        .get(IVec3::from_array(bench.anchor))
        .unwrap();
    assert_eq!(
        station.active_work.as_ref().unwrap().committed_inputs,
        vec![(stone, 1)]
    );
    let saved_stations = crate::server::convert_saved_stations(
        app.world()
            .resource::<crate::craft_stations::CraftStations>(),
        app.world().resource::<crate::items::ItemRegistry>(),
    );
    let saved_jobs = app
        .world()
        .resource::<crate::craft_stations::CraftBookings>()
        .jobs
        .clone();
    let persisted =
        bincode::serde::encode_to_vec((&saved_stations, &saved_jobs), bincode::config::standard())
            .unwrap();
    let ((loaded_stations, loaded_jobs), _): (
        (
            Vec<(IVec3, crate::save::SavedStationState)>,
            crate::action_jobs::JobLedger,
        ),
        usize,
    ) = bincode::serde::decode_from_slice(&persisted, bincode::config::standard()).unwrap();
    let loaded_inputs = &loaded_stations
        .iter()
        .find(|(cell, _)| *cell == IVec3::from_array(bench.anchor))
        .unwrap()
        .1
        .active_work
        .as_ref()
        .unwrap()
        .committed_inputs;
    assert_eq!(loaded_inputs.len(), 1);
    assert_eq!(loaded_inputs[0].item_id, "foundation:stone");
    assert_eq!(loaded_inputs[0].count, 1);
    assert!(
        loaded_jobs
            .records
            .values()
            .any(|job| !job.state.terminal())
    );

    // Damage and rebuild a three-cell portal through the west wall. The
    // threshold stays separate from floor area and replacement gets a new ID.
    edits(
        &app,
        entity,
        105,
        vec![([0, 1, 5], None), ([0, 2, 5], None), ([0, 3, 5], None)],
    );
    settle(&mut app);
    edits(
        &app,
        entity,
        106,
        vec![([0, 1, 5], Some("foundation:door"))],
    );
    settle(&mut app);
    let door_cell = IVec3::new(8, 1, 13);
    let first_door = app
        .world()
        .resource::<crate::placements::Placements>()
        .at(door_cell)
        .unwrap()
        .id;
    command(
        &app,
        107,
        vec![Command::SetPortal {
            placement_id: first_door.0,
            revision: 1,
            key: "passage".into(),
            open: false,
        }],
    );
    app.update();
    command(
        &app,
        108,
        vec![Command::SetPortal {
            placement_id: first_door.0,
            revision: 2,
            key: "passage".into(),
            open: true,
        }],
    );
    app.update();
    edits(&app, entity, 109, vec![([0, 1, 5], None)]);
    settle(&mut app);
    edits(
        &app,
        entity,
        110,
        vec![([0, 1, 5], Some("foundation:door"))],
    );
    settle(&mut app);
    let second_door = app
        .world()
        .resource::<crate::placements::Placements>()
        .at(door_cell)
        .unwrap()
        .id;
    assert_ne!(first_door, second_door);
    assert_eq!(
        room(&app),
        id,
        "portal work must not replace the hall identity"
    );
}
#[test]
#[cfg(unix)]
fn mixed_room_capabilities_conserve_area_explain_access_and_expand_mod_capacity() {
    let (mut app, _root, entity) = scene();
    edits(
        &app,
        entity,
        1,
        vec![
            ([2, 1, 2], Some("foundation:bed")),
            ([4, 1, 2], Some("foundation:bed")),
            ([2, 1, 4], Some("foundation:bed")),
            ([4, 1, 4], Some("foundation:bed")),
            ([6, 1, 5], Some("foundation:chair")),
            ([7, 1, 5], Some("foundation:table")),
            ([3, 1, 6], Some("foundation:storage")),
            ([6, 1, 2], Some("foundation:bench")),
        ],
    );
    settle(&mut app);
    let id = room(&app);
    assert_eq!(
        app.world()
            .resource::<crate::live_spaces::PublishedSpaces>()
            .geometry[&id]
            .floor
            .len(),
        64
    );
    assert_eq!(capacity(&app, id, "foundation:sleeping"), 4.);
    for capability in [
        "foundation:dining",
        "foundation:crafting",
        "foundation:storage",
    ] {
        assert_eq!(capacity(&app, id, capability), 1.);
    }
    assert_eq!(capacity(&app, id, "area_comfort:sleeping"), 2.);
    let blocked = [[1, 1, 2], [3, 1, 2], [2, 1, 1], [2, 1, 3]];
    edits(
        &app,
        entity,
        2,
        blocked
            .iter()
            .map(|cell| (*cell, Some("foundation:storage")))
            .collect(),
    );
    settle(&mut app);
    assert_eq!(room(&app), id);
    assert_eq!(
        app.world()
            .resource::<crate::live_spaces::PublishedSpaces>()
            .geometry[&id]
            .floor
            .len(),
        64
    );
    assert_eq!(capacity(&app, id, "foundation:sleeping"), 3.);
    let slot = UseSlotId {
        placement: PlacementId(1),
        key: "primary".into(),
    };
    let access = &app.world().resource::<FacilityAccess>().slots
        [&(slot.clone(), "foundation:humanoid".into())];
    assert_eq!(access.usable(), Some(false));
    assert!(
        access.approaches.iter().all(
            |approach| approach.obstruction.is_some() && approach.reason.contains("obstructed")
        )
    );
    edits(
        &app,
        entity,
        3,
        blocked.iter().map(|cell| (*cell, None)).collect(),
    );
    settle(&mut app);
    assert_eq!(capacity(&app, id, "foundation:sleeping"), 4.);
    let lease = app
        .world_mut()
        .resource_mut::<crate::interactables::InteractionClaims>()
        .claim_slot(
            IVec3::new(10, 1, 10),
            slot.clone(),
            ReservationOwner::Npc { id: 1 },
        )
        .unwrap();
    settle(&mut app);
    let sleeping =
        app.world().resource::<Capabilities>().snapshot.spaces[&id].profiles["foundation:humanoid"]
            .iter()
            .find(|result| result.id == "foundation:sleeping")
            .unwrap();
    assert_eq!(
        sleeping.attributes["installed"].value,
        Some(Value::Number(4.))
    );
    assert_eq!(
        sleeping.attributes["available"].value,
        Some(Value::Number(3.))
    );
    assert_eq!(capacity(&app, id, "foundation:sleeping"), 4.);
    app.world_mut()
        .resource_mut::<crate::interactables::InteractionClaims>()
        .release_slot(&slot, &lease);
    let mut expansion = Vec::new();
    // These are newly constructed floor surfaces, so this is a one-to-one
    // expansion, rather than a merger with an existing outdoor terrace.
    for x in 10..=13 {
        for z in 0..=9 {
            expansion.push(([x, 0, z], Some("foundation:stone")));
            expansion.push(([x, 4, z], Some("foundation:stone")));
        }
    }
    for x in 10..=13 {
        for z in [0, 9] {
            for y in 1..=3 {
                expansion.push(([x, y, z], Some("foundation:stone")));
            }
        }
    }
    for z in 1..=8 {
        for y in 1..=3 {
            expansion.push(([13, y, z], Some("foundation:stone")));
            expansion.push(([9, y, z], None));
        }
    }
    edits(&app, entity, 4, expansion);
    settle(&mut app);
    assert_eq!(room(&app), id);
    assert_eq!(
        app.world()
            .resource::<crate::live_spaces::PublishedSpaces>()
            .geometry[&id]
            .floor
            .len(),
        96
    );
    assert_eq!(capacity(&app, id, "foundation:sleeping"), 4.);
    assert_eq!(capacity(&app, id, "area_comfort:sleeping"), 3.);
}

#[test]
#[cfg(unix)]
fn overlapping_outdoor_service_areas_share_unique_slots_and_scenic_evidence() {
    let (mut app, _root, entity) = scene();
    let mut terrain = Vec::new();
    for x in 10..=22 {
        for z in 2..=10 {
            terrain.push(([x, 0, z], Some("foundation:stone")));
        }
    }
    terrain.extend([
        ([14, 1, 6], Some("scenic_site:seat")),
        ([18, 1, 6], Some("scenic_site:seat")),
        ([16, 1, 6], Some("scenic_site:view")),
    ]);
    edits(&app, entity, 20, terrain);
    settle(&mut app);

    let capabilities = app.world().resource::<Capabilities>();
    assert_eq!(capabilities.snapshot.outdoor.len(), 2);
    for area in &capabilities.snapshot.outdoor {
        assert!(!area.area.pending);
        assert_eq!(area.result.status, CapabilityStatus::Supported);
        assert_eq!(
            area.result.capacity.as_ref().unwrap().value,
            Some(Value::Number(2.))
        );
        assert_eq!(
            area.result.requirements["view"].placements.len(),
            1,
            "one multi-visible marker counts once"
        );
    }
    let first = app.world().resource::<Capabilities>().snapshot.outdoor[0]
        .anchor
        .clone();
    let lease = app
        .world_mut()
        .resource_mut::<crate::interactables::InteractionClaims>()
        .claim_slot(
            IVec3::new(22, 1, 14),
            first,
            ReservationOwner::Npc { id: 99 },
        )
        .unwrap();
    settle(&mut app);
    for area in &app.world().resource::<Capabilities>().snapshot.outdoor {
        assert_eq!(
            area.result.attributes["available"].value,
            Some(Value::Number(1.)),
            "the same reserved slot is unavailable in every overlapping area"
        );
    }
    drop(lease);
}

#[test]
#[cfg(unix)]
fn staged_preview_matches_committed_facility_access_without_mutating_source() {
    use crate::{
        architecture_solver::{Budget, Cell, Computation},
        placement_preview::{Input, Key},
    };
    use std::{collections::BTreeMap, sync::Arc, task::Poll};
    let (mut app, _root, entity) = scene();
    edits(&app, entity, 800, vec![([2, 1, 2], Some("foundation:bed"))]);
    settle(&mut app);
    for (batch, anchor, definition, expected) in [
        (801, IVec3::new(14, 1, 10), "foundation:bed", 2.),
        (802, IVec3::new(15, 1, 10), "foundation:storage", 1.),
    ] {
        if batch == 802 {
            edits(
                &app,
                entity,
                803,
                vec![
                    ([5, 1, 2], Some("foundation:storage")),
                    ([6, 1, 1], Some("foundation:storage")),
                    ([6, 1, 3], Some("foundation:storage")),
                ],
            );
            settle(&mut app);
        }
        let id = room(&app);
        let chunk_before = app
            .world()
            .get::<crate::voxel::Chunk>(entity)
            .unwrap()
            .clone();
        let placements_before =
            serde_json::to_vec(app.world().resource::<crate::placements::Placements>()).unwrap();
        let blocks = app.world().resource::<BlockRegistry>().clone();
        let slot = blocks.require(definition);
        let input = Input {
            key: Key {
                anchor,
                slot,
                orientation: Cardinal::East,
                spatial_revision: 1,
                chunks: BTreeMap::from([([0, 0, 0], chunk_before.revision)]),
                placements: BTreeMap::new(),
            },
            chunks: BTreeMap::from([(
                [0, 0, 0],
                (
                    chunk_before.clone(),
                    app.world()
                        .get::<crate::voxel::ChunkEntities>(entity)
                        .unwrap()
                        .clone(),
                ),
            )]),
            placements: app
                .world()
                .resource::<crate::placements::Placements>()
                .clone(),
            blocks,
            rules: app
                .world()
                .resource::<crate::world_rules::WorldRules>()
                .clone(),
            affected_floors: app
                .world()
                .resource::<crate::live_spaces::PublishedSpaces>()
                .geometry[&id]
                .floor
                .iter()
                .map(|cell| Cell(cell.0, cell.1, cell.2))
                .collect(),
        };
        let budget = Arc::new(Budget::default());
        let mut task = Computation::new(
            budget.clone(),
            crate::placement_preview::evaluate(input, budget),
        );
        let result = (0..100_000)
            .find_map(|_| {
                let before = task.operations;
                let result = task.advance(127);
                assert!(task.operations - before <= 127);
                if let Poll::Ready(result) = result {
                    Some(result)
                } else {
                    None
                }
            })
            .expect("finite staged edit converges");
        assert!(
            result.text.contains("staged geometry ready"),
            "{}",
            result.text
        );
        let sleeping = result
            .results
            .iter()
            .flat_map(|result| result.profiles.values().flatten())
            .find(|result| result.id == "foundation:sleeping")
            .unwrap();
        assert_eq!(
            sleeping.capacity.as_ref().unwrap().value,
            Some(Value::Number(expected))
        );
        assert!(
            sleeping.attributes["available"].value.is_none(),
            "a local preview must not fabricate reservations"
        );
        assert!(app.world().get::<crate::voxel::Chunk>(entity).unwrap() == &chunk_before);
        assert_eq!(
            serde_json::to_vec(app.world().resource::<crate::placements::Placements>()).unwrap(),
            placements_before
        );
        edits(
            &app,
            entity,
            batch,
            vec![([anchor.x - 8, anchor.y, anchor.z - 8], Some(definition))],
        );
        settle(&mut app);
        assert_eq!(capacity(&app, room(&app), "foundation:sleeping"), expected);
    }
}
