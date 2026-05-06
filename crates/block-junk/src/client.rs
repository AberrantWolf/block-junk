use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use lightyear::prelude::*;

use crate::camera::{FlyCam, FlyCamPlugin};
use crate::protocol::{
    Block, BlockEdit, ChunkCoord, ChunkData, ChunkSnapshot, ChunkUnload, GameSet, PlayerPosition,
    WorldChannel,
};
use crate::voxel::Chunk;

pub struct ClientPlugin;

impl Plugin for ClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(FlyCamPlugin)
            .add_plugins(crate::scripting::ClientScriptingPlugin)
            .init_resource::<ChunkMap>()
            .add_systems(Startup, setup_scene)
            .add_systems(
                Update,
                (place_break_input, send_player_position).in_set(GameSet::Input),
            )
            .add_systems(
                Update,
                (
                    receive_snapshots,
                    receive_block_edit_broadcasts,
                    receive_chunk_unloads,
                )
                    .in_set(GameSet::Simulation),
            )
            .add_systems(Update, mesh_chunks.in_set(GameSet::PostSimulation));
    }
}

/// Client-side chunk lookup, parallel to the server's. Filled by snapshot
/// receipt; consulted when applying broadcast edits or raycasting.
#[derive(Resource, Default)]
pub struct ChunkMap(pub HashMap<ChunkCoord, Entity>);

