//! Per-frame wireframe outline on the world cell under the crosshair.
//!
//! Independent of mode in *behaviour* (always drawn when the raycast
//! hits something) but tinted by [`PlayerMode`] so the player gets a
//! quick read on what the next click will do:
//!
//! - Select: white — neutral inspect target
//! - Plan: yellow — about to tag (or untag)
//! - Build: green — about to place
//! - Destroy: red — about to break
//!
//! Drawn with Bevy gizmos rather than a spawned wireframe entity. Cheap
//! enough to redo every frame; saves us a Transform-sync pass and the
//! visibility/material bookkeeping a real entity would need. If the
//! gizmo line width ever feels too thin we can promote to a real mesh.

use bevy::prelude::*;

use crate::blocks::BlockRegistry;
use crate::client::{RAYCAST_REACH, entity_aware_raycast};
use crate::menu::AppState;
use crate::plans::{PlanDragState, Plans};
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
/// player can see their queue. Visible in every mode, not just Plan —
/// once you're in Build or Destroy you still want to see what your
/// villagers are about to work on. Red for Remove, green for Build.
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

fn draw_target_outline(
    mode: Res<PlayerMode>,
    keys: Res<ButtonInput<KeyCode>>,
    drag: Res<PlanDragState>,
    cam: Query<&GlobalTransform, With<Camera3d>>,
    chunks: Query<(&Chunk, &ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
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
    let Some(hit) = entity_aware_raycast(origin, dir, RAYCAST_REACH, &chunks, &chunk_map, &registry)
    else {
        return;
    };

    // Outline cell = where the next click's *result* lands. For builder
    // semantics that's the empty cell on the outward face in Build mode,
    // and in Plan mode when Shift is held (= tag-build verb). For
    // Destroy / plain-L Plan / Select, the next click acts on the cell
    // under the cursor itself.
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    let target = match *mode {
        PlayerMode::Build => hit.cell + hit.face_normal,
        PlayerMode::Plan if shift => hit.cell + hit.face_normal,
        _ => hit.cell,
    };

    // Voxel cells are unit cubes with `cell` as the integer min corner.
    // Gizmos::cube uses the transform origin as the cube *centre*, so
    // shift by half a unit on each axis to land on the cell.
    let centre = target.as_vec3() + Vec3::splat(0.5);
    let colour = match *mode {
        PlayerMode::Select => Color::srgb(1.0, 1.0, 1.0),
        PlayerMode::Plan => Color::srgb(1.0, 0.85, 0.2),
        PlayerMode::Build => Color::srgb(0.3, 1.0, 0.4),
        PlayerMode::Destroy => Color::srgb(1.0, 0.35, 0.3),
    };
    gizmos.cube(Transform::from_translation(centre), colour);
}
