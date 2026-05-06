use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use lightyear::prelude::*;

use crate::camera::{FlyCam, FlyCamPlugin};
use crate::protocol::{Block, BlockEdit, ChunkCoord, ChunkSnapshot, GameSet, WorldChannel};
use crate::voxel::Chunk;

pub struct ClientPlugin;

impl Plugin for ClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(FlyCamPlugin)
            .add_plugins(crate::scripting::ClientScriptingPlugin)
            .init_resource::<ChunkMap>()
            .add_systems(Startup, setup_scene)
            .add_systems(Update, place_break_input.in_set(GameSet::Input))
            .add_systems(
                Update,
                (receive_snapshots, receive_block_edit_broadcasts).in_set(GameSet::Simulation),
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
        Transform::from_xyz(17.0, 17.0, 80.0),
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

const RAYCAST_REACH: f32 = 100.0;

fn place_break_input(
    mouse: Res<ButtonInput<MouseButton>>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    cam: Query<&GlobalTransform, With<FlyCam>>,
    chunks: Query<(&Chunk, &ChunkCoord, &GlobalTransform)>,
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
    // The MessageSender lives on the connection entity (host-client or
    // netcode client). There's exactly one in any non-server-only mode.
    let Ok(mut sender) = sender.single_mut() else {
        return;
    };
    let cam_pos = cam_t.translation();
    let cam_dir = *cam_t.forward();

    for (chunk, coord, chunk_t) in chunks.iter() {
        let local_origin = cam_pos - chunk_t.translation();
        if let Some(hit) = chunk.raycast(local_origin, cam_dir, RAYCAST_REACH) {
            // Place at the cell adjacent to the hit face; break the hit cell
            // itself. Server-side Chunk::set rejects out-of-interior writes,
            // so a place click against the chunk's outer face becomes a no-op.
            let (pos, block) = if break_click {
                (hit.hit, Block::Empty)
            } else {
                (hit.place_cell(), Block::Solid)
            };
            sender.send::<WorldChannel>(BlockEdit {
                coord: *coord,
                pos,
                block,
            });
            return;
        }
    }
}

/// Snapshot from server → spawn (or replace) the corresponding local chunk.
fn receive_snapshots(
    mut commands: Commands,
    mut receivers: Query<&mut MessageReceiver<ChunkSnapshot>>,
    mut chunks: Query<&mut Chunk>,
    mut map: ResMut<ChunkMap>,
) {
    for mut receiver in receivers.iter_mut() {
        for snapshot in receiver.receive() {
            match map.0.get(&snapshot.coord).copied() {
                Some(entity) => {
                    if let Ok(mut chunk) = chunks.get_mut(entity) {
                        chunk.blocks = snapshot.blocks;
                    }
                }
                None => {
                    let entity = commands
                        .spawn((
                            Chunk {
                                blocks: snapshot.blocks,
                            },
                            snapshot.coord,
                            Name::new(format!("chunk{:?}", snapshot.coord.0.to_array())),
                            Transform::default(),
                        ))
                        .id();
                    map.0.insert(snapshot.coord, entity);
                }
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