fn setup_scene(mut commands: Commands, mut ambient: ResMut<GlobalAmbientLight>) {
    // Default ambient (80) leaves shadowed faces near-black. Bumping it
    // floods all surfaces with enough light to read geometry.
    ambient.brightness = 250.0;

    // Camera + a point "headlamp" so the player can read shapes in the
    // shadow of nearby geometry without needing to fly around to find
    // a light angle that works.
    commands.spawn((
        Camera3d::default(),
        // Above the sine-wave terrain (peaks around y=16), looking down -Z.
        Transform::from_xyz(0.0, 32.0, 60.0),
        FlyCam::default(),
        PointLight {
            intensity: 750_000.0,
            range: 60.0,
            shadows_enabled: false,
            ..default()
        },
    ));

    // Two directional lights from opposite angles. The key light casts
    // shadows; the back light only fills (no shadow map) so it doesn't
    // create competing shadows that fight the key light's. The back light
    // is tinted slightly cool so the two sides of geometry read differently
    // even where they're both lit.
    for (rot, illuminance, shadows, color) in [
        (
            Quat::from_euler(EulerRot::XYZ, -0.8, 0.4, 0.0),
            10_000.0,
            true,
            Color::WHITE,
        ),
        (
            Quat::from_euler(EulerRot::XYZ, 0.5, 2.6, 0.0),
            3_000.0,
            false,
            Color::srgb(0.75, 0.85, 1.0),
        ),
    ] {
        commands.spawn((
            DirectionalLight {
                color,
                illuminance,
                shadows_enabled: shadows,
                ..default()
            },
            Transform::from_rotation(rot),
        ));
    }

    // Screen-centred crosshair: a fullscreen flex container with one tiny child.
    commands
        .spawn(Node {
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            position_type: PositionType::Absolute,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|parent| {
            parent.spawn((
                Node {
                    width: Val::Px(4.0),
                    height: Val::Px(4.0),
                    ..default()
                },
                BackgroundColor(Color::WHITE),
            ));
        });
}

/// Reach in world cells. Generous because the camera is a flying free-cam;
/// real survival reach (Minecraft-y ~5 blocks) lands when there's an avatar.
const RAYCAST_REACH: f32 = 256.0;

fn place_break_input(
    mouse: Res<ButtonInput<MouseButton>>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    cam: Query<&GlobalTransform, With<FlyCam>>,
    chunks: Query<&Chunk>,
    chunk_map: Res<ChunkMap>,
    mut sender: Query<&mut MessageSender<BlockEdit>>,
) {
    let break_click = mouse.just_pressed(MouseButton::Left);
    let place_click = mouse.just_pressed(MouseButton::Right);
    if !break_click && !place_click {
        return;
    }
    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    if !locked {
        return;
    }

    let Ok(cam_t) = cam.single() else {
        return;
    };
    // MessageSender lives on the connection entity; exactly one in any
    // non-server-only mode.
    let Ok(mut sender) = sender.single_mut() else {
        return;
    };
    let cam_pos = cam_t.translation();
    let cam_dir = *cam_t.forward();

    // World-space raycast: walk world cells, dispatch each to its owning
    // chunk via the ChunkMap. Avoids per-chunk iteration order dependence
    // and the padding-vs-interior ambiguity that plagued the per-chunk
    // approach.
    let get_block = |world: IVec3| -> Block {
        let (coord, local) = crate::voxel::world_to_chunk(world);
        chunk_map
            .0
            .get(&coord)
            .and_then(|&entity| chunks.get(entity).ok())
            .map(|chunk| chunk.get(local))
            .unwrap_or(Block::Empty)
    };
    let Some(hit) = crate::voxel::world_raycast(cam_pos, cam_dir, RAYCAST_REACH, get_block) else {
        return;
    };

    let world_target = if break_click {
        hit.hit
    } else {
        hit.hit + hit.face_normal
    };
    let (target_coord, target_local) = crate::voxel::world_to_chunk(world_target);
    let block = if break_click {
        Block::Empty
    } else {
        Block::Solid
    };
    sender.send::<WorldChannel>(BlockEdit {
        coord: target_coord,
        pos: target_local,
        block,
    });
}

/// Snapshot from server → spawn (or replace) the corresponding local chunk.
/// `ChunkData::Procedural` means "regenerate from the shared terrain
/// function locally" — server didn't ship the bytes because the chunk
/// has never been edited.
fn receive_snapshots(
    mut commands: Commands,
    mut receivers: Query<&mut MessageReceiver<ChunkSnapshot>>,
    mut chunks: Query<&mut Chunk>,
    mut map: ResMut<ChunkMap>,
) {
    for mut receiver in receivers.iter_mut() {
        for snapshot in receiver.receive() {
            let chunk = match snapshot.data {
                ChunkData::Procedural => Chunk::from_terrain(snapshot.coord),
                ChunkData::Edited(blocks) => Chunk { blocks },
            };
            match map.0.get(&snapshot.coord).copied() {
                Some(entity) => {
                    if let Ok(mut existing) = chunks.get_mut(entity) {
                        *existing = chunk;
                    }
                }
                None => {
                    let entity = commands
                        .spawn((
                            chunk,
                            snapshot.coord,
                            Name::new(format!("chunk{:?}", snapshot.coord.0.to_array())),
                            crate::voxel::chunk_world_transform(snapshot.coord),
                        ))
                        .id();
                    map.0.insert(snapshot.coord, entity);
                }
            }
        }
    }
}

/// Period (seconds) between player-position updates sent to the server.
/// 10 Hz is plenty for AoI streaming decisions and stays under 200 B/s.
const POSITION_SEND_PERIOD: f32 = 0.1;

fn send_player_position(
    time: Res<Time>,
    mut accum: Local<f32>,
    cam: Query<&GlobalTransform, With<FlyCam>>,
    mut sender: Query<&mut MessageSender<PlayerPosition>>,
) {
    *accum += time.delta_secs();
    if *accum < POSITION_SEND_PERIOD {
        return;
    }
    *accum = 0.0;

    let Ok(cam_t) = cam.single() else {
        return;
    };
    let Ok(mut sender) = sender.single_mut() else {
        return;
    };
    sender.send::<WorldChannel>(PlayerPosition(cam_t.translation()));
}

/// Server says a chunk has left our AoI: drop our local copy. The server
/// keeps its master record (so any edits we made aren't lost), and we'll
/// receive a fresh snapshot next time we walk back into range.
fn receive_chunk_unloads(
    mut commands: Commands,
    mut receivers: Query<&mut MessageReceiver<ChunkUnload>>,
    mut map: ResMut<ChunkMap>,
) {
    for mut receiver in receivers.iter_mut() {
        for unload in receiver.receive() {
            if let Some(entity) = map.0.remove(&unload.coord) {
                commands.entity(entity).despawn();
            }
        }
    }
}

/// Server broadcast of an applied edit → apply it to our local chunk so the
/// client view stays in sync.
fn receive_block_edit_broadcasts(
    mut receivers: Query<&mut MessageReceiver<BlockEdit>>,
    mut chunks: Query<&mut Chunk>,
    map: Res<ChunkMap>,
) {
    for mut receiver in receivers.iter_mut() {
        for edit in receiver.receive() {
            let Some(&entity) = map.0.get(&edit.coord) else {
                continue;
            };
            if let Ok(mut chunk) = chunks.get_mut(entity) {
                chunk.set(edit.pos, edit.block);
            }
        }
    }
}

fn mesh_chunks(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    chunks: Query<
        (
            Entity,
            &Chunk,
            Option<&MeshMaterial3d<StandardMaterial>>,
        ),
        Changed<Chunk>,
    >,
) {
    for (entity, chunk, material) in chunks.iter() {
        let Some(mesh) = chunk.build_mesh() else {
            continue;
        };
        let mesh_handle = meshes.add(mesh);
        let mut e = commands.entity(entity);
        e.insert(Mesh3d(mesh_handle));
        if material.is_none() {
            e.insert(MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgb(0.5, 0.7, 0.4),
                perceptual_roughness: 0.9,
                ..default()
            })));
        }
    }
}
