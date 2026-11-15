//! Real content negotiation, including signature verification and selected preset.
use super::*;
use crate::protocol::CHUNK_PADDED;
use crate::spatial::SpatialDataset;
use crate::spatial_revision::RevisionDataset;

fn exercise(mismatch: bool) {
    use lightyear::prelude::server::{
        NetcodeConfig as ServerConfig, NetcodeServer, ServerUdpIo, Start,
    };
    let tick = Duration::from_secs_f64(1. / 60.);
    let key = [73; 32];
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    drop(socket);
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
    crate::scripting::install_independent_fixture(&mut server, block_junk_mod_api::Side::Server);
    server
        .init_resource::<crate::spatial::SpatialDatasetRegistry>()
        .insert_resource(crate::network::ServerCredentials {
            access: crate::network::ServerAccess::Open,
            netcode_key: key,
            administrators: Vec::new(),
        })
        .add_observer(send_server_hello)
        .add_observer(register_new_client)
        .add_systems(Update, verify_client_ready);
    server
        .init_resource::<crate::space_network::SpaceSenders>()
        .init_resource::<crate::capability_network::CapabilitySenders>()
        .init_resource::<crate::capabilities::Capabilities>()
        .init_resource::<crate::live_spaces::PublishedSpaces>()
        .init_resource::<crate::space_identity::SpaceIdentities>()
        .init_resource::<SnapshotBudgets>()
        .init_resource::<SpatialSubscriptions>()
        .init_resource::<ClientAvatars>()
        .init_resource::<PlayerStates>()
        .init_resource::<SpatialVisibilityState>()
        .add_systems(
            PostUpdate,
            update_spatial_visibility.before(ReplicationSystems::Send),
        )
        .add_systems(
            PostUpdate,
            (
                crate::space_network::publish,
                crate::capability_network::publish,
            ),
        );
    server
        .init_resource::<crate::placements::Placements>()
        .add_plugins(crate::placement_network::PlacementServerPlugin);
    let distant = Vec3::splat(2048.0);
    let stone = server
        .world()
        .resource::<ItemRegistry>()
        .slot_of(&"foundation:stone".into())
        .unwrap();
    server.world_mut().spawn((
        WorldItem {
            item: stone,
            count: 1,
            translation: distant,
        },
        crate::spatial::ordinary_spatial_replica(crate::spatial::SpatialScope::Point(distant)),
    ));
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

    let (mut client, link) = new_client(address, key, tick, mismatch);
    server.finish();
    server.cleanup();
    client.finish();
    client.cleanup();
    let mut items = client.world_mut().query::<&WorldItem>();
    for _ in 0..250 {
        server.update();
        client.update();
        assert!(items.iter(client.world()).next().is_none());
        std::thread::sleep(Duration::from_millis(2));
    }
    let ready = server
        .world_mut()
        .query::<&AuthenticatedPlayer>()
        .iter(server.world())
        .count();
    if mismatch {
        assert_eq!(ready, 0);
        assert!(
            client
                .world()
                .contains_resource::<crate::menu::ConnectionFatal>()
        );
    } else {
        assert_eq!(ready, 1);
        assert!(client.world().get::<GameReady>(link).is_some());
        assert_eq!(
            client
                .world()
                .resource::<crate::scripting::SelectedWorld>()
                .0
                .id,
            "foundation:plain"
        );
        assert_eq!(client.world().resource::<TerrainSlots>().generation.seed, 7);
    }
    if !mismatch {
        exercise_avatar_visibility(&mut server, &mut client);
        exercise_architecture(&mut server, &mut client);
        exercise_portals(&mut server, &mut client);
    }
    client.world_mut().trigger(Disconnect { entity: link });
    for _ in 0..40 {
        server.update();
        client.update();
        std::thread::sleep(Duration::from_millis(2));
    }
    if !mismatch {
        // A fresh client App reconnects with the same signing identity. Only
        // initial terrain is fixture data; architectural recovery uses the real
        // authenticated subscription publisher and UDP revision assembler.
        let expected: Vec<_> = server
            .world()
            .resource::<crate::live_spaces::PublishedSpaces>()
            .geometry
            .keys()
            .copied()
            .collect();
        let (mut reconnected, connection) = new_client(address, key, tick, false);
        reconnected.finish();
        reconnected.cleanup();
        for _ in 0..250 {
            server.update();
            reconnected.update();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(reconnected.world().get::<GameReady>(connection).is_some());
        let peer = server
            .world_mut()
            .query_filtered::<Entity, (With<GameReady>, With<Connected>)>()
            .single(server.world())
            .unwrap();
        let coord = ChunkCoord(IVec3::ZERO);
        let source = server.world().resource::<ChunkMap>().0[&coord];
        let chunk = server.world().get::<Chunk>(source).unwrap().clone();
        let revision = chunk.revision;
        let sidecar = server.world().get::<ChunkEntities>(source).unwrap().clone();
        let entity = reconnected.world_mut().spawn((chunk, sidecar)).id();
        reconnected
            .world_mut()
            .resource_mut::<ChunkMap>()
            .0
            .insert(coord, entity);
        {
            let mut published = server
                .world_mut()
                .resource_mut::<crate::live_spaces::PublishedSpaces>();
            for basis in published.basis.values_mut() {
                basis.insert([0, 0, 0], revision);
            }
            published.revision += 1;
        }
        server
            .world_mut()
            .resource_mut::<SpatialSubscriptions>()
            .client_to_chunks
            .entry(peer)
            .or_default()
            .insert(coord);
        for _ in 0..100 {
            server.update();
            reconnected.update();
            std::thread::sleep(Duration::from_millis(2));
        }
        let view = &reconnected
            .world()
            .resource::<crate::space_network::ClientSpaces>()
            .visible;
        assert_eq!(
            view.spaces
                .iter()
                .map(|space| crate::space_identity::SpaceId(space.id))
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            reconnected
                .world()
                .resource::<crate::scripting::SelectedWorld>()
                .0
                .id,
            "foundation:plain"
        );
        reconnected
            .world_mut()
            .trigger(Disconnect { entity: connection });
        for _ in 0..10 {
            server.update();
            reconnected.update();
        }
    }
}

fn exercise_avatar_visibility(server: &mut App, client: &mut App) {
    let (&connection, &avatar) = server
        .world()
        .resource::<ClientAvatars>()
        .0
        .iter()
        .next()
        .unwrap();
    let pose = server.world().get::<AvatarPose>(avatar).unwrap();
    let spawn_chunk = world_to_chunk_coord(pose.translation);
    let mut avatars = client.world_mut().query_filtered::<Entity, With<Avatar>>();
    assert_eq!(avatars.iter(client.world()).count(), 0);

    for subscribed in [true, false, true] {
        let mut subscriptions = server.world_mut().resource_mut::<SpatialSubscriptions>();
        let chunks = subscriptions.client_to_chunks.get_mut(&connection).unwrap();
        if subscribed {
            chunks.insert(spawn_chunk);
        } else {
            chunks.remove(&spawn_chunk);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            server.update();
            client.update();
            let entity = avatars.single(client.world()).ok();
            if let (true, Some(entity)) = (subscribed, entity) {
                assert!(client.world().get::<Predicted>(entity).is_some());
                assert!(client.world().get::<Interpolated>(entity).is_none());
                break;
            }
            if !subscribed && entity.is_none() {
                assert_eq!(avatars.iter(client.world()).count(), 0);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "avatar visibility did not follow the subscription"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn new_client(
    address: std::net::SocketAddr,
    key: [u8; 32],
    tick: Duration,
    mismatch: bool,
) -> (App, Entity) {
    use lightyear::prelude::client::{NetcodeClient, NetcodeConfig as ClientConfig};
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
    crate::scripting::install_independent_fixture(&mut client, block_junk_mod_api::Side::Client);
    client
        .init_resource::<crate::spatial::SpatialDatasetRegistry>()
        .init_resource::<crate::ui_capture::UiCaptures>();
    client
        .init_resource::<crate::voxel::ChunkMap>()
        .init_resource::<crate::space_network::ClientSpaces>()
        .init_resource::<crate::capability_network::ClientCapabilities>()
        .add_systems(Update, receive_architecture)
        .add_systems(
            PostUpdate,
            (
                crate::space_network::expose_ready_revision,
                crate::capability_network::expose_ready_revision,
                crate::space_network::request_replacement,
                crate::capability_network::request_replacement,
            )
                .chain(),
        );
    client.add_plugins(crate::placement_network::PlacementClientPlugin);
    let identity = crate::identity::fixture_identity([91; 32]);
    let id = identity.player_id();
    client.insert_resource(identity);
    crate::client::install_fixture_handshake(&mut client);
    // A joining client must take its selected preset from the handshake.
    client
        .world_mut()
        .resource_mut::<crate::scripting::SelectedWorld>()
        .0
        .id = "previous:world".into();
    if mismatch {
        client
            .world_mut()
            .resource_mut::<crate::scripting::WorldCatalog>()
            .base_fingerprint ^= 1;
    }
    let netcode = NetcodeClient::new(
        Authentication::Manual {
            server_addr: address,
            client_id: id,
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
    (client, link)
}

#[test]
fn independent_preset_negotiates_before_game_ready_over_udp() {
    exercise(false);
}

#[test]
fn mismatched_content_never_becomes_game_ready_over_udp() {
    exercise(true);
}

fn receive_architecture(
    mut receivers: Query<&mut MessageReceiver<crate::protocol::SpatialMessage>>,
    mut spaces: ResMut<crate::space_network::ClientSpaces>,
    mut capabilities: ResMut<crate::capability_network::ClientCapabilities>,
    mut chunks: Query<(&mut Chunk, &mut crate::voxel::ChunkEntities)>,
    map: Res<ChunkMap>,
    blocks: Res<BlockRegistry>,
    mut placements: ResMut<crate::placement_network::ClientPlacements>,
) {
    for mut receiver in &mut receivers {
        for message in receiver.receive() {
            match message {
                crate::protocol::SpatialMessage::Revision(frame) => {
                    if frame.dataset() == crate::space_network::SpaceDataset::ID {
                        spaces.receive(frame).unwrap();
                    } else if frame.dataset() == crate::capability_network::CapabilityDataset::ID {
                        capabilities.receive(frame).unwrap();
                    }
                }
                crate::protocol::SpatialMessage::Delta {
                    dataset,
                    schema_fingerprint,
                    payload,
                } if dataset == crate::spatial::TERRAIN_DATASET_ID => {
                    assert_eq!(
                        schema_fingerprint,
                        crate::spatial::TERRAIN_SCHEMA_FINGERPRINT
                    );
                    let (edit, used) =
                        bincode::serde::decode_from_slice(&payload, bincode::config::standard())
                            .unwrap();
                    assert_eq!(used, payload.len());
                    crate::client::apply_broadcast_edit(edit, &mut chunks, &map);
                }
                crate::protocol::SpatialMessage::Delta {
                    dataset,
                    schema_fingerprint,
                    payload,
                } if dataset == crate::placement_network::PlacementDataset::ID => {
                    assert_eq!(
                        schema_fingerprint,
                        crate::placement_network::PlacementDataset::SCHEMA_FINGERPRINT
                    );
                    let mut references = crate::spatial::SpatialDecodeRegistry::default();
                    for (slot, _) in blocks.iter() {
                        references.register_reference("block", slot.0 as u32);
                    }
                    placements
                        .apply_replica_delta(
                            &payload,
                            &references,
                            &map.0.keys().copied().collect(),
                        )
                        .unwrap();
                }
                _ => {}
            }
        }
    }
}
fn exercise_architecture(server: &mut App, client: &mut App) {
    use crate::{
        architecture_solver::{
            Cell,
            topology::{Boundary, Ceiling, Space, Volume},
        },
        space_identity::{Membership, SpaceDraft, SpaceIdentities},
    };
    use std::collections::{BTreeMap, BTreeSet};
    let peer = server
        .world_mut()
        .query_filtered::<Entity, With<GameReady>>()
        .single(server.world())
        .unwrap();
    let coord = ChunkCoord(IVec3::ZERO);
    server
        .world_mut()
        .resource_mut::<SpatialSubscriptions>()
        .client_to_chunks
        .entry(peer)
        .or_default()
        .insert(coord);
    let floor = BTreeSet::from([Cell(1, 1, 1)]);
    let (ids, _) = server
        .world_mut()
        .resource_mut::<SpaceIdentities>()
        .reconcile(
            &BTreeSet::new(),
            &[SpaceDraft {
                membership: Membership::from_cells([[1, 1, 1]]).unwrap(),
                content_fingerprint: [17; 32],
            }],
        )
        .unwrap();
    let id = ids[0];
    {
        let mut published = server
            .world_mut()
            .resource_mut::<crate::live_spaces::PublishedSpaces>();
        published.geometry.insert(
            id,
            Space {
                floor,
                roofs: BTreeMap::from([(Cell(1, 1, 1), Ceiling::At(Cell(1, 4, 1)))]),
                clearance: BTreeMap::from([(Cell(1, 1, 1), Ceiling::At(Cell(1, 4, 1)))]),
                volume: Volume::Bounded(2),
                boundary: Boundary::Closed,
                pending: false,
                placements: BTreeSet::new(),
            },
        );
        published.basis.insert(id, BTreeMap::from([([0, 0, 0], 7)]));
        published.revision = 1;
    }
    for _ in 0..100 {
        server.update();
        client.update();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        client
            .world()
            .resource::<crate::space_network::ClientSpaces>()
            .visible
            .spaces
            .is_empty(),
        "geometry must wait for its terrain basis"
    );
    let chunk = client
        .world_mut()
        .spawn(crate::voxel::Chunk {
            revision: 7,
            blocks: Vec::new(),
        })
        .id();
    client
        .world_mut()
        .resource_mut::<ChunkMap>()
        .0
        .insert(coord, chunk);
    client.update();
    assert_eq!(
        client
            .world()
            .resource::<crate::space_network::ClientSpaces>()
            .visible
            .spaces[0]
            .id,
        id.0
    );
    // A terrain revision can change while exact membership and facilities stay
    // identical. Its new basis still needs a committed spatial publication.
    client
        .world_mut()
        .get_mut::<crate::voxel::Chunk>(chunk)
        .unwrap()
        .revision = 8;
    client.update();
    assert!(
        client
            .world()
            .resource::<crate::space_network::ClientSpaces>()
            .waiting_for_terrain
    );
    {
        let mut published = server
            .world_mut()
            .resource_mut::<crate::live_spaces::PublishedSpaces>();
        published.basis.insert(id, BTreeMap::from([([0, 0, 0], 8)]));
        published.revision = 2;
    }
    for _ in 0..60 {
        server.update();
        client.update();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        !client
            .world()
            .resource::<crate::space_network::ClientSpaces>()
            .waiting_for_terrain
    );
    assert_eq!(
        client
            .world()
            .resource::<crate::space_network::ClientSpaces>()
            .visible
            .revision,
        2
    );
    let result = block_junk_mod_api::capabilities::CapabilityResult {
        id: "foundation:sleeping".into(),
        display_name: "Sleeping".into(),
        status: block_junk_mod_api::capabilities::CapabilityStatus::Supported,
        requirements: BTreeMap::new(),
        capacity: Some(block_junk_mod_api::capabilities::Evidence::number(
            "usable sleep slots".into(),
            1.,
        )),
        attributes: BTreeMap::new(),
    };
    {
        let mut capabilities = server
            .world_mut()
            .resource_mut::<crate::capabilities::Capabilities>();
        capabilities.snapshot.revision = 1;
        capabilities.snapshot.spatial_revision = 2;
        capabilities.snapshot.access_revision = 1;
        capabilities.snapshot.spaces.insert(
            id,
            crate::capabilities::SpaceCapabilities {
                profiles: BTreeMap::from([("foundation:humanoid".into(), vec![result])]),
            },
        );
    }
    for _ in 0..60 {
        server.update();
        client.update();
        std::thread::sleep(Duration::from_millis(2));
    }
    let replicated = &client
        .world()
        .resource::<crate::capability_network::ClientCapabilities>()
        .visible;
    assert_eq!(replicated.spatial_revision, 2);
    assert_eq!(replicated.spaces[0].0, id.0);
    assert_eq!(
        replicated.spaces[0].1.profiles["foundation:humanoid"][0]
            .capacity
            .as_ref()
            .unwrap()
            .value,
        Some(block_junk_mod_api::capabilities::Value::Number(1.))
    );
    // Recovery uses the production client request and authenticated publisher.
    client
        .world_mut()
        .resource_mut::<crate::space_network::ClientSpaces>()
        .replacement_requested = true;
    for _ in 0..60 {
        server.update();
        client.update();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        client
            .world()
            .resource::<crate::space_network::ClientSpaces>()
            .visible
            .spaces[0]
            .id,
        id.0
    );
    server
        .world_mut()
        .resource_mut::<SpatialSubscriptions>()
        .client_to_chunks
        .get_mut(&peer)
        .unwrap()
        .clear();
    for _ in 0..60 {
        server.update();
        client.update();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        client
            .world()
            .resource::<crate::space_network::ClientSpaces>()
            .visible
            .spaces
            .is_empty()
    );
    assert!(
        client
            .world()
            .resource::<crate::capability_network::ClientCapabilities>()
            .visible
            .spaces
            .is_empty()
    );
    assert!(server.world().resource::<SpaceIdentities>().records[&id].active);
}

#[derive(Resource, Default)]
struct PortalResponses(Vec<crate::portal_service::PortalResult>);
fn collect_portal_responses(
    mut receivers: Query<&mut MessageReceiver<crate::portal_service::PortalResult>>,
    mut responses: ResMut<PortalResponses>,
) {
    for mut receiver in &mut receivers {
        responses.0.extend(receiver.receive());
    }
}
fn exercise_portals(server: &mut App, client: &mut App) {
    use crate::portal_service::PortalRequest;
    use block_junk_mod_api::architecture::PlacementId;
    let peer = server
        .world_mut()
        .query_filtered::<Entity, With<GameReady>>()
        .single(server.world())
        .unwrap();
    let link = client
        .world_mut()
        .query_filtered::<Entity, With<Client>>()
        .single(client.world())
        .unwrap();
    server
        .init_resource::<crate::placements::Placements>()
        .init_resource::<crate::spatial_events::SpatialJournal>()
        .init_resource::<PendingTerrainEffects>()
        .init_resource::<ClientAvatars>()
        .init_resource::<RequestBudgets>()
        .init_resource::<ChunkMap>()
        .add_message::<CellEdit>()
        .add_systems(
            Update,
            (crate::portal_service::receive, flush_mod_terrain_effects).chain(),
        );
    client
        .init_resource::<PortalResponses>()
        .add_systems(Update, collect_portal_responses);
    let anchor = IVec3::new(4, 2, 4);
    let registry = server.world().resource::<BlockRegistry>();
    let mut terrain = crate::terrain_transaction::TerrainTransaction::default();
    terrain
        .stage(
            BlockEdit {
                anchor,
                slot: registry.require("foundation:door"),
                orientation: Cardinal::East,
            },
            registry,
            &|_| Some((BlockSlot::EMPTY, None, 0)),
        )
        .unwrap();
    let mut placements = crate::placements::Placements::default();
    placements
        .apply_effects(&terrain.effects, registry)
        .unwrap();
    let coord = ChunkCoord(IVec3::ZERO);
    let mut chunk = Chunk {
        revision: 1,
        blocks: vec![BlockSlot::EMPTY; (CHUNK_PADDED as usize).pow(3)],
    };
    let side = placements.sidecar_for(coord, registry);
    for (cell, slot, _) in terrain.changes() {
        chunk.apply_committed_cell(world_to_chunk(cell).1, slot);
    }
    server.insert_resource(placements);
    for app in [&mut *server, &mut *client] {
        let existing = app.world().resource::<ChunkMap>().0.get(&coord).copied();
        let entity = if let Some(entity) = existing {
            app.world_mut()
                .entity_mut(entity)
                .insert((chunk.clone(), side.clone()));
            entity
        } else {
            app.world_mut().spawn((chunk.clone(), side.clone())).id()
        };
        app.world_mut()
            .resource_mut::<ChunkMap>()
            .0
            .insert(coord, entity);
    }
    let avatar = server
        .world_mut()
        .spawn((
            crate::protocol::Actor,
            AvatarPose {
                translation: crate::physics::standing_pose_translation(anchor - IVec3::X),
                yaw: 0.,
            },
        ))
        .id();
    server
        .world_mut()
        .resource_mut::<ClientAvatars>()
        .0
        .insert(peer, avatar);
    server
        .world_mut()
        .resource_mut::<SpatialSubscriptions>()
        .client_to_chunks
        .entry(peer)
        .or_default()
        .insert(coord);
    let remote = server.world().get::<RemoteId>(peer).unwrap().0;
    {
        let mut subscriptions = server.world_mut().resource_mut::<SpatialSubscriptions>();
        subscriptions
            .chunk_to_clients
            .entry(coord)
            .or_default()
            .insert(remote);
        subscriptions.client_to_peer.insert(peer, remote);
    }
    let send = |client: &mut App, request_id, revision, open| {
        client
            .world_mut()
            .get_mut::<MessageSender<PortalRequest>>(link)
            .unwrap()
            .send::<WorldChannel>(PortalRequest {
                request_id,
                placement_id: 1,
                revision,
                key: crate::protocol::BoundedString::new("passage").unwrap(),
                open,
            })
    };
    let pump = |server: &mut App, client: &mut App| {
        for _ in 0..40 {
            server.update();
            client.update();
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    send(client, 1, 1, false);
    pump(server, client);
    assert_eq!(
        server
            .world()
            .resource::<crate::placements::Placements>()
            .records[&PlacementId(1)]
            .revision,
        2
    );
    let entity = client.world().resource::<ChunkMap>().0[&coord];
    assert!(matches!(
        client
            .world()
            .get::<crate::voxel::ChunkEntities>(entity)
            .unwrap()
            .get(anchor),
        Some(crate::voxel::EntryKind::Anchor {
            portal_open: Some(0),
            ..
        })
    ));
    assert_eq!(client.world().get::<Chunk>(entity).unwrap().revision, 2);
    assert_portal_traversal(server, anchor, true);
    assert_portal_traversal(client, anchor, true);
    let placement = client
        .world()
        .resource::<crate::placement_network::ClientPlacements>()
        .at(anchor + IVec3::Y)
        .unwrap();
    assert_eq!(
        (placement.id, placement.revision, placement.portal_open),
        (1, 2, Some(0))
    );
    send(client, 2, 1, false);
    pump(server, client);
    assert!(
        client
            .world()
            .resource::<PortalResponses>()
            .0
            .iter()
            .any(|result| result.request_id == 2 && result.revision.is_none())
    );
    assert_eq!(client.world().get::<Chunk>(entity).unwrap().revision, 2);
    send(client, 3, 2, true);
    pump(server, client);
    assert!(matches!(
        client
            .world()
            .get::<crate::voxel::ChunkEntities>(entity)
            .unwrap()
            .get(anchor),
        Some(crate::voxel::EntryKind::Anchor {
            portal_open: Some(1),
            ..
        })
    ));
    assert_portal_traversal(server, anchor, false);
    assert_portal_traversal(client, anchor, false);
    let placement = client
        .world()
        .resource::<crate::placement_network::ClientPlacements>()
        .at(anchor)
        .unwrap();
    assert_eq!(
        (placement.id, placement.revision, placement.portal_open),
        (1, 3, Some(1))
    );
    server
        .world_mut()
        .get_mut::<AvatarPose>(avatar)
        .unwrap()
        .translation = crate::physics::standing_pose_translation(anchor);
    send(client, 4, 3, false);
    pump(server, client);
    assert!(
        client
            .world()
            .resource::<PortalResponses>()
            .0
            .iter()
            .any(|result| result.request_id == 4
                && result
                    .reason
                    .as_ref()
                    .is_some_and(|reason| reason.contains("actor")))
    );
    assert_eq!(client.world().get::<Chunk>(entity).unwrap().revision, 3);
}

fn assert_portal_traversal(app: &mut App, anchor: IVec3, closed: bool) {
    use crate::pathfinding::Walkability;
    let mut state = bevy::ecs::system::SystemState::<(
        Query<(&Chunk, &crate::voxel::ChunkEntities)>,
        Query<&Chunk>,
        Query<&crate::voxel::ChunkEntities>,
        Res<ChunkMap>,
        Res<BlockRegistry>,
    )>::new(app.world_mut());
    let (collision_chunks, chunks, sidecars, map, registry) = state.get(app.world()).unwrap();
    let bounds = crate::collision::Aabb::from_centre_half(
        anchor.as_vec3() + Vec3::new(0.5, 0.9, 0.5),
        crate::physics::PLAYER_HALF_EXTENTS,
    );
    let physics = crate::collision::WorldCollision {
        chunks: &collision_chunks,
        chunk_map: &map,
        registry: &registry,
    };
    assert_eq!(
        physics
            .candidates(bounds)
            .iter()
            .any(|obstacle| obstacle.min.cmplt(bounds.max).all()
                && obstacle.max.cmpgt(bounds.min).all()),
        closed
    );
    let navigation = crate::npc::WorldWalk {
        chunks: &chunks,
        sidecars: &sidecars,
        chunk_map: &map,
        registry: &registry,
    };
    assert_eq!(navigation.blocks_body(anchor), closed);
    assert_eq!(navigation.blocks_body(anchor + IVec3::Y), closed);
}
