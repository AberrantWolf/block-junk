use bevy::input::mouse::AccumulatedMouseScroll;
use bevy::platform::collections::{HashMap, HashSet};
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use block_junk_mod_api::blocks::Cardinal;
use lightyear::prelude::*;

use crate::blocks::{BlockRegistry, BlockSlot, TerrainSlots};
use crate::camera::{FlyCam, FlyCamPlugin};
use crate::protocol::{
    Avatar, AvatarPose, BlockEdit, BlockManifest, ChunkCoord, ChunkData, ChunkSnapshot,
    ChunkUnload, GameSet, PlayerPose, WorldChannel,
};
use crate::voxel::{Chunk, ChunkEntities, EntryKind};

pub struct ClientPlugin;

impl Plugin for ClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(FlyCamPlugin)
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
            .add_systems(Startup, setup_scene)
            .add_systems(
                Update,
                (
                    place_break_input,
                    send_player_pose,
                    cycle_selected_or_rotation,
                    reset_rotation_on_selection_change,
                )
                    .in_set(GameSet::Input),
            )
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
                )
                    .in_set(GameSet::PostSimulation),
            )
            .add_observer(attach_avatar_visuals);
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

/// Client-side chunk lookup, parallel to the server's. Filled by snapshot
/// receipt; consulted when applying broadcast edits or raycasting.
#[derive(Resource, Default)]
pub struct ChunkMap(pub HashMap<ChunkCoord, Entity>);

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

/// Marker for the translucent ghost-cuboid the player sees at the
/// placement target. Always exactly one in the world; visibility toggles
/// based on whether a placement target exists.
#[derive(Component)]
struct PlacementPreview;

/// Cached material handles for the placement preview. The valid material's
/// `base_color` is rewritten each frame from the selected block's swatch
/// so the cube reads as a tinted version of what would land. Invalid
/// stays a fixed red — that's a "stop" colour, no need to tint.
#[derive(Resource)]
struct PreviewAssets {
    valid: Handle<StandardMaterial>,
    invalid: Handle<StandardMaterial>,
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

