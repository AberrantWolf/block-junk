//! In-world overlays for committed plans: dithered ghost meshes at
//! Build cells, world-space crosshatch at Remove cells.
//!
//! Build tags render as a ghost of the actual block — the shared cube
//! with [`GhostBlockMaterial`] compositing the block's real baked
//! textures for voxel blocks, the block's glTF scene re-materialed via
//! [`GhostMeshStyle`] for mesh blocks. Ghosts are screen-door dithered
//! (see `preview.rs` module docs for why not alpha blending), tinted
//! white when materials are satisfied and amber while waiting on
//! deposits, and fade with camera distance to a faint-but-never-zero
//! sketch — a far-off planned building reads as a soft suggestion, not
//! a wall of noise.
//!
//! Remove tags render as a red diagonal crosshatch on a slightly
//! inflated unit cube per tagged cell ([`CrosshatchMaterial`]). The
//! pattern derives from world coordinates, so it is continuous across
//! neighbouring tagged cells and identical on terrain and mesh blocks —
//! and a cancelled cell in a demolition field reads as an obvious hole.
//!
//! Sync is diff-based off `Plans` change detection, keyed by the full
//! desired overlay (kind + slot + orientation + satisfied), so a
//! deposit that satisfies a plan respawns its ghost with the white
//! tint.

use bevy::light::NotShadowCaster;
use bevy::prelude::*;
use std::collections::HashMap;

use block_junk_mod_api::blocks::Cardinal;

use crate::block_textures::{BlockTextures, GhostBlockMaterial, GhostParams};
use crate::blocks::{BlockRegistry, BlockSlot};
use crate::client::{ghost_block_material, world_footprint};
use crate::menu::AppState;
use crate::plans::Plans;
use crate::preview::{
    CrosshatchMaterial, CrosshatchParams, DitherParams, GhostMeshStyle, PreviewSceneReady,
};
use crate::protocol::{GameSet, PlanKind};

/// Dither coverage of committed Build ghosts. Deliberately sparser
/// than the cursor preview's 0.55 so an in-flight placement always
/// reads stronger than the committed queue behind it.
const GHOST_COVERAGE: f32 = 0.35;
/// Coverage of the behind-wall x-ray pass.
const GHOST_XRAY_COVERAGE: f32 = 0.12;
/// Distance-fade floor for the front pass (~11% of full coverage —
/// faint sketch, never invisible).
const GHOST_MIN_COVERAGE: f32 = 0.04;
/// Distance-fade floor for the x-ray pass.
const GHOST_XRAY_MIN_COVERAGE: f32 = 0.02;
/// Camera distance where committed-overlay fading begins (metres) —
/// roughly the far end of interaction range.
const FADE_START: f32 = 16.0;
/// Camera distance where fading bottoms out.
const FADE_END: f32 = 48.0;
/// Tint for Build plans still waiting on materials: amber, the
/// at-a-distance answer to "why is nobody building this?".
const WAITING_TINT: Vec4 = Vec4::new(1.0, 0.72, 0.25, 1.0);

/// Marker on every overlay root entity.
#[derive(Component)]
struct PlanGhost;

/// What one tagged cell should display. Doubles as the diff key: any
/// change (kind, slot, orientation, satisfaction) despawns + respawns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OverlayDesired {
    Build {
        slot: BlockSlot,
        orientation: Cardinal,
        satisfied: bool,
    },
    RemoveHatch,
}

struct GhostEntry {
    entity: Entity,
    desired: OverlayDesired,
}

/// Cell → live overlay. The sync system's diff target.
#[derive(Resource, Default)]
struct PlanGhostIndex {
    by_cell: HashMap<IVec3, GhostEntry>,
}

/// Shared material handles. Ghost materials are keyed per
/// (slot, satisfied) — every ghost of the same block type + state
/// shares one front + x-ray pair. The crosshatch is world-derived, so
/// a single handle covers every Remove cell.
#[derive(Resource, Default)]
struct GhostMaterials {
    by_key: HashMap<(BlockSlot, bool), (Handle<GhostBlockMaterial>, Handle<GhostBlockMaterial>)>,
    crosshatch: Option<Handle<CrosshatchMaterial>>,
}

