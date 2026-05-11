use bevy::input::mouse::AccumulatedMouseScroll;
use bevy::platform::collections::{HashMap, HashSet};
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use block_junk_mod_api::blocks::Cardinal;
use lightyear::prelude::*;

use bevy::scene::SceneInstanceReady;

use lightyear::input::native::prelude::*;

use crate::blocks::{BlockRegistry, BlockSlot, TerrainSlots};
use crate::camera::{FlyCam, FlyCamPlugin};
use crate::collision::WorldCollision;
use crate::physics::apply_walk_step;
use crate::preview::{PreviewBack, PreviewFront, PreviewPlugin};
use crate::protocol::{
    Avatar, AvatarOnGround, AvatarPose, AvatarVelocity, BlockEdit, BlockManifest, ChunkCoord,
    ChunkData, ChunkSnapshot, ChunkUnload, GameSet, MovementMode, PlayerInput, WorldChannel,
};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap, EntryKind};

pub struct ClientPlugin;

impl Plugin for ClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(FlyCamPlugin)
            .add_plugins(PreviewPlugin)
            .add_plugins(crate::scripting::ClientScriptingPlugin);
        // ClientScriptingPlugin inserts BlockRegistry. Derive client-side
        // resources from it.
        let palette = {
            let reg = app.world().resource::<BlockRegistry>();
            PlaceablePalette(reg.iter_placeable().collect())
        };
        let terrain_slots = TerrainSlots::from_registry(app.world().resource::<BlockRegistry>());
        app.insert_resource(palette);
        app.insert_resource(terrain_slots);
        app.init_resource::<ChunkMap>()
            .init_resource::<SelectedBlock>()
            .init_resource::<PlacementRotation>()
            .init_resource::<BlockEntities>()
            .init_resource::<PreviewState>()
            .add_observer(swap_preview_scene_materials)
            .add_systems(Startup, (setup_scene, setup_placement_preview))
            .add_systems(
                Update,
                (
                    place_break_input,
                    cycle_selected_or_rotation,
                    reset_rotation_on_selection_change,
                )
                    .in_set(GameSet::Input),
            )
            // Input replication: ActionState<PlayerInput> on the predicted
            // avatar gets written from the keyboard each fixed tick.
            // WriteClientInputs is the lightyear-defined set that ensures
            // the input is buffered before the simulation reads it.
            .add_systems(
                FixedPreUpdate,
                buffer_input.in_set(client::input::InputSystems::WriteClientInputs),
            )
            // Owner-side prediction: run the same controller the server
            // runs, against the same inputs, so we don't wait for the
            // server's reply to see ourselves move. Lightyear rolls back
            // and replays this when it receives a server correction.
            .add_systems(FixedUpdate, client_player_step)
            // Chained: receive_snapshots inserts new chunks via Commands;
            // receive_block_edit_broadcasts queries those chunks. Without
            // a sync point between them an edit landing in the same tick
            // as its chunk snapshot can fall through (chunk not yet in
            // the world). `.chain()` inserts the apply_deferred between
            // each pair, which is overkill for the avatar/manifest
            // systems but cheap.
            .add_systems(
                Update,
                (
                    receive_block_manifest,
                    receive_snapshots,
                    receive_block_edit_broadcasts,
                    receive_chunk_unloads,
                    sync_avatar_transforms,
                )
                    .chain()
                    .in_set(GameSet::Simulation),
            )
            .add_systems(
                Update,
                (
                    mesh_chunks,
                    refresh_block_entities,
                    update_hotbar_highlight,
                    update_placement_preview,
                    attach_avatar_visuals,
                )
                    .in_set(GameSet::PostSimulation),
            )
            // The owner's predicted avatar arrives via replication after
            // connect; this observer wires its camera, input marker, and
            // headlamp once it's there.
            .add_observer(handle_predicted_spawn);
    }
}

/// Pre-built mesh + material for remote-player avatars. Built once during
/// `setup_scene` so every replicated avatar shares the same handles instead
/// of allocating new GPU resources per spawn.
#[derive(Resource)]
struct AvatarAssets {
    mesh: Handle<Mesh>,
    material: Handle<StandardMaterial>,
}

/// Tracks the ECS entity rendering each placed block-entity (a block
/// whose `BlockDef.mesh` is set, e.g. furniture, doors). Indexed by world
/// cell with a parallel per-chunk set so we can despawn an entire chunk's
/// block entities cheaply on `ChunkUnload`.
#[derive(Resource, Default)]
pub struct BlockEntities {
    by_cell: HashMap<IVec3, Entity>,
    by_chunk: HashMap<ChunkCoord, HashSet<IVec3>>,
}

/// Cached list of placeable blocks for hotbar / cycling. Built once at
/// startup from `BlockRegistry::iter_placeable`. If/when mods can add
/// blocks at runtime this will need invalidation; for now it's static.
#[derive(Resource)]
pub struct PlaceablePalette(pub Vec<BlockSlot>);

/// Index into [`PlaceablePalette`] of the currently selected block. Mouse
/// wheel cycles; right-click places.
#[derive(Resource, Default)]
pub struct SelectedBlock(pub usize);

impl SelectedBlock {
    pub fn current(&self, palette: &PlaceablePalette) -> BlockSlot {
        palette.0[self.0]
    }
}

/// Manual orientation offset applied on top of the player's facing-derived
/// orientation at place time. Ctrl+MouseWheel advances/retreats one
/// cardinal step. Reset to the default ([`Cardinal::East`]) whenever the
/// hotbar selection changes — orientation context is per-item, so picking
/// a new item shouldn't carry forward the previous item's rotation.
#[derive(Resource, Default)]
pub struct PlacementRotation(pub Cardinal);

#[derive(Component)]
struct HotbarSlot(usize);

/// Root of the cube-style preview (used when the selected block is a
/// plain voxel block — no glTF mesh). Holds the world Transform and
/// Visibility; child entities carry the front+back-pass mesh draws.
#[derive(Component)]
struct PreviewCubeRoot;

