//! Exercises production request handlers across two independent Apps and real
//! UDP sockets. Authentication is seeded after transport connection: identity
//! challenge/signature tests own that gate; all mutation authorization is real.
use super::*;
use crate::protocol::{PlanEdit, StateSyncChannel};

fn pump(server: &mut App, client: &mut App, frames: usize) {
    for _ in 0..frames {
        server.update();
        client.update();
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn receive_plan_mirror(
    mut receivers: Query<&mut MessageReceiver<crate::protocol::SpatialMessage>>,
    mut mirror: ResMut<Plans>,
    mut stations: ResMut<CraftStations>,
) {
    let mut decode = crate::spatial::SpatialDecodeRegistry::default();
    decode.register_reference("block", 1);
    decode.register_reference("item", 0);
    let active = HashSet::from_iter([ChunkCoord(IVec3::ZERO)]);
    for mut receiver in &mut receivers {
        for message in receiver.receive() {
            if let crate::protocol::SpatialMessage::Delta {
                dataset, payload, ..
            } = message
            {
                match dataset {
                    crate::spatial::DatasetId(1) => mirror
                        .apply_replica_delta(&payload, &decode, &active)
                        .unwrap(),
                    crate::spatial::DatasetId(4) => stations
                        .apply_replica_delta(&payload, &decode, &active)
                        .unwrap(),
                    _ => {}
                }
            }
        }
    }
}

#[derive(Resource, Default)]
struct NpcInspectionMirror(Vec<NpcDetails>);
fn receive_npc_mirror(
    mut receivers: Query<&mut MessageReceiver<NpcDetails>>,
    mut mirror: ResMut<NpcInspectionMirror>,
) {
    for mut receiver in &mut receivers {
        mirror.0.extend(receiver.receive());
    }
}

#[test]
fn udp_designate_deposit_repaint_cancel_and_complete() {
    use lightyear::prelude::client::{NetcodeClient, NetcodeConfig as ClientConfig};
    use lightyear::prelude::server::{
        NetcodeConfig as ServerConfig, NetcodeServer, ServerUdpIo, Start,
    };
    use ndshape::ConstShape;
    let tick = Duration::from_secs_f64(1.0 / 60.0);
    let mut server = App::new();
    server.add_plugins((
        MinimalPlugins,
        TransformPlugin,
        bevy::state::app::StatesPlugin,
    ));
    server.add_plugins(lightyear::prelude::server::ServerPlugins {
        tick_duration: tick,
    });
    server.add_plugins(crate::network::ProtocolPlugin);
    crate::configure_shared_schedule(&mut server);
    server
        .init_resource::<PlayerStates>()
        .init_resource::<SnapshotBudgets>();
    server.add_observer(forget_disconnected_client);
    server
        .init_resource::<RequestBudgets>()
        .init_resource::<SpatialSubscriptions>()
        .init_resource::<ClientAvatars>()
        .init_resource::<BlockWorkLeases>()
        .init_resource::<crate::plan_claims::PlanClaims>()
        .init_resource::<crate::haul::HaulStore>()
        .init_resource::<ChunkMap>()
        .add_message::<CellEdit>()
        .add_message::<NpcWorkCompleted>();
    let defs = serde_json::from_value(serde_json::json!([
        {"id":"test:wall", "display_name":"Wall", "flags":{"placeable":true,"solid":true}, "color":[1,1,1],
         "materials":[{"item":"test:stone","count":2}], "footprint":[[0,0,0],[1,0,0]]},
        {"id":"test:bench", "display_name":"Bench", "flags":{"placeable":true,"solid":true}, "color":[1,1,1], "station_tag":"test:craft", "station_tier":1,"facility_slots":[{"key":"seat","yaw":0,"approaches":[[1,0,0]],"exclusive":true,"interactions":["rest"]}],"facility_actions":{"rest":{"kind":"timed","duration_secs":0.02,"need_restore":{"need":"rest","restores":0.5}}}}
    ])).unwrap();
    server.insert_resource(BlockRegistry::build(defs).unwrap().0);
    server.insert_resource(crate::scripting::SelectedWorld(
        serde_json::from_value(serde_json::json!({
            "id":"test:world", "display_name":"Test world",
            "terrain":{"stone":"test:wall","dirt":"test:wall","grass":"test:wall","wood":"test:wall","leaves":"test:wall","gravel":"test:wall","forage":"test:wall"},
            "generation":block_junk_mod_api::worlds::GenerationConfig::default(),
            "player_spawn":[0,1,0], "movement_profile":"test:walker", "settlement":"test:settlement"
        }))
        .unwrap(),
    ));
    server.insert_resource(
        ItemRegistry::build(
            serde_json::from_value(serde_json::json!([
                {"id":"test:stone","display_name":"Stone","mesh":"unused.gltf"},
                {"id":"test:z_tool","display_name":"Tool","mesh":"unused.gltf","tool_tags":["mining"]}
            ]))
            .unwrap(),
        )
        .unwrap(),
    );
    let recipe_defs = serde_json::from_value(serde_json::json!([
        {"id":"test:fast", "display_name":"Fast", "inputs":[{"item":"test:stone","count":2}], "output":{"item":"test:stone","count":1}, "duration_secs":0.1, "station":"test:craft"},
        {"id":"test:slow", "display_name":"Slow", "inputs":[{"item":"test:stone","count":2}], "output":{"item":"test:stone","count":1}, "duration_secs":100.0, "station":"test:craft"}
    ])).unwrap();
    let recipes =
        RecipeRegistry::build(recipe_defs, server.world().resource::<ItemRegistry>()).unwrap();
    server.insert_resource(recipes);
    server.add_plugins(crate::craft_stations::CraftStationsServerPlugin);
    server.add_plugins(crate::plans::PlansServerPlugin);
    server.init_resource::<crate::placements::Placements>();
    crate::facility_inventory::install_fixture(&mut server);
    let mod_root = tempfile::tempdir().unwrap();
    let observer = mod_root.path().join("observer");
    std::fs::create_dir(&observer).unwrap();
    std::fs::write(
        observer.join("manifest.toml"),
        "id='observer'\nname='observer'\nversion='0.1.0'\napi_version='0.6.0'\n",
    )
    .unwrap();
    std::fs::write(observer.join("data.lua"), "engine.state.register(1,{})").unwrap();
    std::fs::write(
        observer.join("events.lua"),
        r#"
        engine.on_block_placed(function(event)
            if event.pos.x == 4 and event.pos.y == 1 and event.pos.z == 4 then
                engine.commands.push {kind='designate',cell={20,1,20},block='test:wall'}
                engine.commands.push {kind='set_state',scope='world',expected_revision=0,json='{}'}
                engine.commands.push {kind='cancel_project',project_id=999999,revision=1}
            end
        end)
    "#,
    )
    .unwrap();
    crate::scripting::install_command_fixture(&mut server, mod_root.path());
    install_interaction_fixture(&mut server);
    server
        .init_resource::<crate::npc::NpcEntityIndex>()
        .add_systems(Update, receive_npc_inspection_requests);
    server.add_systems(
        Update,
        (
            receive_deposit_requests,
            receive_pickup_requests,
            receive_drop_requests,
            receive_drop_tool_requests,
            receive_block_edits,
            apply_npc_work,
            auto_clear_stale_plans,
        )
            .chain()
            .in_set(GameSet::Simulation),
    );
    let mut chunk = Chunk {
        revision: 0,
        blocks: vec![BlockSlot::EMPTY; crate::voxel::ChunkShape::USIZE],
    };
    for x in 1..=32 {
        for z in 1..=32 {
            chunk.set(IVec3::new(x, 1, z), BlockSlot(1));
        }
    }
    let chunk_entity = server
        .world_mut()
        .spawn((chunk, ChunkEntities::default()))
        .id();
    server
        .world_mut()
        .resource_mut::<ChunkMap>()
        .0
        .insert(ChunkCoord(IVec3::ZERO), chunk_entity);
    let bind = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = bind.local_addr().unwrap();
    drop(bind);
    let key = [7; 32];
    let listener = server
        .world_mut()
        .spawn((
            NetcodeServer::new(ServerConfig {
                protocol_id: crate::network::NETCODE_PROTOCOL_ID,
                private_key: key,
                ..default()
            }),
            LocalAddr(address),
            ServerUdpIo::default(),
        ))
        .id();
    server.world_mut().trigger(Start { entity: listener });

    let mut client = App::new();
    client.add_plugins((
        MinimalPlugins,
        TransformPlugin,
        bevy::state::app::StatesPlugin,
    ));
    client.add_plugins(lightyear::prelude::client::ClientPlugins {
        tick_duration: tick,
    });
    client.add_plugins(crate::network::ProtocolPlugin);
    client
        .init_resource::<Plans>()
        .init_resource::<CraftStations>()
        .init_resource::<NpcInspectionMirror>()
        .add_systems(Update, (receive_plan_mirror, receive_npc_mirror));
    let netcode = NetcodeClient::new(
        Authentication::Manual {
            server_addr: address,
            client_id: 17,
            private_key: key,
            protocol_id: crate::network::NETCODE_PROTOCOL_ID,
        },
        ClientConfig::default(),
    )
    .unwrap();
    let link = client
        .world_mut()
        .spawn((
            Client::default(),
            LocalAddr("127.0.0.1:0".parse().unwrap()),
            PeerAddr(address),
            Link::new(None),
            ReplicationReceiver,
            PredictionManager::default(),
            netcode,
            UdpIo::default(),
        ))
        .id();
    client.world_mut().trigger(Connect { entity: link });
    server.finish();
    server.cleanup();
    client.finish();
    client.cleanup();
    pump(&mut server, &mut client, 100);
    assert!(
        client.world().get::<Connected>(link).is_some(),
        "UDP connection must establish"
    );
    let connection = server
        .world_mut()
        .query_filtered::<Entity, With<ClientOf>>()
        .single(server.world())
        .unwrap();
    server.world_mut().entity_mut(connection).insert((
        GameReady,
        AuthenticatedPlayer {
            player_id: 17,
            public_key: [0; 32],
            administrator: false,
        },
        ReplicationSender,
    ));
    let anchor = IVec3::new(4, 1, 4);
    let item = crate::items::ItemSlot(0);
    let avatar = server
        .world_mut()
        .spawn((
            Avatar,
            AvatarPose {
                translation: anchor.as_vec3() + Vec3::Y,
                yaw: 0.0,
            },
            Carrying {
                item: Some(item),
                count: 4,
            },
            EquippedTool::default(),
        ))
        .id();
    server
        .world_mut()
        .resource_mut::<ClientAvatars>()
        .0
        .insert(connection, avatar);
    let peer = server.world().get::<RemoteId>(connection).unwrap().0;
    server
        .world_mut()
        .resource_mut::<SpatialSubscriptions>()
        .chunk_to_clients
        .entry(ChunkCoord(IVec3::ZERO))
        .or_default()
        .insert(peer);
    let kind = PlanKind::Build {
        slot: BlockSlot(1),
        orientation: Cardinal::East,
    };
    let send_plan = |client: &mut App, kind| {
        client
            .world_mut()
            .get_mut::<MessageSender<PlanEdit>>(link)
            .unwrap()
            .send::<StateSyncChannel>(PlanEdit { cell: anchor, kind })
    };
    send_plan(&mut client, Some(kind));
    pump(&mut server, &mut client, 30);
    assert_eq!(
        client.world().resource::<Plans>().get(anchor),
        server.world().resource::<Plans>().get(anchor),
        "project identity and footprint replicate through spatial deltas"
    );
    let project_id = server
        .world()
        .resource::<Plans>()
        .get(anchor)
        .expect("UDP designation")
        .project_id;
    client
        .world_mut()
        .get_mut::<MessageSender<DepositRequest>>(link)
        .unwrap()
        .send::<WorldChannel>(DepositRequest { cell: anchor });
    pump(&mut server, &mut client, 30);
    assert_eq!(server.world().get::<Carrying>(avatar).unwrap().count, 2);
    assert!(
        server
            .world()
            .resource::<Plans>()
            .get(anchor)
            .unwrap()
            .is_satisfied()
    );
    send_plan(&mut client, Some(kind));
    pump(&mut server, &mut client, 30);
    assert_eq!(
        server
            .world()
            .resource::<Plans>()
            .get(anchor)
            .unwrap()
            .project_id,
        project_id
    );
    assert!(
        server
            .world()
            .resource::<Plans>()
            .get(anchor)
            .unwrap()
            .is_satisfied()
    );
    send_plan(&mut client, None);
    send_plan(&mut client, None);
    pump(&mut server, &mut client, 30);
    assert!(server.world().resource::<Plans>().get(anchor).is_none());
    let refunded: u32 = server
        .world_mut()
        .query::<&WorldItem>()
        .iter(server.world())
        .map(|i| i.count)
        .sum();
    assert_eq!(refunded, 2);
    send_plan(&mut client, Some(kind));
    pump(&mut server, &mut client, 30);
    client
        .world_mut()
        .get_mut::<MessageSender<DepositRequest>>(link)
        .unwrap()
        .send::<WorldChannel>(DepositRequest { cell: anchor });
    pump(&mut server, &mut client, 30);
    // Geometry can become unavailable after designation. A failed placement
    // must leave the paid project intact even after its work duration elapses.
    server
        .world_mut()
        .resource_mut::<ChunkMap>()
        .0
        .remove(&ChunkCoord(IVec3::ZERO));
    for sequence in 1..=60 {
        client
            .world_mut()
            .get_mut::<MessageSender<BlockWorkIntent>>(link)
            .unwrap()
            .send::<WorldChannel>(BlockWorkIntent {
                sequence,
                target: Some(BlockWorkTarget::Plan { cell: anchor }),
            });
        pump(&mut server, &mut client, 5);
    }
    assert!(
        server
            .world()
            .resource::<Plans>()
            .get(anchor)
            .unwrap()
            .is_satisfied(),
        "blocked completion retains escrow"
    );
    assert!(
        server
            .world()
            .resource::<Plans>()
            .get(anchor)
            .unwrap()
            .progress_secs
            > 0.0
    );
    assert!(
        server
            .world()
            .resource::<crate::construction::ConstructionLedger>()
            .pending_refunds
            .is_empty()
    );
    server
        .world_mut()
        .resource_mut::<ChunkMap>()
        .0
        .insert(ChunkCoord(IVec3::ZERO), chunk_entity);
    client
        .world_mut()
        .get_mut::<MessageSender<BlockWorkIntent>>(link)
        .unwrap()
        .send::<WorldChannel>(BlockWorkIntent {
            sequence: 61,
            target: Some(BlockWorkTarget::Plan { cell: anchor }),
        });
    pump(&mut server, &mut client, 30);
    assert!(
        server.world().resource::<Plans>().get(anchor).is_none(),
        "work completes"
    );
    let (_, local) = world_to_chunk(anchor);
    assert_eq!(
        server
            .world()
            .get::<Chunk>(chunk_entity)
            .unwrap()
            .get(local),
        BlockSlot(1)
    );
    let refunded: u32 = server
        .world_mut()
        .query::<&WorldItem>()
        .iter(server.world())
        .map(|i| i.count)
        .sum();
    assert_eq!(refunded, 2, "completed escrow must not refund");
    // Feed the same authoritative completion boundary with competing/stale
    // NPC results. Return the canceled project's loose stack to escrow first.
    let loose = server
        .world_mut()
        .query_filtered::<Entity, With<WorldItem>>()
        .single(server.world())
        .unwrap();
    server.world_mut().despawn(loose);
    let npc_cell = anchor + IVec3::Z * 3;
    let mut proposed = crate::protocol::PlanState::new(
        kind,
        vec![crate::protocol::MaterialEntry {
            item,
            needed: 2,
            present: 0,
        }],
    );
    proposed.footprint = vec![npc_cell, npc_cell + IVec3::X];
    server.world_mut().resource_scope(
        |world, mut ledger: Mut<crate::construction::ConstructionLedger>| {
            assert!(ledger.designate(&mut world.resource_mut::<Plans>(), npc_cell, proposed));
        },
    );
    assert_eq!(
        server
            .world_mut()
            .resource_mut::<Plans>()
            .deposit(npc_cell, item, 2),
        2
    );
    let token = server
        .world()
        .resource::<Plans>()
        .get(npc_cell)
        .unwrap()
        .token();
    let worker = NpcId(88);
    let worker_entity = server
        .world_mut()
        .spawn((
            Npc,
            worker,
            Carrying::default(),
            crate::npc::Needs(std::collections::HashMap::from([("work".into(), 1.)])),
            Brain {
                goal: Goal::Idle,
                rng: 1,
                home_facility: None,
                preempt_cooldown_secs: 0.,
                policy_results: Vec::new(),
                active_policy_job: Some((format!("construction:{}", token.0), token.1)),
                awaiting_construction: Some(token),
            },
            AvatarPose {
                translation: npc_cell.as_vec3() + Vec3::Y,
                yaw: 0.0,
            },
            EquippedTool::default(),
        ))
        .id();
    server
        .world_mut()
        .resource_mut::<crate::plan_claims::PlanClaims>()
        .try_claim(npc_cell, worker);
    server.world_mut().write_message(NpcWorkCompleted {
        cell: npc_cell,
        plan_kind: kind,
        need_restore: Some(block_junk_mod_api::blocks::NeedRestore {
            need: "work".into(),
            restores: 0.25,
        }),
        worker,
        project_token: (token.0 - 1, token.1),
    });
    pump(&mut server, &mut client, 2);
    assert!(
        server
            .world()
            .resource::<Plans>()
            .get(npc_cell)
            .unwrap()
            .is_satisfied()
    );
    assert_eq!(
        server
            .world()
            .resource::<crate::plan_claims::PlanClaims>()
            .holder(npc_cell),
        Some(worker),
        "a stale completion must not release current work"
    );
    server
        .world_mut()
        .resource_mut::<crate::plan_claims::PlanClaims>()
        .release(npc_cell, worker);
    server.world_mut().write_message(NpcWorkCompleted {
        cell: npc_cell,
        plan_kind: kind,
        need_restore: Some(block_junk_mod_api::blocks::NeedRestore {
            need: "work".into(),
            restores: 0.25,
        }),
        worker,
        project_token: token,
    });
    pump(&mut server, &mut client, 2);
    assert!(
        server.world().resource::<Plans>().get(npc_cell).is_some(),
        "unreserved worker cannot complete"
    );
    {
        let mut brain = server.world_mut().get_mut::<Brain>(worker_entity).unwrap();
        assert_eq!(brain.policy_results.len(), 1);
        assert_eq!(
            brain.policy_results[0].state,
            block_junk_mod_api::npcs::PolicyJobState::Failed
        );
        assert!(brain.awaiting_construction.is_none());
        brain.policy_results.clear();
        brain.active_policy_job = Some((format!("construction:{}", token.0), token.1));
        brain.awaiting_construction = Some(token);
    }
    assert_eq!(
        server.world().get::<Needs>(worker_entity).unwrap().0["work"],
        1.,
        "failed work grants no benefit"
    );
    let construction_cause = server
        .world_mut()
        .resource_mut::<crate::scripting::ServerMods>()
        .0
        .enqueue_event("construction_requested", &(), None)
        .unwrap();
    server
        .world()
        .resource::<crate::scripting::ServerMods>()
        .0
        .commands
        .batches
        .lock()
        .unwrap()
        .push_back(block_junk_scripting::commands::CommandBatch {
            id: 100,
            mod_id: "observer".into(),
            behavior: "construction-test".into(),
            cause: Some(construction_cause),
            commands: vec![block_junk_scripting::commands::Command::StartConstruction {
                project_id: token.0,
                revision: token.1,
                npc_id: worker.0,
            }],
        });
    pump(&mut server, &mut client, 2);
    assert_eq!(
        server
            .world()
            .resource::<crate::construction::ConstructionLedger>()
            .jobs
            .records[&1]
            .state,
        block_junk_mod_api::authority::JobState::Working
    );
    let mut completion_edits = server
        .world()
        .resource::<Messages<CellEdit>>()
        .get_cursor_current();
    for _ in 0..2 {
        server.world_mut().write_message(NpcWorkCompleted {
            cell: npc_cell,
            plan_kind: kind,
            need_restore: Some(block_junk_mod_api::blocks::NeedRestore {
                need: "work".into(),
                restores: 0.25,
            }),
            worker,
            project_token: token,
        });
    }
    pump(&mut server, &mut client, 2);
    assert_eq!(
        server
            .world()
            .resource::<crate::construction::ConstructionLedger>()
            .jobs
            .records[&1]
            .state,
        block_junk_mod_api::authority::JobState::Completed
    );
    assert!(
        completion_edits
            .read(server.world().resource::<Messages<CellEdit>>())
            .any(|edit| edit.world == npc_cell && edit.cause == Some(construction_cause)),
        "timed completion must preserve its event chain"
    );
    {
        let brain = server.world().get::<Brain>(worker_entity).unwrap();
        assert_eq!(
            brain.policy_results.len(),
            1,
            "duplicate completion produces one outcome"
        );
        assert_eq!(
            brain.policy_results[0].state,
            block_junk_mod_api::npcs::PolicyJobState::Completed
        );
        assert!(brain.awaiting_construction.is_none());
    }
    assert_eq!(
        server.world().get::<Needs>(worker_entity).unwrap().0["work"],
        0.75,
        "duplicate completion grants one benefit"
    );
    assert!(server.world().resource::<Plans>().get(npc_cell).is_none());
    assert!(
        server
            .world_mut()
            .query::<&WorldItem>()
            .iter(server.world())
            .next()
            .is_none(),
        "NPC completion consumes escrow exactly once"
    );
    // Craft requests share the transport and authoritative carry service.
    use crate::craft_stations::{
        CancelOrder, CraftBookings, DepositToStation, QueueOrder, WorkStart, WorkStop,
    };
    let station_cell = anchor + IVec3::X * 3;
    let (_, local) = world_to_chunk(station_cell);
    server
        .world_mut()
        .get_mut::<Chunk>(chunk_entity)
        .unwrap()
        .set(local, BlockSlot(2));
    *server.world_mut().get_mut::<Carrying>(avatar).unwrap() = Carrying {
        item: Some(item),
        count: 6,
    };
    client
        .world_mut()
        .get_mut::<MessageSender<DepositToStation>>(link)
        .unwrap()
        .send::<WorldChannel>(DepositToStation { station_cell });
    client
        .world_mut()
        .get_mut::<MessageSender<QueueOrder>>(link)
        .unwrap()
        .send::<WorldChannel>(QueueOrder {
            station_cell,
            recipe_id: "test:fast".into(),
            quantity: 1,
        });
    pump(&mut server, &mut client, 30);
    assert_eq!(
        server
            .world()
            .resource::<CraftStations>()
            .get(station_cell)
            .unwrap()
            .inventory[&item],
        6
    );
    for _ in 0..2 {
        client
            .world_mut()
            .get_mut::<MessageSender<WorkStart>>(link)
            .unwrap()
            .send::<WorldChannel>(WorkStart { station_cell });
    }
    pump(&mut server, &mut client, 60);
    let jobs = &server.world().resource::<CraftBookings>().jobs;
    assert!(jobs.records.values().any(|job| job.state
        == block_junk_mod_api::authority::JobState::Completed
        && job.actor == block_junk_mod_api::authority::ReservationOwner::Player { id: 17 }));
    pump(&mut server, &mut client, 150);
    let mirrored = client
        .world()
        .resource::<CraftStations>()
        .get(station_cell)
        .unwrap();
    assert_eq!(mirrored.inventory[&item], 4);
    assert!(mirrored.active_work.is_none());
    assert!(
        mirrored.orders.is_empty(),
        "committed completion must reach the subscribed client"
    );

    assert_eq!(
        server
            .world()
            .resource::<CraftStations>()
            .get(station_cell)
            .unwrap()
            .inventory[&item],
        4
    );
    assert!(
        server
            .world()
            .resource::<CraftStations>()
            .get(station_cell)
            .unwrap()
            .active_work
            .is_none()
    );
    assert_eq!(
        server
            .world_mut()
            .query::<&WorldItem>()
            .iter(server.world())
            .filter(|i| i.item == item)
            .map(|i| i.count)
            .sum::<u32>(),
        1,
        "duplicate starts cannot produce a second output"
    );
    client
        .world_mut()
        .get_mut::<MessageSender<QueueOrder>>(link)
        .unwrap()
        .send::<WorldChannel>(QueueOrder {
            station_cell,
            recipe_id: "test:slow".into(),
            quantity: 1,
        });
    pump(&mut server, &mut client, 20);
    client
        .world_mut()
        .get_mut::<MessageSender<WorkStart>>(link)
        .unwrap()
        .send::<WorldChannel>(WorkStart { station_cell });
    pump(&mut server, &mut client, 20);
    client
        .world_mut()
        .get_mut::<MessageSender<WorkStop>>(link)
        .unwrap()
        .send::<WorldChannel>(WorkStop { station_cell });
    pump(&mut server, &mut client, 20);
    let paused = server
        .world()
        .resource::<CraftStations>()
        .get(station_cell)
        .unwrap()
        .active_work
        .clone()
        .unwrap();
    assert_eq!(paused.committed_inputs, vec![(item, 2)]);
    pump(&mut server, &mut client, 20);
    assert_eq!(
        server
            .world()
            .resource::<CraftStations>()
            .get(station_cell)
            .unwrap()
            .active_work
            .as_ref()
            .unwrap(),
        &paused
    );
    let canceled_order = server
        .world()
        .resource::<CraftStations>()
        .get(station_cell)
        .unwrap()
        .orders
        .iter()
        .find(|order| order.recipe_id == "test:slow")
        .unwrap()
        .clone();
    for _ in 0..2 {
        client
            .world_mut()
            .get_mut::<MessageSender<CancelOrder>>(link)
            .unwrap()
            .send::<WorldChannel>(CancelOrder {
                station_cell,
                order_id: canceled_order.id,
                revision: canceled_order.revision,
            });
    }
    pump(&mut server, &mut client, 20);
    assert_eq!(
        server
            .world()
            .resource::<CraftStations>()
            .get(station_cell)
            .unwrap()
            .inventory[&item],
        4
    );
    assert!(
        server
            .world()
            .resource::<CraftStations>()
            .get(station_cell)
            .unwrap()
            .active_work
            .is_none()
    );
    // The mod action uses the same real facility placed through UDP above.
    // Inspect its committed need outcome through the normal authenticated RPC.
    let visitor = NpcId(99);
    let visitor_entity = server
        .world_mut()
        .spawn((
            Npc,
            visitor,
            NpcKind("test:visitor".into()),
            NpcStats(Default::default()),
            Needs(std::collections::HashMap::from_iter([("rest".into(), 1.)])),
            Brain {
                goal: Goal::Idle,
                rng: 1,
                home_facility: None,
                preempt_cooldown_secs: 0.,
                policy_results: Vec::new(),
                active_policy_job: None,
                awaiting_construction: None,
            },
            AvatarPose {
                translation: crate::physics::standing_pose_translation(station_cell + IVec3::X),
                yaw: 0.,
            },
            Carrying::default(),
            EquippedTool::default(),
        ))
        .id();
    server
        .world_mut()
        .resource_mut::<crate::npc::NpcEntityIndex>()
        .by_id
        .insert(visitor, visitor_entity);
    server
        .world()
        .resource::<crate::scripting::ServerMods>()
        .0
        .commands
        .batches
        .lock()
        .unwrap()
        .push_back(block_junk_scripting::commands::CommandBatch {
            id: 899,
            mod_id: "observer".into(),
            behavior: "visitor_policy".into(),
            cause: None,
            commands: vec![block_junk_scripting::commands::Command::ReadPlacement {
                cell: station_cell.to_array(),
            }],
        });
    pump(&mut server, &mut client, 3);
    let placement = server
        .world()
        .resource::<crate::placements::Placements>()
        .at(station_cell)
        .unwrap()
        .clone();
    let slot_id = block_junk_mod_api::architecture::UseSlotId {
        placement: placement.id,
        key: "seat".into(),
    };
    server
        .world_mut()
        .resource_mut::<crate::facility_access::FacilityAccess>()
        .slots
        .insert(
            (slot_id.clone(), "test:walker".into()),
            crate::facility_access::SlotAccess {
                id: slot_id,
                placement_revision: placement.revision,
                profile: "test:walker".into(),
                interactions: vec!["rest".into()],
                exclusive: true,
                approaches: vec![crate::facility_access::ApproachAccess {
                    cell: (station_cell + IVec3::X).to_array(),
                    status: crate::facility_access::AccessStatus::Usable,
                    obstruction: None,
                    reason: "fixture approach is supported".into(),
                }],
            },
        );
    server
        .world()
        .resource::<crate::scripting::ServerMods>()
        .0
        .commands
        .batches
        .lock()
        .unwrap()
        .push_back(block_junk_scripting::commands::CommandBatch {
            id: 900,
            mod_id: "observer".into(),
            behavior: "visitor_policy".into(),
            cause: None,
            commands: vec![block_junk_scripting::commands::Command::StartInteraction {
                placement_id: placement.id.0,
                revision: placement.revision,
                npc_id: visitor.0,
                slot_key: "seat".into(),
                action: "rest".into(),
                approach: (station_cell + IVec3::X).to_array(),
            }],
        });
    pump(&mut server, &mut client, 60);
    let jobs = &server
        .world()
        .resource::<crate::facility_interactions::FacilityInteractions>()
        .jobs;
    let completed = jobs.records.values().find(|job| {
        job.actor == block_junk_mod_api::authority::ReservationOwner::Npc { id: visitor.0 }
            && matches!(
                &job.target,
                block_junk_mod_api::authority::JobTarget::Interaction { action, .. }
                    if action == "rest"
            )
    });
    assert!(
        completed
            .is_some_and(|job| job.state == block_junk_mod_api::authority::JobState::Completed),
        "interaction jobs={:?}; command results={:?}; access={:?}",
        jobs.records,
        server
            .world()
            .resource::<crate::scripting::ServerMods>()
            .0
            .commands
            .results
            .lock()
            .unwrap(),
        server
            .world()
            .resource::<crate::facility_access::FacilityAccess>()
            .slots
    );
    client
        .world_mut()
        .get_mut::<MessageSender<RequestNpcDetails>>(link)
        .unwrap()
        .send::<WorldChannel>(RequestNpcDetails { npc_id: 99 });
    pump(&mut server, &mut client, 20);
    let replies = &client.world().resource::<NpcInspectionMirror>().0;
    assert!(
        replies
            .iter()
            .any(|reply| reply.npc_id == 99 && reply.needs.get("rest") == Some(&0.5)),
        "{replies:?}"
    );

    client
        .world_mut()
        .get_mut::<MessageSender<QueueOrder>>(link)
        .unwrap()
        .send::<WorldChannel>(QueueOrder {
            station_cell,
            recipe_id: "test:slow".into(),
            quantity: 1,
        });
    pump(&mut server, &mut client, 20);
    client
        .world_mut()
        .get_mut::<MessageSender<WorkStart>>(link)
        .unwrap()
        .send::<WorldChannel>(WorkStart { station_cell });
    pump(&mut server, &mut client, 20);
    // Player transfers use the same bounded delivery/conservation services.
    let tool = server
        .world()
        .resource::<ItemRegistry>()
        .slot_of(&"test:z_tool".into())
        .unwrap();
    let target = server
        .world()
        .get::<AvatarPose>(avatar)
        .unwrap()
        .translation;
    server.world_mut().spawn(WorldItem {
        item: tool,
        count: 3,
        translation: target,
    });
    for _ in 0..2 {
        client
            .world_mut()
            .get_mut::<MessageSender<PickupRequest>>(link)
            .unwrap()
            .send::<WorldChannel>(PickupRequest { target });
    }
    pump(&mut server, &mut client, 20);
    assert_eq!(
        server.world().get::<EquippedTool>(avatar).unwrap().item,
        Some(tool)
    );
    assert_eq!(
        server
            .world_mut()
            .query::<&WorldItem>()
            .iter(server.world())
            .filter(|pile| pile.item == tool)
            .map(|pile| pile.count)
            .sum::<u32>(),
        2
    );
    server.world_mut().get_mut::<Carrying>(avatar).unwrap().item = Some(item);
    server
        .world_mut()
        .get_mut::<Carrying>(avatar)
        .unwrap()
        .count = u32::MAX;
    let pending_before = server
        .world()
        .resource::<crate::item_placement::ItemPlacementQueue>()
        .0
        .len();
    for _ in 0..2 {
        client
            .world_mut()
            .get_mut::<MessageSender<DropRequest>>(link)
            .unwrap()
            .send::<WorldChannel>(DropRequest);
    }
    pump(&mut server, &mut client, 20);
    assert_eq!(server.world().get::<Carrying>(avatar).unwrap().count, 0);
    let pending = &server
        .world()
        .resource::<crate::item_placement::ItemPlacementQueue>()
        .0;
    assert_eq!(pending.len(), pending_before + 1);
    assert_eq!(pending.back().unwrap().count, u32::MAX);
    client.world_mut().trigger(Disconnect { entity: link });
    pump(&mut server, &mut client, 30);
    assert!(
        !server
            .world()
            .resource::<CraftBookings>()
            .has_worker(station_cell)
    );
    assert_eq!(
        server
            .world()
            .resource::<CraftStations>()
            .get(station_cell)
            .unwrap()
            .inventory[&item],
        2
    );
    assert_eq!(
        server
            .world()
            .resource::<CraftStations>()
            .get(station_cell)
            .unwrap()
            .active_work
            .as_ref()
            .unwrap()
            .committed_inputs,
        vec![(item, 2)]
    );
    assert!(server.world().resource::<BlockWorkLeases>().0.is_empty());
    assert!(server.world().resource::<ClientAvatars>().0.is_empty());
    assert_eq!(
        server.world().resource::<PlayerStates>().0[&17].carry.count,
        0
    );
    // No refund destination: move the remaining station stock into a project,
    // cancel with all geometry unloaded, then retry once its chunk returns.
    let amount = server
        .world_mut()
        .resource_mut::<CraftStations>()
        .edit(&station_cell, 0, |state| {
            state.inventory.remove(&item).unwrap()
        })
        .unwrap();
    let pending_cell = anchor + IVec3::Z * 6;
    let mut proposed = crate::protocol::PlanState::new(
        kind,
        vec![crate::protocol::MaterialEntry {
            item,
            needed: amount,
            present: 0,
        }],
    );
    proposed.footprint = vec![pending_cell, pending_cell + IVec3::X];
    server.world_mut().resource_scope(
        |world, mut ledger: Mut<crate::construction::ConstructionLedger>| {
            assert!(ledger.designate(&mut world.resource_mut::<Plans>(), pending_cell, proposed));
            assert_eq!(
                world
                    .resource_mut::<Plans>()
                    .deposit(pending_cell, item, amount),
                amount
            );
            assert!(ledger.cancel(&mut world.resource_mut::<Plans>(), pending_cell));
        },
    );
    server.world_mut().resource_mut::<ChunkMap>().0.clear();
    pump(&mut server, &mut client, 20);
    assert_eq!(
        server
            .world()
            .resource::<crate::construction::ConstructionLedger>()
            .pending_refunds[0]
            .items,
        vec![(item, amount)]
    );
    server
        .world_mut()
        .resource_mut::<ChunkMap>()
        .0
        .insert(ChunkCoord(IVec3::ZERO), chunk_entity);
    pump(&mut server, &mut client, 20);
    assert!(
        server
            .world()
            .resource::<crate::construction::ConstructionLedger>()
            .pending_refunds
            .is_empty()
    );
    assert_eq!(
        server
            .world_mut()
            .query::<&WorldItem>()
            .iter(server.world())
            .filter(|i| i.item == item)
            .map(|i| i.count)
            .sum::<u32>(),
        1 + amount
    );
    let mods = &server.world().resource::<crate::scripting::ServerMods>().0;
    let results = mods.commands.results.lock().unwrap();
    assert!(results["observer"].iter().any(|result| {
        !result.accepted
            && result
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("project missing"))
    }));
    assert!(
        mods.saved_state().is_empty(),
        "UDP-triggered rejected callback batch leaked state"
    );
    assert!(
        server
            .world()
            .resource::<Plans>()
            .get(IVec3::new(20, 1, 20))
            .is_none(),
        "UDP-triggered rejected callback batch leaked a project"
    );
    drop(results);
}