/// Shared unit cube for voxel-block ghosts (scaled per footprint) and
/// crosshatch shells (inflated 1%).
#[derive(Resource)]
struct GhostCubeMesh(Handle<Mesh>);

pub struct PlanGhostsPlugin;

impl Plugin for PlanGhostsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PlanGhostIndex>();
        app.init_resource::<GhostMaterials>();
        app.add_systems(OnEnter(AppState::InGame), setup_ghost_assets);
        // Overlay entities carry DespawnOnExit; the index tracking them
        // must be dropped in the same transition or the sync diff would
        // chase dead entity ids next session. (Private type, so the reset
        // lives here rather than in client.rs::cleanup_session.)
        app.add_systems(OnExit(AppState::InGame), reset_ghost_index);
        app.add_systems(
            Update,
            (sync_plan_ghosts, reveal_ready_ghost_scenes)
                .in_set(GameSet::PostSimulation)
                .run_if(in_state(AppState::InGame)),
        );
    }
}

fn reset_ghost_index(mut index: ResMut<PlanGhostIndex>) {
    index.by_cell.clear();
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

/// Ghost params for a committed Build overlay: distance-faded, tinted
/// by material satisfaction.
fn committed_ghost_params(slot: BlockSlot, satisfied: bool, xray: bool) -> GhostParams {
    let tint = if satisfied { Vec4::ONE } else { WAITING_TINT };
    let (coverage, min_coverage) = if xray {
        (GHOST_XRAY_COVERAGE, GHOST_XRAY_MIN_COVERAGE)
    } else {
        (GHOST_COVERAGE, GHOST_MIN_COVERAGE)
    };
    GhostParams::faded(
        u32::from(slot.0),
        tint,
        coverage,
        min_coverage,
        FADE_START,
        FADE_END,
    )
}

/// Diff the live `Plans` against the overlay index: despawn overlays
/// whose tag is gone or changed, spawn overlays for new tags. Runs
/// only on `Plans` mutation, so a parked world costs one `is_changed`
/// check per frame.
#[allow(
    clippy::too_many_arguments,
    reason = "overlay spawn pulls mesh+scene+material assets"
)]
fn sync_plan_ghosts(
    plans: Res<Plans>,
    registry: Res<BlockRegistry>,
    asset_server: Res<AssetServer>,
    cube: Res<GhostCubeMesh>,
    textures: Res<BlockTextures>,
    mut materials: ResMut<GhostMaterials>,
    mut ghost_mats: ResMut<Assets<GhostBlockMaterial>>,
    mut hatch_mats: ResMut<Assets<CrosshatchMaterial>>,
    mut index: ResMut<PlanGhostIndex>,
    mut commands: Commands,
) {
    if !plans.is_changed() {
        return;
    }

    let mut desired: HashMap<IVec3, OverlayDesired> = HashMap::new();
    for (cell, state) in plans.iter() {
        match state.kind {
            PlanKind::Build { slot, orientation } => {
                // Bounds-guard: plan slots arrive over the wire; a slot
                // the local registry doesn't know renders nothing
                // rather than panicking in `def`.
                if (slot.0 as usize) < registry.slot_count() {
                    desired.insert(
                        *cell,
                        OverlayDesired::Build {
                            slot,
                            orientation,
                            satisfied: state.is_satisfied(),
                        },
                    );
                }
            }
            PlanKind::Remove => {
                desired.insert(*cell, OverlayDesired::RemoveHatch);
            }
        }
    }

    // Despawn stale overlays (tag cleared) and changed ones (slot,
    // orientation, or satisfaction moved); changed cells respawn in
    // the loop below.
    index.by_cell.retain(|cell, entry| {
        let keep = desired.get(cell) == Some(&entry.desired);
        if !keep {
            commands.entity(entry.entity).despawn();
        }
        keep
    });

    for (cell, want) in desired {
        if index.by_cell.contains_key(&cell) {
            continue;
        }
        let entity = match want {
            OverlayDesired::RemoveHatch => {
                let material = materials
                    .crosshatch
                    .get_or_insert_with(|| {
                        hatch_mats.add(CrosshatchMaterial {
                            params: CrosshatchParams {
                                color: Vec4::new(1.0, 0.15, 0.15, 0.5),
                                period: 0.25,
                                duty: 0.4,
                                min_alpha: 0.2,
                                fade_start: FADE_START,
                                fade_end: FADE_END,
                                _pad0: 0.0,
                                _pad1: 0.0,
                                _pad2: 0.0,
                            },
                        })
                    })
                    .clone();
                commands
                    .spawn((
                        PlanGhost,
                        DespawnOnExit(AppState::InGame),
                        Mesh3d(cube.0.clone()),
                        MeshMaterial3d(material),
                        NotShadowCaster,
                        Transform {
                            translation: cell.as_vec3() + Vec3::splat(0.5),
                            // 1% inflation floats the shell ~5 mm off
                            // the real faces — kills z-fighting without
                            // a depth bias.
                            scale: Vec3::splat(1.01),
                            ..default()
                        },
                        Visibility::Visible,
                        Name::new("plan_hatch"),
                    ))
                    .id()
            }
            OverlayDesired::Build {
                slot,
                orientation,
                satisfied,
            } => {
                let def = registry.def(slot);
                if let Some(mesh_path) = def.mesh.as_ref() {
                    // Mesh block: the actual glTF scene, materials
                    // re-created as dithered ghosts by the shared
                    // WorldInstanceReady observer. Hidden until the
                    // swap lands (PreviewSceneReady), then revealed by
                    // `reveal_ready_ghost_scenes`.
                    let tint = if satisfied { Vec4::ONE } else { WAITING_TINT };
                    let scene: Handle<WorldAsset> =
                        asset_server.load(format!("{mesh_path}#Scene0"));
                    commands
                        .spawn((
                            PlanGhost,
                            DespawnOnExit(AppState::InGame),
                            WorldAssetRoot(scene),
                            GhostMeshStyle {
                                front: DitherParams::faded(
                                    tint,
                                    GHOST_COVERAGE,
                                    GHOST_MIN_COVERAGE,
                                    FADE_START,
                                    FADE_END,
                                ),
                                xray: DitherParams::faded(
                                    tint,
                                    GHOST_XRAY_COVERAGE,
                                    GHOST_XRAY_MIN_COVERAGE,
                                    FADE_START,
                                    FADE_END,
                                ),
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
                    // Voxel block: shared cube spanning the rotated
                    // footprint, compositing the block's real texture.
                    let (front, xray) = materials
                        .by_key
                        .entry((slot, satisfied))
                        .or_insert_with(|| {
                            (
                                ghost_mats.add(ghost_block_material(
                                    &textures,
                                    committed_ghost_params(slot, satisfied, false),
                                    false,
                                )),
                                ghost_mats.add(ghost_block_material(
                                    &textures,
                                    committed_ghost_params(slot, satisfied, true),
                                    true,
                                )),
                            )
                        })
                        .clone();
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
                            DespawnOnExit(AppState::InGame),
                            Transform {
                                translation: centre,
                                scale: extents,
                                ..default()
                            },
                            Visibility::Visible,
                            Name::new(format!("plan_ghost_cube:{}", def.id)),
                        ))
                        .with_children(|root| {
                            root.spawn((
                                Mesh3d(cube.0.clone()),
                                MeshMaterial3d(front.clone()),
                                NotShadowCaster,
                            ));
                            root.spawn((
                                Mesh3d(cube.0.clone()),
                                MeshMaterial3d(xray.clone()),
                                NotShadowCaster,
                            ));
                        })
                        .id()
                }
            }
        };
        index.by_cell.insert(
            cell,
            GhostEntry {
                entity,
                desired: want,
            },
        );
    }
}

/// Flip a ghost scene visible once the material-swap observer marks it
/// ready. Cube ghosts and hatches spawn visible directly (no asset
/// wait).
fn reveal_ready_ghost_scenes(
    mut ghosts: Query<&mut Visibility, (With<PlanGhost>, Added<PreviewSceneReady>)>,
) {
    for mut visibility in ghosts.iter_mut() {
        *visibility = Visibility::Visible;
    }
}