/// Marker on the SceneRoot entity used when the selected block has a
/// glTF mesh. Spawned lazily when a mesh block is selected; despawned
/// when the slot changes back to non-mesh or to a different mesh slot.
#[derive(Component)]
struct PreviewSceneRoot;

/// Set on a `PreviewSceneRoot` after we've finished walking its
/// descendants and replaced their materials with our preview pair. Until
/// this marker is present the scene is kept hidden — we don't want the
/// player to see one frame of the bed at full opacity with original
/// materials before the swap completes.
#[derive(Component)]
struct PreviewSceneReady;

/// Shared material handles for every preview draw. Two materials
/// (front, back) get re-tinted each frame from the selected block's
/// swatch + a validity flag, so a single pair covers every block. The
/// cube mesh is held alive via the cube preview's `Mesh3d` child
/// entities, no separate handle needed here.
#[derive(Resource)]
struct PreviewMaterials {
    front: Handle<PreviewFront>,
    back: Handle<PreviewBack>,
}

/// Live state for the preview pipeline. `cube_root` is spawned once at
/// startup; `scene_root` is spawned/despawned lazily as the player
/// cycles between mesh and non-mesh selections.
#[derive(Resource, Default)]
struct PreviewState {
    cube_root: Option<Entity>,
    scene_root: Option<Entity>,
    /// Slot the current `scene_root` was spawned for. When the player
    /// switches to a different mesh block we have to despawn + respawn
    /// to load the new glTF.
    scene_slot: Option<BlockSlot>,
}

fn setup_scene(
    mut commands: Commands,
    mut ambient: ResMut<GlobalAmbientLight>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    palette: Res<PlaceablePalette>,
    registry: Res<BlockRegistry>,
) {
    // Default ambient (80) leaves shadowed faces near-black. Bumping it
    // floods all surfaces with enough light to read geometry.
    ambient.brightness = 250.0;

    // Single shared cuboid + material for all remote-player avatars. Roughly
    // Minecraft proportions (0.6×1.8×0.6 m), centred on the avatar's
    // Transform so the world position matches the camera-eye height that
    // the owner reports to the server.
    commands.insert_resource(AvatarAssets {
        mesh: meshes.add(Cuboid::new(0.6, 1.8, 0.6)),
        material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.95, 0.55, 0.25),
            perceptual_roughness: 0.6,
            ..default()
        }),
    });


    // Camera + a point "headlamp" so the player can read shapes in the
    // shadow of nearby geometry without needing to fly around to find
    // a light angle that works.
    // The camera is no longer a free-floating local entity — it's
    // attached to the predicted avatar via `handle_predicted_spawn` once
    // the server replicates it. Until then (a few ms in solo mode, up
    // to ~200 ms over the network) the screen has no active 3D camera.

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

    // Screen-centred crosshair.
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

    // Hotbar on the right edge: vertical column of slots. Selected slot
    // gets a white border via update_hotbar_highlight.
    commands
        .spawn(Node {
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            position_type: PositionType::Absolute,
            justify_content: JustifyContent::FlexEnd,
            align_items: AlignItems::Center,
            padding: UiRect::right(Val::Px(20.0)),
            ..default()
        })
        .with_children(|root| {
            root.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(4.0),
                ..default()
            })
            .with_children(|row| {
                for (i, &slot) in palette.0.iter().enumerate() {
                    let [r, g, b] = registry.def(slot).color;
                    row.spawn((
                        Node {
                            width: Val::Px(44.0),
                            height: Val::Px(44.0),
                            border: UiRect::all(Val::Px(2.0)),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            ..default()
                        },
                        BorderColor::all(Color::BLACK),
                        BackgroundColor(Color::srgba(0.1, 0.1, 0.1, 0.6)),
                        HotbarSlot(i),
                    ))
                    .with_children(|slot| {
                        slot.spawn((
                            Node {
                                width: Val::Px(32.0),
                                height: Val::Px(32.0),
                                ..default()
                            },
                            BackgroundColor(Color::srgb(r, g, b)),
                        ));
                    });
                }
            });
        });
}

/// One scroll handler covers both jobs because Ctrl gates which one fires:
///   - Plain wheel cycles the selected block in the hotbar.
///   - Ctrl+wheel rotates the manual placement orientation 90° per click.
/// We keep them in one system so the wheel never double-fires (rotating
/// AND cycling) on a frame where the modifier flips mid-scroll.
fn cycle_selected_or_rotation(
    scroll: Res<AccumulatedMouseScroll>,
    keys: Res<ButtonInput<KeyCode>>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    mut selected: ResMut<SelectedBlock>,
    mut rotation: ResMut<PlacementRotation>,
    palette: Res<PlaceablePalette>,
) {
    // Don't steal scrolls from menus / unlocked cursor states.
    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    if !locked {
        return;
    }
    let dy = scroll.delta.y;
    if dy.abs() < 0.5 {
        return;
    }
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    if ctrl {
        // CCW step on scroll-up matches the right-hand rule for +Y rotation
        // (positive yaw is CCW viewed from above) — rotating "up the wheel"
        // turns the bed's head left, which is the natural feel.
        let step = if dy > 0.0 { 1 } else { -1 };
        rotation.0 = rotation.0.rotated(step);
        return;
    }
    let n = palette.0.len();
    if n == 0 {
        return;
    }
    // Hotbar is laid out top→bottom (index 0 at top). Scroll up moves the
    // highlight to the slot *above* the current one, i.e. toward index 0.
    if dy > 0.0 {
        selected.0 = (selected.0 + n - 1) % n;
    } else {
        selected.0 = (selected.0 + 1) % n;
    }
}

/// Snap rotation back to the default whenever the selected block changes,
/// so the user never gets a "why is this rotated weird" surprise after
/// switching items in the hotbar. Bevy's resource change-detection tick
/// makes this a one-liner.
fn reset_rotation_on_selection_change(
    selected: Res<SelectedBlock>,
    mut rotation: ResMut<PlacementRotation>,
) {
    if selected.is_changed() {
        rotation.0 = Cardinal::default();
    }
}

