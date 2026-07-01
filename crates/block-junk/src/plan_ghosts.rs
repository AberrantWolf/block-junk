//! Translucent ghost meshes at planned Build cells.
//!
//! A Build tag used to render only as a colored outline — *where*
//! something is planned, but not *what*. This module gives every Build
//! plan a ghost of the actual block: the shared preview cube (scaled to
//! the rotated footprint) for voxel blocks, the block's glTF scene for
//! mesh blocks, both drawn with the front/back preview material pass
//! from [`crate::preview`] so the ghost reads through walls the same
//! way the cursor placement preview does.
//!
//! Tint: the block's registry color at a *lower* alpha than the cursor
//! preview, so an in-flight placement (what an R-click would tag right
//! now) always reads stronger than the already-committed plans behind
//! it.
//!
//! Sync is diff-based off `Plans` change detection: ghosts spawn when a
//! Build tag appears, despawn when it clears (built, un-tagged, or
//! cancelled), and respawn if the tag's slot/orientation changes.
//! Remove plans get nothing — they already overlay the destroy outline
//! on a real block.

use bevy::prelude::*;
use std::collections::HashMap;

use block_junk_mod_api::blocks::Cardinal;

use crate::blocks::{BlockRegistry, BlockSlot};
use crate::client::world_footprint;
use crate::menu::AppState;
use crate::plans::Plans;
use crate::preview::{PreviewBack, PreviewFront, PreviewMaterialPair, PreviewSceneReady};
use crate::protocol::{GameSet, PlanKind};

/// Ghost fill alpha. Deliberately weaker than the cursor preview's 0.4
/// so committed plans sit visually behind the live placement ghost.
const GHOST_FRONT_ALPHA: f32 = 0.22;
/// Ghost back-pass darken factor — close to neutral so a field of
/// plans doesn't black out the wall behind it.
const GHOST_BACK_DARKEN: f32 = 0.8;

/// Marker on every ghost root entity.
#[derive(Component)]
struct PlanGhost;

struct GhostEntry {
    entity: Entity,
    slot: BlockSlot,
    orientation: Cardinal,
}

/// Cell → live ghost. The sync system's diff target.
#[derive(Resource, Default)]
struct PlanGhostIndex {
    by_cell: HashMap<IVec3, GhostEntry>,
}

/// One fixed-tint material pair per block slot, shared by every ghost
/// of that block. Unlike the cursor preview's single re-tinted pair,
/// ghosts of different blocks coexist on screen, so the tint has to
/// live per-slot.
#[derive(Resource, Default)]
struct GhostMaterials {
    by_slot: HashMap<BlockSlot, (Handle<PreviewFront>, Handle<PreviewBack>)>,
}

/// Shared unit cube for voxel-block ghosts (scaled per footprint).
#[derive(Resource)]
struct GhostCubeMesh(Handle<Mesh>);

pub struct PlanGhostsPlugin;

impl Plugin for PlanGhostsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PlanGhostIndex>();
        app.init_resource::<GhostMaterials>();
        app.add_systems(OnEnter(AppState::InGame), setup_ghost_assets);
        app.add_systems(
            Update,
            (sync_plan_ghosts, reveal_ready_ghost_scenes)
                .in_set(GameSet::PostSimulation)
                .run_if(in_state(AppState::InGame)),
        );
    }
}

fn setup_ghost_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    existing: Option<Res<GhostCubeMesh>>,
) {
    // OnEnter(InGame) re-fires on un-pause; the mesh outlives pause.
    if existing.is_some() {
        return;
    }
    commands.insert_resource(GhostCubeMesh(meshes.add(Cuboid::new(1.0, 1.0, 1.0))));
}

