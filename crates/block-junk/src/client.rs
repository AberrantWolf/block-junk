use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

use crate::camera::{FlyCam, FlyCamPlugin};
use crate::protocol::{Block, BlockEdit, GameSet};
use crate::voxel::Chunk;

pub struct ClientPlugin;

impl Plugin for ClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(FlyCamPlugin)
            .add_plugins(crate::scripting::ClientScriptingPlugin)
            .add_systems(Startup, setup_scene)
            .add_systems(Update, place_break_input.in_set(GameSet::Input))
            .add_systems(Update, mesh_chunks.in_set(GameSet::PostSimulation));
    }
}

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
    chunks: Query<(Entity, &Chunk, &GlobalTransform)>,
    mut writer: MessageWriter<BlockEdit>,
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
    let cam_pos = cam_t.translation();
    let cam_dir = *cam_t.forward();

    for (entity, chunk, chunk_t) in chunks.iter() {
        let local_origin = cam_pos - chunk_t.translation();
        if let Some(hit) = chunk.raycast(local_origin, cam_dir, RAYCAST_REACH) {
            // Place at the cell adjacent to the hit face; break the hit cell
            // itself. Chunk::set silently rejects out-of-interior writes, so
            // a place click against the chunk's outer face is a no-op.
            let (pos, block) = if break_click {
                (hit.hit, Block::Empty)
            } else {
                (hit.place_cell(), Block::Solid)
            };
            writer.write(BlockEdit {
                chunk: entity,
                pos,
                block,
            });
            return;
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