fn update_hotbar_highlight(
    selected: Res<SelectedBlock>,
    mut slots: Query<(&HotbarSlot, &mut BorderColor)>,
) {
    if !selected.is_changed() {
        return;
    }
    for (slot, mut border) in slots.iter_mut() {
        *border = if slot.0 == selected.0 {
            BorderColor::all(Color::WHITE)
        } else {
            BorderColor::all(Color::BLACK)
        };
    }
}

/// Reach in world cells. Generous because the camera is a flying free-cam;
/// real survival reach (Minecraft-y ~5 blocks) lands when there's an avatar.
const RAYCAST_REACH: f32 = 256.0;

/// Convenience: compose the player's facing-derived orientation with the
/// manual rotation offset to get the orientation a place action would use.
fn placement_orientation(player_yaw: f32, manual: Cardinal) -> Cardinal {
    Cardinal::from_yaw_facing(player_yaw).rotated(manual as i32)
}

/// Resolve a default-orientation footprint into world cells given an
/// anchor cell and the current orientation. Single-cell footprints fall
/// out trivially as `[anchor]`; multi-cell ones get rotated.
fn world_footprint(anchor: IVec3, def_footprint: &[[i32; 3]], orientation: Cardinal) -> Vec<IVec3> {
    def_footprint
        .iter()
        .map(|&offset| anchor + IVec3::from_array(orientation.rotate_offset(offset)))
        .collect()
}

