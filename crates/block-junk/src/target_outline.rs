//! Per-frame wireframe outline on the world cell under the crosshair.
//!
//! Independent of mode in *behaviour* (always drawn when there's an
//! actionable target) but tinted to read what the next click would do.
//! See [[player-input-scheme-l-click-primary]] for the canonical
//! verb-by-target table — this module is the visible half of that
//! contract.
//!
//! Phase-0 palette (more colours land as Phase 1/3 features add their
//! verbs):
//!
//! - **Plan mode**:
//!   - Destroy slot → red (tag-for-remove)
//!   - Block slot   → green (tag-for-build, on the outward-face cell)
//!
//! - **Normal mode**:
//!   - Cursor on a tagged plan cell (Remove or Build) → orange
//!     (L-click holds to self-work)
//!   - Cursor on any other solid block → red
//!     (R-click holds to direct-destroy)
//!   - Otherwise: no outline
//!
//! Drawn with Bevy gizmos rather than a spawned wireframe entity. Cheap
//! enough to redo every frame; saves us a Transform-sync pass and the
//! visibility/material bookkeeping a real entity would need.

use bevy::prelude::*;

use crate::blocks::BlockRegistry;
use crate::client::{
    PlaceablePalette, RAYCAST_REACH, SelectedBlock, entity_aware_raycast,
};
use crate::menu::AppState;
use crate::plans::{PlanDragState, Plans, raycast_plans};
use crate::player_mode::PlayerMode;
use crate::protocol::{GameSet, PlanKind};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap};

pub struct TargetOutlinePlugin;

impl Plugin for TargetOutlinePlugin {
    fn build(&self, app: &mut App) {
        // PostSimulation runs after physics + frame interpolation, so the
        // camera GlobalTransform is the value the renderer will actually
        // use this frame. Drawing the gizmo from earlier in the schedule
        // would lag the cursor by one frame.
        app.add_systems(
            Update,
            draw_target_outline
                .in_set(GameSet::PostSimulation)
                .run_if(in_state(AppState::InGame)),
        );
        app.add_systems(
            Update,
            draw_plan_outlines
                .in_set(GameSet::PostSimulation)
                .run_if(in_state(AppState::InGame)),
        );
    }
}

/// Render every tagged cell as a persistent gizmo wireframe so the
/// player can see their queue. Visible in every mode — once you're in
/// Normal you still want to see what your villagers are about to work
/// on. Red for Remove, green for Build.
fn draw_plan_outlines(plans: Res<Plans>, mut gizmos: Gizmos) {
    for (cell, kind) in plans.iter() {
        let centre = cell.as_vec3() + Vec3::splat(0.5);
        let colour = match kind {
            PlanKind::Remove => Color::srgb(1.0, 0.2, 0.2),
            PlanKind::Build { .. } => Color::srgb(0.2, 1.0, 0.4),
        };
        gizmos.cube(Transform::from_translation(centre), colour);
    }
}