/// Diff the live `Plans` against the ghost index: despawn ghosts whose
/// tag is gone or changed, spawn ghosts for new Build tags. Runs only
/// on `Plans` mutation, so a parked world costs one `is_changed` check
/// per frame.
#[allow(
    clippy::too_many_arguments,
    reason = "ghost spawn pulls mesh+scene+material assets"
)]
fn sync_plan_ghosts(
    plans: Res<Plans>,
    registry: Res<BlockRegistry>,
    asset_server: Res<AssetServer>,
    cube: Res<GhostCubeMesh>,
    mut materials: ResMut<GhostMaterials>,
    mut front_mats: ResMut<Assets<PreviewFront>>,
    mut back_mats: ResMut<Assets<PreviewBack>>,
    mut index: ResMut<PlanGhostIndex>,
    mut commands: Commands,
) {
    if !plans.is_changed() {
        return;
    }

    let mut desired: HashMap<IVec3, (BlockSlot, Cardinal)> = HashMap::new();
    for (cell, state) in plans.iter() {
        if let PlanKind::Build { slot, orientation } = state.kind {
            // Bounds-guard: plan slots arrive over the wire; a slot the
            // local registry doesn't know renders nothing rather than
            // panicking in `def`.
            if (slot.0 as usize) < registry.slot_count() {
                desired.insert(*cell, (slot, orientation));
            }
        }
    }

    // Despawn stale ghosts (tag cleared) and changed ones (slot or
    // orientation moved — rare, but a re-tag over the same cell is
    // legal); changed cells respawn in the loop below.
    index.by_cell.retain(|cell, entry| {
        let keep = matches!(
            desired.get(cell),
            Some(&(slot, orientation)) if slot == entry.slot && orientation == entry.orientation,
        );
        if !keep {
            commands.entity(entry.entity).despawn();
        }
        keep
    });

    for (cell, (slot, orientation)) in desired {
        if index.by_cell.contains_key(&cell) {
            continue;
        }
        let def = registry.def(slot);
        let (front, back) = materials
            .by_slot
            .entry(slot)
            .or_insert_with(|| {
                let [r, g, b] = def.color;
                (
                    front_mats.add(PreviewFront {
                        color: LinearRgba::new(r, g, b, GHOST_FRONT_ALPHA),
                    }),
                    back_mats.add(PreviewBack {
                        color: LinearRgba::new(
                            GHOST_BACK_DARKEN,
                            GHOST_BACK_DARKEN,
                            GHOST_BACK_DARKEN,
                            1.0,
                        ),
                    }),
                )
            })
            .clone();

        let entity = if let Some(mesh_path) = def.mesh.as_ref() {
            // Mesh block: the actual glTF scene, materials swapped to
            // the ghost pair by the shared SceneInstanceReady observer.
            // Hidden until the swap lands (PreviewSceneReady), then
            // revealed by `reveal_ready_ghost_scenes`.
            let scene: Handle<Scene> = asset_server.load(format!("{mesh_path}#Scene0"));
            commands
                .spawn((
                    PlanGhost,
                    SceneRoot(scene),
                    PreviewMaterialPair {
                        front: front.clone(),
                        back: back.clone(),
                    },
                    Transform {
                        translation: cell.as_vec3() + Vec3::new(0.5, 0.0, 0.5),
                        rotation: Quat::from_rotation_y(orientation.yaw()),
                        ..default()
                    },
                    Visibility::Hidden,
                    Name::new(format!("plan_ghost_scene:{}", def.id)),
                ))
                .id()
        } else {
            // Voxel block: shared cube spanning the rotated footprint.
            let cells = world_footprint(cell, &def.footprint, orientation);
            let mut min = cell;
            let mut max = cell;
            for &c in &cells {
                min = min.min(c);
                max = max.max(c);
            }
            let extents = (max - min + IVec3::ONE).as_vec3();
            let centre = min.as_vec3() + extents * 0.5;
            commands
                .spawn((
                    PlanGhost,
                    Transform {
                        translation: centre,
                        scale: extents,
                        ..default()
                    },
                    Visibility::Visible,
                    Name::new(format!("plan_ghost_cube:{}", def.id)),
                ))
                .with_children(|root| {
                    root.spawn((Mesh3d(cube.0.clone()), MeshMaterial3d(front.clone())));
                    root.spawn((Mesh3d(cube.0.clone()), MeshMaterial3d(back.clone())));
                })
                .id()
        };
        index.by_cell.insert(
            cell,
            GhostEntry {
                entity,
                slot,
                orientation,
            },
        );
    }
}

/// Flip a ghost scene visible once the material-swap observer marks it
/// ready. Cube ghosts spawn visible directly (no asset wait).
fn reveal_ready_ghost_scenes(
    mut ghosts: Query<&mut Visibility, (With<PlanGhost>, Added<PreviewSceneReady>)>,
) {
    for mut visibility in ghosts.iter_mut() {
        *visibility = Visibility::Visible;
    }
}