    // Translucent placement-preview cuboid. Its Mesh is a unit cube; the
    // update system rewrites translation+scale to span the rotated
    // footprint. Two materials so we can flag invalid placements with a
    // distinct red tint without ever blocking the click — the player still
    // gets clear feedback before they release the button.
    let preview_cube = meshes.add(Cuboid::new(1.0, 1.0, 1.0));
    let valid_material = materials.add(StandardMaterial {
        base_color: Color::srgba(1.0, 1.0, 1.0, 0.35),
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        ..default()
    });
    let invalid_material = materials.add(StandardMaterial {
        base_color: Color::srgba(1.0, 0.2, 0.2, 0.45),
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        ..default()
    });
    commands.spawn((
        PlacementPreview,
        Mesh3d(preview_cube),
        MeshMaterial3d(valid_material.clone()),
        Visibility::Hidden,
        Transform::default(),
        Name::new("placement_preview"),
    ));
    commands.insert_resource(PreviewAssets {
        valid: valid_material,
        invalid: invalid_material,
    });

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

/// Repaint the placement preview each frame: aim the cuboid at the cell
/// the player would place into, scale it to span the rotated footprint,
/// and tint it valid/invalid based on whether every footprint cell is
/// empty. Hidden when the cursor is unlocked or the ray misses the world.
///
/// The preview reads the current selection's footprint from the registry,
/// so single-cell blocks naturally render as a unit cube and multi-cell
/// blocks (the bed) render as a 2-long cuboid that follows the player's
/// orientation.
fn update_placement_preview(
    cam: Query<(&GlobalTransform, &FlyCam)>,
    chunks: Query<(&Chunk, &ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    selected: Res<SelectedBlock>,
    palette: Res<PlaceablePalette>,
    rotation: Res<PlacementRotation>,
    registry: Res<BlockRegistry>,
    preview_assets: Res<PreviewAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut preview: Query<
        (
            &mut Visibility,
            &mut Transform,
            &mut MeshMaterial3d<StandardMaterial>,
        ),
        With<PlacementPreview>,
    >,
) {
    let Ok((mut visibility, mut transform, mut mat_handle)) = preview.single_mut() else {
        return;
    };

    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    if !locked {
        *visibility = Visibility::Hidden;
        return;
    }

    let Ok((cam_t, fly)) = cam.single() else {
        *visibility = Visibility::Hidden;
        return;
    };
    let cam_pos = cam_t.translation();
    let cam_dir = *cam_t.forward();

    // Same raycast as the actual click path so the preview never lies
    // about where the place would land — including walking past the
    // airspace inside a partial-cell block-entity.
    let Some(hit) = entity_aware_raycast(
        cam_pos,
        cam_dir,
        RAYCAST_REACH,
        &chunks,
        &chunk_map,
        &registry,
    ) else {
        *visibility = Visibility::Hidden;
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
        // A mod registered an empty footprint — degenerate, but don't
        // panic the renderer. Hide and bail.
        *visibility = Visibility::Hidden;
        return;
    }

    // Validation: every footprint cell must currently be empty. For
    // single-cell blocks this is "is the target air?"; for multi-cell
    // blocks (the bed) it's the same check across each occupied cell.
    let valid = cells.iter().all(|&c| get_block(c).is_empty());

    let mut min = cells[0];
    let mut max = cells[0];
    for &c in &cells[1..] {
        min = min.min(c);
        max = max.max(c);
    }
    let extents = (max - min + IVec3::ONE).as_vec3();
    let centre = min.as_vec3() + extents * 0.5;
    *transform = Transform::from_translation(centre).with_scale(extents);

    let want = if valid {
        preview_assets.valid.clone()
    } else {
        preview_assets.invalid.clone()
    };
    if mat_handle.0 != want {
        mat_handle.0 = want;
    }

    // Re-tint the valid material from the selected block's swatch each
    // frame. Cheap and saves us from caching per-block material handles
    // for every placeable. The invalid material stays a fixed red.
    if let Some(mat) = materials.get_mut(&preview_assets.valid) {
        let [r, g, b] = def.color;
        mat.base_color = Color::srgba(r, g, b, 0.35);
    }

    *visibility = Visibility::Visible;
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

/// Period (seconds) between player-pose updates sent to the server. 10 Hz
/// is plenty for AoI streaming decisions and stays under 200 B/s.
const POSE_SEND_PERIOD: f32 = 0.1;

fn send_player_pose(
    time: Res<Time>,
    mut accum: Local<f32>,
    cam: Query<(&GlobalTransform, &FlyCam)>,
    mut sender: Query<&mut MessageSender<PlayerPose>>,
) {
    *accum += time.delta_secs();
    if *accum < POSE_SEND_PERIOD {
        return;
    }
    *accum = 0.0;

    let Ok((cam_t, fly)) = cam.single() else {
        return;
    };
    let Ok(mut sender) = sender.single_mut() else {
        return;
    };
    sender.send::<WorldChannel>(PlayerPose {
        translation: cam_t.translation(),
        yaw: fly.yaw,
    });
}

/// Replicated `AvatarPose` is the authoritative state for remote players;
/// Bevy's renderer reads `Transform`. Copy across whenever the pose changes.
/// Yaw rotates the body around +Y; pitch is intentionally not applied —
/// when we add a head/torso split the head will get its own component.
fn sync_avatar_transforms(
    mut avatars: Query<(&AvatarPose, &mut Transform), Changed<AvatarPose>>,
) {
    for (pose, mut transform) in avatars.iter_mut() {
        transform.translation = pose.translation;
        transform.rotation = Quat::from_rotation_y(pose.yaw);
    }
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
            entities.remove(cell);
        } else {
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

/// A replicated avatar entity arrived from the server — paint it with the
/// shared mesh + material so the player has something to look at. The
/// entity already has a Transform from replication; Mesh3d won't override it.
fn attach_avatar_visuals(
    trigger: On<Add, Avatar>,
    assets: Res<AvatarAssets>,
    mut commands: Commands,
) {
    info!("remote avatar entered view: {:?}", trigger.entity);
    commands.entity(trigger.entity).insert((
        Mesh3d(assets.mesh.clone()),
        MeshMaterial3d(assets.material.clone()),
    ));
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
    for (chunk, sidecar, coord) in chunks_changed.iter() {
        let mut new_anchors: HashSet<IVec3> = HashSet::default();
        for entry in &sidecar.entries {
            if let EntryKind::Anchor { .. } = entry.kind {
                new_anchors.insert(entry.cell);
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
            // via the sidecar entry we just iterated.
            let (cc, local) = crate::voxel::world_to_chunk(*cell);
            debug_assert_eq!(cc, *coord);
            let slot = chunk.get(local);
            let def = registry.def(slot);
            let Some(mesh_path) = def.mesh.as_ref() else {
                // Sidecar says anchor here but the slot isn't a mesh
                // block. Bug somewhere upstream; warn and skip.
                warn!(?cell, "anchor entry on non-mesh slot; skipping render");
                continue;
            };
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