#[allow(clippy::too_many_arguments, reason = "outline pulls from many resources")]
fn draw_target_outline(
    mode: Res<PlayerMode>,
    drag: Res<PlanDragState>,
    cam: Query<&GlobalTransform, With<Camera3d>>,
    chunks: Query<(&Chunk, &ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    plans: Res<Plans>,
    selected: Res<SelectedBlock>,
    palette: Res<PlaceablePalette>,
    mut gizmos: Gizmos,
) {
    // During an in-flight Plan-mode drag the rectangle preview is the
    // authoritative indicator; the cursor wireframe would just chase
    // background blocks and add noise. The drag preview gizmo takes
    // over until release.
    if drag.active.is_some() {
        return;
    }
    let Ok(cam_t) = cam.single() else {
        return;
    };
    let origin = cam_t.translation();
    let dir = *cam_t.forward();

    match *mode {
        PlayerMode::Plan => draw_plan_target(
            origin, dir, &chunks, &chunk_map, &registry, &plans, &selected, &palette, &mut gizmos,
        ),
        PlayerMode::Normal => draw_normal_target(
            origin, dir, &chunks, &chunk_map, &registry, &plans, &mut gizmos,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_plan_target(
    origin: Vec3,
    dir: Vec3,
    chunks: &Query<(&Chunk, &ChunkEntities)>,
    chunk_map: &ChunkMap,
    registry: &BlockRegistry,
    plans: &Plans,
    selected: &SelectedBlock,
    palette: &PlaceablePalette,
    gizmos: &mut Gizmos,
) {
    // Plan-mode Remove sees through already-tagged cells so the cursor
    // can preview a Remove behind another Remove tag — same trick
    // `plan_mode_input` uses on the actual press.
    let destroy_selected = selected.current_block(palette).is_none();
    let skip_plan_remove = if destroy_selected { Some(plans) } else { None };
    let Some(hit) = entity_aware_raycast(
        origin,
        dir,
        RAYCAST_REACH,
        chunks,
        chunk_map,
        registry,
        skip_plan_remove,
    ) else {
        return;
    };
    // Outline cell = where the tag would land. Build (block slot)
    // lands on the outward-face neighbour; Remove (Destroy slot)
    // tags the cell under the cursor.
    let target = if destroy_selected {
        hit.cell
    } else {
        hit.cell + hit.face_normal
    };
    let colour = if destroy_selected {
        Color::srgb(1.0, 0.35, 0.3)
    } else {
        Color::srgb(0.3, 1.0, 0.4)
    };
    draw_cell(gizmos, target, colour);
}

fn draw_normal_target(
    origin: Vec3,
    dir: Vec3,
    chunks: &Query<(&Chunk, &ChunkEntities)>,
    chunk_map: &ChunkMap,
    registry: &BlockRegistry,
    plans: &Plans,
    gizmos: &mut Gizmos,
) {
    // Two candidate targets, same logic as `resolve_self_work` uses:
    // either the world-raycast cell is itself tagged (Remove on solid)
    // or a plan-raycast picks up a Build tag in empty space. Whichever
    // is closer to the camera wins (tie → world).
    let world_hit = entity_aware_raycast(
        origin, dir, RAYCAST_REACH, chunks, chunk_map, registry, None,
    );
    let world_tagged = world_hit.as_ref().and_then(|h| {
        plans.get(h.cell).map(|_| {
            let dist = (h.cell.as_vec3() + Vec3::splat(0.5) - origin).length();
            (dist, h.cell)
        })
    });
    let plan_hit = raycast_plans(origin, dir, RAYCAST_REACH, plans);

    let tagged_target = match (world_tagged, plan_hit) {
        (Some(a), Some(b)) if a.0 <= b.0 => Some(a.1),
        (Some(_), Some(b)) => Some(b.1),
        (Some(a), None) => Some(a.1),
        (None, Some(b)) => Some(b.1),
        (None, None) => None,
    };
    if let Some(cell) = tagged_target {
        // Orange: L-click would self-work this cell.
        draw_cell(gizmos, cell, Color::srgb(1.0, 0.6, 0.1));
        return;
    }
    // No tag under the cursor; fall back to the world hit. Red means
    // R-click would direct-destroy whatever block is here.
    if let Some(hit) = world_hit {
        draw_cell(gizmos, hit.cell, Color::srgb(1.0, 0.35, 0.3));
    }
}

/// Voxel cells are unit cubes with `cell` as the integer min corner.
/// `Gizmos::cube` uses the transform origin as the cube *centre*, so
/// shift by half a unit on each axis to land on the cell.
fn draw_cell(gizmos: &mut Gizmos, cell: IVec3, colour: Color) {
    let centre = cell.as_vec3() + Vec3::splat(0.5);
    gizmos.cube(Transform::from_translation(centre), colour);
}