fn place_break_input(
    mouse: Res<ButtonInput<MouseButton>>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    cam: Query<(&GlobalTransform, &FlyCam)>,
    chunks: Query<(&Chunk, &ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    selected: Res<SelectedBlock>,
    palette: Res<PlaceablePalette>,
    rotation: Res<PlacementRotation>,
    registry: Res<BlockRegistry>,
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

    let Ok((cam_t, fly)) = cam.single() else {
        return;
    };
    // MessageSender lives on the connection entity; exactly one in any
    // non-server-only mode.
    let Ok(mut sender) = sender.single_mut() else {
        return;
    };
    let cam_pos = cam_t.translation();
    let cam_dir = *cam_t.forward();

    let get_block = |world: IVec3| -> BlockSlot {
        let (coord, local) = crate::voxel::world_to_chunk(world);
        chunk_map
            .0
            .get(&coord)
            .and_then(|&entity| chunks.get(entity).ok())
            .map(|(chunk, _)| chunk.get(local))
            .unwrap_or(BlockSlot::EMPTY)
    };
    let Some(hit) = entity_aware_raycast(
        cam_pos,
        cam_dir,
        RAYCAST_REACH,
        &chunks,
        &chunk_map,
        &registry,
    ) else {
        return;
    };

    if break_click {
        // Server resolves anchor + footprint via the chunk sidecar.
        // Orientation is irrelevant on a break request; default is fine.
        let _ = get_block; // consumed via the raycast
        sender.send::<WorldChannel>(BlockEdit {
            anchor: hit.cell,
            slot: BlockSlot::EMPTY,
            orientation: Cardinal::default(),
        });
    } else {
        let anchor = hit.cell + hit.face_normal;
        let slot = selected.current(&palette);
        let orientation = placement_orientation(fly.yaw, rotation.0);
        sender.send::<WorldChannel>(BlockEdit {
            anchor,
            slot,
            orientation,
        });
    }
}

/// Raycast hit for the place/break path. `cell` is the world cell that
/// would receive the action: for break, the cell whose block should be
/// affected; for place, the cell adjacent to the hit face.
struct EntityAwareHit {
    cell: IVec3,
    face_normal: IVec3,
}

/// Walks world cells like the plain voxel raycast, but treats block-entity
/// cells specially: when the ray enters an entity cell, AABB-test against
/// the entity's declared (rotated) bounds. On miss, keep stepping past so
/// the ray "sees through" the airspace inside a partial-cell mesh and can
/// land on whatever is behind it. On hit, return the entity cell.
///
/// For non-entity cells the behaviour is identical to `world_raycast`.
fn entity_aware_raycast(
    origin: Vec3,
    dir: Vec3,
    max_distance: f32,
    chunks: &Query<(&Chunk, &ChunkEntities)>,
    chunk_map: &ChunkMap,
    registry: &BlockRegistry,
) -> Option<EntityAwareHit> {
    let lookup = |world: IVec3| -> (BlockSlot, Option<EntryKind>) {
        let (coord, local) = crate::voxel::world_to_chunk(world);
        let Some(&entity) = chunk_map.0.get(&coord) else {
            return (BlockSlot::EMPTY, None);
        };
        let Ok((chunk, entities)) = chunks.get(entity) else {
            return (BlockSlot::EMPTY, None);
        };
        (chunk.get(local), entities.get(world))
    };

    // Two-pass: first find the nearest cell whose block-entity AABB
    // genuinely contains the ray, OR a non-entity solid cell (cube AABB).
    // Reuse the cube-stepping core; for each non-empty cell, decide
    // whether to accept based on entity kind + AABB test.
    let mut cell = origin.floor().as_ivec3();
    let mut entered_face = IVec3::ZERO;

    let (slot, kind) = lookup(cell);
    if !slot.is_empty()
        && cell_passes_test(
            origin,
            dir,
            cell,
            slot,
            kind,
            registry,
            chunks,
            chunk_map,
            max_distance,
        )
    {
        // Origin already inside a hit-tested entity / non-entity solid.
        return Some(EntityAwareHit {
            cell,
            face_normal: -entered_face,
        });
    }

    let step = dir.signum().as_ivec3();
    let next = cell.as_vec3() + dir.signum().max(Vec3::ZERO);
    let mut t_max = Vec3::select(
        dir.cmpeq(Vec3::ZERO),
        Vec3::INFINITY,
        (next - origin) / dir,
    );
    let t_delta = dir.abs().recip();

    loop {
        let axis = if t_max.x <= t_max.y && t_max.x <= t_max.z {
            0
        } else if t_max.y <= t_max.z {
            1
        } else {
            2
        };
        let t = t_max[axis];
        if t > max_distance {
            return None;
        }
        cell[axis] += step[axis];
        entered_face = IVec3::ZERO;
        entered_face[axis] = step[axis];
        let _ = t;
        t_max[axis] += t_delta[axis];

        let (slot, kind) = lookup(cell);
        if slot.is_empty() {
            continue;
        }
        if cell_passes_test(
            origin,
            dir,
            cell,
            slot,
            kind,
            registry,
            chunks,
            chunk_map,
            max_distance,
        ) {
            return Some(EntityAwareHit {
                cell,
                face_normal: -entered_face,
            });
        }
    }
}

/// Decide whether a non-empty cell counts as a hit. Plain solid blocks
/// always do. Block-entity cells (anchor or ghost) defer to the entity's
/// rotated AABB so the ray walks past airspace inside a partial mesh.
#[allow(clippy::too_many_arguments, reason = "raycast helper is naturally chunky")]
fn cell_passes_test(
    origin: Vec3,
    dir: Vec3,
    cell: IVec3,
    slot: BlockSlot,
    kind: Option<EntryKind>,
    registry: &BlockRegistry,
    chunks: &Query<(&Chunk, &ChunkEntities)>,
    chunk_map: &ChunkMap,
    max_distance: f32,
) -> bool {
    let def = registry.def(slot);
    if def.mesh.is_none() {
        // Non-entity solid: accept the cube hit unconditionally.
        return true;
    }
    // Block-entity cell. Resolve to the anchor + orientation, then
    // ray-AABB test.
    let (anchor, orientation) = match kind {
        Some(EntryKind::Anchor { orientation }) => (cell, orientation),
        Some(EntryKind::Ghost { anchor }) => {
            // Look up the anchor's orientation via its chunk's sidecar.
            let (coord, _) = crate::voxel::world_to_chunk(anchor);
            let Some(&entity) = chunk_map.0.get(&coord) else {
                return true; // anchor not loaded; conservative — accept hit
            };
            let Ok((_, entities)) = chunks.get(entity) else {
                return true;
            };
            match entities.get(anchor) {
                Some(EntryKind::Anchor { orientation }) => (anchor, orientation),
                _ => return true, // sidecar inconsistency; accept
            }
        }
        None => return true, // entity flagged in def but no sidecar yet
    };

    let aabb = def
        .entity_aabb
        .unwrap_or_else(|| block_junk_mod_api::blocks::EntityAabb::cube_union(&def.footprint))
        .rotated(orientation);
    // World-space AABB: relative to anchor's bottom-centre.
    let anchor_origin = anchor.as_vec3() + Vec3::new(0.5, 0.0, 0.5);
    let world_min = anchor_origin + Vec3::from_array(aabb.min);
    let world_max = anchor_origin + Vec3::from_array(aabb.max);
    ray_aabb_within(origin, dir, world_min, world_max, max_distance)
}

/// Slab test: does the ray hit the AABB anywhere along [0, max_distance]?
fn ray_aabb_within(origin: Vec3, dir: Vec3, min: Vec3, max: Vec3, max_distance: f32) -> bool {
    let inv = Vec3::select(dir.cmpeq(Vec3::ZERO), Vec3::INFINITY, dir.recip());
    let t1 = (min - origin) * inv;
    let t2 = (max - origin) * inv;
    let tmin = t1.min(t2);
    let tmax = t1.max(t2);
    let t_enter = tmin.x.max(tmin.y).max(tmin.z);
    let t_exit = tmax.x.min(tmax.y).min(tmax.z);
    t_enter <= t_exit && t_exit >= 0.0 && t_enter <= max_distance
}

/// Build the preview pipeline: a single shared cube mesh, one front
/// material, one back material, and a `PreviewCubeRoot` parent with
/// front + back-pass mesh draws as children. The scene path is created
/// lazily by `update_placement_preview` when the player picks a mesh
/// block, since it needs an asset path that we only know at that point.
fn setup_placement_preview(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut front_mats: ResMut<Assets<PreviewFront>>,
    mut back_mats: ResMut<Assets<PreviewBack>>,
    mut state: ResMut<PreviewState>,
) {
    let cube_mesh = meshes.add(Cuboid::new(1.0, 1.0, 1.0));
    let front = front_mats.add(PreviewFront {
        color: LinearRgba::new(1.0, 1.0, 1.0, 0.4),
    });
    let back = back_mats.add(PreviewBack {
        // Default darken factor; valid placements re-tint to neutral
        // grey, invalid to a red shade.
        color: LinearRgba::new(0.6, 0.6, 0.6, 1.0),
    });

    let root = commands
        .spawn((
            PreviewCubeRoot,
            Transform::default(),
            Visibility::Hidden,
            Name::new("preview_cube_root"),
        ))
        .with_children(|parent| {
            parent.spawn((
                Mesh3d(cube_mesh.clone()),
                MeshMaterial3d(front.clone()),
                Name::new("preview_cube_front"),
            ));
            parent.spawn((
                Mesh3d(cube_mesh.clone()),
                MeshMaterial3d(back.clone()),
                Name::new("preview_cube_back"),
            ));
        })
        .id();
    state.cube_root = Some(root);

    let _ = cube_mesh; // strong handle is now held by the spawned children
    commands.insert_resource(PreviewMaterials { front, back });
}

/// Repaint the placement preview each frame. Routes between two render
/// paths based on the selected block:
///   - Voxel block (no `def.mesh`) → the pre-built cube preview, scaled
///     to span the rotated footprint.
///   - Mesh block (e.g. the bed) → a `PreviewSceneRoot` with the actual
///     glTF; its materials are swapped to the front+back preview pair
///     by `swap_preview_scene_materials` once the scene populates.
///
/// In both cases the front + back-pass draws come for free — both sit
/// under the root entity and pick up its world transform via Bevy's
/// hierarchy.
#[allow(clippy::too_many_arguments, reason = "preview spans many subsystems")]
fn update_placement_preview(
    cam: Query<(&GlobalTransform, &FlyCam)>,
    chunks: Query<(&Chunk, &ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    selected: Res<SelectedBlock>,
    palette: Res<PlaceablePalette>,
    rotation: Res<PlacementRotation>,
    registry: Res<BlockRegistry>,
    asset_server: Res<AssetServer>,
    materials_handles: Res<PreviewMaterials>,
    mut front_mats: ResMut<Assets<PreviewFront>>,
    mut back_mats: ResMut<Assets<PreviewBack>>,
    mut state: ResMut<PreviewState>,
    mut commands: Commands,
    mut roots: Query<(&mut Visibility, &mut Transform)>,
    scene_ready: Query<(), With<PreviewSceneReady>>,
) {
    let hide = |entity: Option<Entity>, q: &mut Query<(&mut Visibility, &mut Transform)>| {
        if let Some(e) = entity {
            if let Ok((mut v, _)) = q.get_mut(e) {
                *v = Visibility::Hidden;
            }
        }
    };

    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    if !locked {
        hide(state.cube_root, &mut roots);
        hide(state.scene_root, &mut roots);
        return;
    }

    let Ok((cam_t, fly)) = cam.single() else {
        hide(state.cube_root, &mut roots);
        hide(state.scene_root, &mut roots);
        return;
    };
    let cam_pos = cam_t.translation();
    let cam_dir = *cam_t.forward();

    let Some(hit) = entity_aware_raycast(
        cam_pos,
        cam_dir,
        RAYCAST_REACH,
        &chunks,
        &chunk_map,
        &registry,
    ) else {
        hide(state.cube_root, &mut roots);
        hide(state.scene_root, &mut roots);
        return;
    };
    let anchor = hit.cell + hit.face_normal;
    let get_block = |world: IVec3| -> BlockSlot {
        let (coord, local) = crate::voxel::world_to_chunk(world);
        chunk_map
            .0
            .get(&coord)
            .and_then(|&entity| chunks.get(entity).ok())
            .map(|(chunk, _)| chunk.get(local))
            .unwrap_or(BlockSlot::EMPTY)
    };

    let slot = selected.current(&palette);
    let def = registry.def(slot);
    let orientation = placement_orientation(fly.yaw, rotation.0);
    let cells = world_footprint(anchor, &def.footprint, orientation);
    if cells.is_empty() {
        hide(state.cube_root, &mut roots);
        hide(state.scene_root, &mut roots);
        return;
    }
    let valid = cells.iter().all(|&c| get_block(c).is_empty());

    // Re-tint shared materials from the selection swatch + validity.
    // Front: tinted with alpha so the ghost reads as the chosen block;
    // a red override tells the player "no" without hiding the preview.
    // Back: a near-grey multiply factor; for invalid we shift it warm
    // so the X-ray shadow on the wall behind also reads "no".
    let [r, g, b] = def.color;
    let front_color = if valid {
        LinearRgba::new(r, g, b, 0.4)
    } else {
        LinearRgba::new(1.0, 0.2, 0.2, 0.55)
    };
    let back_color = if valid {
        LinearRgba::new(0.55, 0.55, 0.55, 1.0)
    } else {
        LinearRgba::new(0.7, 0.4, 0.4, 1.0)
    };
    if let Some(m) = front_mats.get_mut(&materials_handles.front) {
        m.color = front_color;
    }
    if let Some(m) = back_mats.get_mut(&materials_handles.back) {
        m.color = back_color;
    }

    if def.mesh.is_some() {
        // Mesh path. Spawn / replace the SceneRoot if we don't already
        // have one for this slot. Spawning is cheap on the second hit
        // (asset cache); the SceneInstanceReady observer handles the
        // material swap a frame or two later.
        if state.scene_slot != Some(slot) {
            if let Some(old) = state.scene_root.take() {
                commands.entity(old).despawn();
            }
            let mesh_path = def.mesh.as_ref().unwrap();
            let scene: Handle<Scene> = asset_server.load(format!("{mesh_path}#Scene0"));
            let entity = commands
                .spawn((
                    PreviewSceneRoot,
                    SceneRoot(scene),
                    Transform::default(),
                    Visibility::Hidden,
                    Name::new(format!("preview_scene:{}", def.id)),
                ))
                .id();
            state.scene_root = Some(entity);
            state.scene_slot = Some(slot);
        }
        if let Some(scene_entity) = state.scene_root {
            if let Ok((mut vis, mut transform)) = roots.get_mut(scene_entity) {
                transform.translation = anchor.as_vec3() + Vec3::new(0.5, 0.0, 0.5);
                transform.rotation = Quat::from_rotation_y(orientation.yaw());
                transform.scale = Vec3::ONE;
                *vis = if scene_ready.contains(scene_entity) {
                    Visibility::Visible
                } else {
                    // Materials haven't been swapped yet — don't flash
                    // the original glTF materials at the player.
                    Visibility::Hidden
                };
            }
        }
        hide(state.cube_root, &mut roots);
    } else {
        // Voxel path. Position+scale the cube to span the footprint.
        let mut min = cells[0];
        let mut max = cells[0];
        for &c in &cells[1..] {
            min = min.min(c);
            max = max.max(c);
        }
        let extents = (max - min + IVec3::ONE).as_vec3();
        let centre = min.as_vec3() + extents * 0.5;
        if let Some(cube) = state.cube_root {
            if let Ok((mut vis, mut transform)) = roots.get_mut(cube) {
                transform.translation = centre;
                transform.rotation = Quat::IDENTITY;
                transform.scale = extents;
                *vis = Visibility::Visible;
            }
        }
        hide(state.scene_root, &mut roots);
    }
}

/// Walk a freshly-spawned preview SceneRoot's descendants and replace
/// every `Mesh3d` entity's material with our `PreviewFront` handle, plus
/// add a sibling-as-child carrying `PreviewBack` for the depth-flipped
/// X-ray pass. Marker swap completes by inserting `PreviewSceneReady`
/// on the root, which `update_placement_preview` reads to decide when
/// the scene can finally be made visible.
fn swap_preview_scene_materials(
    trigger: On<SceneInstanceReady>,
    scene_roots: Query<(), With<PreviewSceneRoot>>,
    children_q: Query<&Children>,
    meshes: Query<&Mesh3d>,
    materials: Res<PreviewMaterials>,
    mut commands: Commands,
) {
    let root = trigger.event_target();
    if !scene_roots.contains(root) {
        return;
    }
    // BFS through descendants. For each Mesh3d we find: install our
    // front material (replacing whatever StandardMaterial the glTF
    // shipped with) and parent a back-pass twin underneath it.
    let mut stack: Vec<Entity> = vec![root];
    while let Some(entity) = stack.pop() {
        if let Ok(children) = children_q.get(entity) {
            stack.extend(children.iter());
        }
        let Ok(mesh) = meshes.get(entity) else {
            continue;
        };
        let mesh_handle = mesh.0.clone();
        commands
            .entity(entity)
            .remove::<MeshMaterial3d<StandardMaterial>>()
            .insert(MeshMaterial3d(materials.front.clone()))
            .with_children(|parent| {
                parent.spawn((
                    Mesh3d(mesh_handle),
                    MeshMaterial3d(materials.back.clone()),
                ));
            });
    }
    commands.entity(root).insert(PreviewSceneReady);
}

/// Snapshot from server → spawn (or replace) the corresponding local chunk.
/// `ChunkData::Procedural` means "regenerate from the shared terrain
/// function locally" — server didn't ship the bytes because the chunk
/// has never been edited. Entity sidecars travel alongside; an empty
/// sidecar (procedural chunks) is still applied so a stale sidecar from
/// a previous load doesn't survive an unload+reload.
fn receive_snapshots(
    mut commands: Commands,
    mut receivers: Query<&mut MessageReceiver<ChunkSnapshot>>,
    mut chunks: Query<(&mut Chunk, &mut ChunkEntities)>,
    mut map: ResMut<ChunkMap>,
    terrain_slots: Res<TerrainSlots>,
) {
    for mut receiver in receivers.iter_mut() {
        for snapshot in receiver.receive() {
            let chunk = match snapshot.data {
                ChunkData::Procedural => Chunk::from_terrain(snapshot.coord, &terrain_slots),
                ChunkData::Edited(blocks) => Chunk { blocks },
            };
            let entities = ChunkEntities {
                entries: snapshot.entities,
            };
            match map.0.get(&snapshot.coord).copied() {
                Some(entity) => {
                    if let Ok((mut existing_chunk, mut existing_entities)) = chunks.get_mut(entity)
                    {
                        *existing_chunk = chunk;
                        *existing_entities = entities;
                    }
                }
                None => {
                    let entity = commands
                        .spawn((
                            chunk,
                            entities,
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

/// Read keyboard + camera yaw and write the next `PlayerInput` to the
/// owner's predicted avatar. Runs in `FixedPreUpdate` (the
/// WriteClientInputs set ensures the input is buffered before the
/// simulation reads it). Lightyear takes care of replicating the buffer
/// to the server with sequence-numbered redundancy so a dropped UDP
/// packet doesn't drop a tick of input.
///
/// `prev_toggle` is a tiny rising-edge tracker — `ButtonInput.just_pressed`
/// is set once per Update tick, but FixedPreUpdate may run multiple times
/// per Update; without the latch we'd toggle the mode N times per actual
/// keypress.
fn buffer_input(
    keys: Res<ButtonInput<KeyCode>>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    flycam: Query<&FlyCam>,
    mut q: Query<&mut ActionState<PlayerInput>, With<InputMarker<PlayerInput>>>,
    mut prev_toggle: Local<bool>,
) {
    let Ok(mut state) = q.single_mut() else {
        return;
    };

    // Skip input while the cursor is free (alt-tabbed, settings menu);
    // keep yaw in sync via the FlyCam value. A zero-input ActionState
    // is what the controller treats as "no keys held."
    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    let yaw = flycam.single().map(|f| f.yaw).unwrap_or(0.0);

    let mut input = PlayerInput {
        yaw,
        ..Default::default()
    };
    if locked {
        let mut wd = [0i8; 3];
        // Convention: forward = -Z (matches Bevy yaw=0), right = +X, up = +Y.
        if keys.pressed(KeyCode::KeyW) { wd[2] -= 1; }
        if keys.pressed(KeyCode::KeyS) { wd[2] += 1; }
        if keys.pressed(KeyCode::KeyA) { wd[0] -= 1; }
        if keys.pressed(KeyCode::KeyD) { wd[0] += 1; }
        if keys.pressed(KeyCode::Space) { wd[1] += 1; }
        if keys.pressed(KeyCode::ShiftLeft) { wd[1] -= 1; }
        input.wishdir = wd;
        input.jump = keys.pressed(KeyCode::Space);

        let toggle_now = keys.pressed(KeyCode::F1);
        if toggle_now && !*prev_toggle {
            input.toggle_mode = true;
        }
        *prev_toggle = toggle_now;
    }

    state.0 = input;
}

/// Owner-side prediction tick: run the same controller the server runs,
/// against the same input buffered above. Lightyear rolls back and
/// replays this when the server sends a position correction.
fn client_player_step(
    time: Res<Time>,
    chunks: Query<(&'static Chunk, &'static ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    mut avatars: Query<
        (
            &mut AvatarPose,
            &mut AvatarVelocity,
            &mut AvatarOnGround,
            &mut MovementMode,
            &ActionState<PlayerInput>,
        ),
        With<Predicted>,
    >,
) {
    let dt = time.delta_secs();
    let world = WorldCollision {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &registry,
    };
    for (mut pose, mut vel, mut on_ground, mut mode, input) in avatars.iter_mut() {
        apply_walk_step(&mut pose, &mut vel, &mut on_ground, &mut mode, &input.0, dt, &world);
    }
}

/// Replicated `AvatarPose` is the authoritative state; Bevy's renderer
/// reads `Transform`. Translation always syncs. Rotation syncs only for
/// non-owner avatars — the owner's predicted avatar has a `FlyCam` that
/// owns the full camera rotation (yaw from input *plus* pitch, which
/// isn't on the wire because the avatar body is a single yaw-rotated
/// cuboid with no head pitch). Without the filter, `sync_avatar_transforms`
/// would clobber pitch every tick when the server-authoritative pose
/// arrives, causing the visible "snap-to-horizon" judder.
fn sync_avatar_transforms(
    mut avatars: Query<(&AvatarPose, &mut Transform, Has<FlyCam>), Changed<AvatarPose>>,
) {
    for (pose, mut transform, has_flycam) in avatars.iter_mut() {
        transform.translation = pose.translation;
        if !has_flycam {
            transform.rotation = Quat::from_rotation_y(pose.yaw);
        }
    }
}

/// Wire the owner's predicted avatar with everything that makes it
/// playable: a camera, the FlyCam yaw/pitch state for mouse-look, an
/// input marker so `buffer_input` knows where to write, an initial
/// `ActionState`, and the headlamp PointLight that used to live on the
/// standalone camera.
fn handle_predicted_spawn(
    trigger: On<Add, (Avatar, Predicted)>,
    avatars: Query<(), (With<Avatar>, With<Predicted>)>,
    mut commands: Commands,
) {
    let entity = trigger.entity;
    if avatars.get(entity).is_err() {
        return;
    }
    info!("predicted avatar arrived: {entity:?}");
    commands.entity(entity).insert((
        Camera3d::default(),
        Transform::default(),
        FlyCam::default(),
        ActionState::<PlayerInput>::default(),
        InputMarker::<PlayerInput>::default(),
        PointLight {
            intensity: 750_000.0,
            range: 60.0,
            shadows_enabled: false,
            ..default()
        },
    ));
}

/// Server's slot ↔ id table arrives once on connect. Compare against our
/// local `BlockRegistry`; any mismatch indicates a divergent mod set and
/// is logged loudly. We don't disconnect today (until saves exist there's
/// no real harm), but the loud log makes the failure obvious in dev.
fn receive_block_manifest(
    mut receivers: Query<&mut MessageReceiver<BlockManifest>>,
    registry: Res<BlockRegistry>,
) {
    for mut receiver in receivers.iter_mut() {
        for manifest in receiver.receive() {
            let mut mismatches = 0usize;
            for (i, server_id) in manifest.slots.iter().enumerate() {
                let slot = BlockSlot(i as u16);
                if i >= registry.slot_count() {
                    error!(slot = i, id = %server_id, "server has block id we don't");
                    mismatches += 1;
                    continue;
                }
                let local_id = registry.id_of(slot);
                if local_id != server_id {
                    error!(
                        slot = i,
                        server_id = %server_id,
                        client_id = %local_id,
                        "block manifest mismatch",
                    );
                    mismatches += 1;
                }
            }
            if manifest.slots.len() < registry.slot_count() {
                error!(
                    server_count = manifest.slots.len(),
                    client_count = registry.slot_count(),
                    "client registered more blocks than server",
                );
                mismatches += 1;
            }
            if mismatches == 0 {
                info!(
                    "block manifest OK ({} slot(s) agreed)",
                    manifest.slots.len()
                );
            }
        }
    }
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

/// Server broadcast of an applied edit → mirror it into the local chunk
/// state so this client's view stays in sync. Both place and break
/// expand the def's footprint locally; the broadcast carries the anchor +
/// orientation and we derive cells the same way the server did.
///
/// For breaks, we read the slot at the anchor *before* clearing so we
/// know which footprint to expand (the broadcast doesn't include it —
/// the client is expected to read it from local state). Cells that fall
/// in unloaded chunks are silently skipped; their sidecar will arrive
/// via `ChunkSnapshot` whenever the chunk enters AoI.
fn receive_block_edit_broadcasts(
    mut receivers: Query<&mut MessageReceiver<BlockEdit>>,
    mut chunks: Query<(&mut Chunk, &mut ChunkEntities)>,
    map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
) {
    for mut receiver in receivers.iter_mut() {
        let edits: Vec<BlockEdit> = receiver.receive().collect();
        for edit in edits {
            apply_broadcast_edit(edit, &mut chunks, &map, &registry);
        }
    }
}

fn apply_broadcast_edit(
    edit: BlockEdit,
    chunks: &mut Query<(&mut Chunk, &mut ChunkEntities)>,
    map: &ChunkMap,
    registry: &BlockRegistry,
) {
    // For a break we need the slot that *was* at the anchor — the wire
    // doesn't carry it (the broadcast says "anchor + EMPTY"), so we read
    // it from the local chunk. The wire DOES carry `orientation` (the
    // orientation the entity had at the time of the break), so we trust
    // that directly rather than re-deriving it from our sidecar.
    let slot = if edit.slot.is_empty() {
        let (anchor_coord, anchor_local) = crate::voxel::world_to_chunk(edit.anchor);
        let Some(&anchor_entity) = map.0.get(&anchor_coord) else {
            return;
        };
        let Ok((chunk, _)) = chunks.get(anchor_entity) else {
            return;
        };
        let anchor_slot = chunk.get(anchor_local);
        if anchor_slot.is_empty() {
            // Already cleared (a previous broadcast applied). No-op.
            return;
        }
        anchor_slot
    } else {
        edit.slot
    };

    let def = registry.def(slot);
    let cells = world_footprint(edit.anchor, &def.footprint, edit.orientation);
    let new_slot = if edit.slot.is_empty() {
        BlockSlot::EMPTY
    } else {
        edit.slot
    };
    let needs_sidecar = def.mesh.is_some();

    for cell in cells {
        let (coord, local) = crate::voxel::world_to_chunk(cell);
        let Some(&entity) = map.0.get(&coord) else {
            continue;
        };
        let Ok((mut chunk, mut entities)) = chunks.get_mut(entity) else {
            continue;
        };
        chunk.set(local, new_slot);
        if edit.slot.is_empty() {
            // remove() is a no-op if no entry — covers both block-entity
            // breaks and plain-cube breaks uniformly.
            entities.remove(cell);
        } else if needs_sidecar {
            let kind = if cell == edit.anchor {
                EntryKind::Anchor {
                    orientation: edit.orientation,
                }
            } else {
                EntryKind::Ghost {
                    anchor: edit.anchor,
                }
            };
            entities.insert(cell, kind);
        }
    }
}

/// Paint replicated avatar entities with the shared cuboid mesh, EXCEPT
/// the owner's own avatar — they'd see the cuboid filling their view.
/// We use a regular system rather than an `On<Add, Avatar>` observer
/// because lightyear's `Predicted`/`Interpolated` markers may arrive in a
/// later replication tick than the `Avatar` component itself; an observer
/// firing on `Avatar` alone would happily mesh up the owner's predicted
/// entity before the marker showed up.
fn attach_avatar_visuals(
    avatars: Query<Entity, (With<Avatar>, Without<Mesh3d>, Without<Predicted>)>,
    assets: Res<AvatarAssets>,
    mut commands: Commands,
) {
    for entity in avatars.iter() {
        info!("remote avatar entered view: {entity:?}");
        commands.entity(entity).insert((
            Mesh3d(assets.mesh.clone()),
            MeshMaterial3d(assets.material.clone()),
        ));
    }
}

fn mesh_chunks(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    registry: Res<BlockRegistry>,
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
        let Some(mesh) = chunk.build_mesh(&registry) else {
            continue;
        };
        let mesh_handle = meshes.add(mesh);
        let mut e = commands.entity(entity);
        e.insert(Mesh3d(mesh_handle));
        if material.is_none() {
            // base_color WHITE so the per-vertex colours emitted by the
            // mesher are passed through unmodulated; PBR still adds shading.
            e.insert(MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::WHITE,
                perceptual_roughness: 0.9,
                ..default()
            })));
        }
    }
}

/// Spawn / despawn ECS entities for blocks whose `BlockDef.mesh` is set
/// (block entities — beds, doors, etc.). Anchors drive rendering; ghost
/// cells live only in the chunk's slot grid + sidecar so the cube mesher
/// skips them but no duplicate scene is spawned.
///
/// Two phases per tick:
///   1. **Cleanup**: chunks tracked here that are no longer in
///      `ChunkMap` were unloaded; despawn all their block entities.
///   2. **Diff per changed chunk** (chunk's `Chunk` *or* `ChunkEntities`
///      mutated this tick): rescan the sidecar's anchor entries against
///      what we've spawned. Despawn dropped, spawn new with the
///      orientation rotation baked into the Transform.
///
/// Runs in `PostSimulation` after the chunk-receive systems so the
/// `Chunk` data, sidecar, and `ChunkMap` reflect this tick's events.
fn refresh_block_entities(
    chunks_changed: Query<
        (&Chunk, &ChunkEntities, &ChunkCoord),
        Or<(Changed<Chunk>, Changed<ChunkEntities>)>,
    >,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    asset_server: Res<AssetServer>,
    mut entities: ResMut<BlockEntities>,
    mut commands: Commands,
) {
    // 1. Drop entities for chunks that no longer exist.
    let stale: Vec<ChunkCoord> = entities
        .by_chunk
        .keys()
        .copied()
        .filter(|c| !chunk_map.0.contains_key(c))
        .collect();
    for coord in stale {
        if let Some(cells) = entities.by_chunk.remove(&coord) {
            for cell in cells {
                if let Some(entity) = entities.by_cell.remove(&cell) {
                    commands.entity(entity).despawn();
                }
            }
        }
    }

    // 2. Per changed chunk: diff sidecar Anchor entries vs spawned set.
    // Filter to anchors whose slot is actually a mesh block. Worlds saved
    // before the place handler stopped writing sidecar entries for plain
    // cubes can carry leftover Anchors on non-mesh slots; ignoring them
    // here lets those worlds heal silently as the affected blocks get
    // broken (which always clears the entry).
    for (chunk, sidecar, coord) in chunks_changed.iter() {
        let mut new_anchors: HashSet<IVec3> = HashSet::default();
        for entry in &sidecar.entries {
            if let EntryKind::Anchor { .. } = entry.kind {
                let (cc, local) = crate::voxel::world_to_chunk(entry.cell);
                debug_assert_eq!(cc, *coord);
                if registry.def(chunk.get(local)).mesh.is_some() {
                    new_anchors.insert(entry.cell);
                }
            }
        }

        let old_anchors = entities.by_chunk.get(coord).cloned().unwrap_or_default();

        for cell in old_anchors.difference(&new_anchors) {
            if let Some(entity) = entities.by_cell.remove(cell) {
                commands.entity(entity).despawn();
            }
        }

        for cell in new_anchors.difference(&old_anchors) {
            // Resolve the slot + orientation. Slot via the chunk grid
            // (the anchor cell holds the block-entity's slot); orientation
            // via the sidecar entry we just iterated. `new_anchors` was
            // already filtered to mesh slots, so `def.mesh` is Some here.
            let (cc, local) = crate::voxel::world_to_chunk(*cell);
            debug_assert_eq!(cc, *coord);
            let slot = chunk.get(local);
            let def = registry.def(slot);
            let mesh_path = def.mesh.as_ref().expect("non-mesh slot filtered above");
            let orientation = match sidecar.get(*cell) {
                Some(EntryKind::Anchor { orientation }) => orientation,
                _ => Cardinal::default(),
            };
            let scene: Handle<Scene> = asset_server.load(format!("{mesh_path}#Scene0"));
            let translation = cell.as_vec3() + Vec3::new(0.5, 0.0, 0.5);
            let rotation = Quat::from_rotation_y(orientation.yaw());
            let entity = commands
                .spawn((
                    SceneRoot(scene),
                    Transform {
                        translation,
                        rotation,
                        ..default()
                    },
                    Name::new(format!("block_entity:{}{:?}", def.id, cell.to_array())),
                ))
                .id();
            entities.by_cell.insert(*cell, entity);
        }

        entities.by_chunk.insert(*coord, new_anchors);
    }
}
