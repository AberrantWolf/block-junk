use core::time::Duration;

use bevy::animation::{AnimatedBy, AnimationTargetId};
use bevy::input::mouse::AccumulatedMouseScroll;
use bevy::platform::collections::{HashMap, HashSet};
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use block_junk_mod_api::blocks::Cardinal;
use lightyear::prelude::*;

use bevy::scene::SceneInstanceReady;

use lightyear::frame_interpolation::prelude::*;
use lightyear::input::native::prelude::*;

use crate::block_textures::{BlockTextures, BlockTextureExt, BlockTexturesPlugin, ChunkMaterial};
use crate::blocks::{BlockRegistry, BlockSlot, TerrainSlots};
use crate::camera::{FlyCam, FlyCamPlugin};
use crate::collision::WorldCollision;
use crate::menu::AppState;
use crate::npc::{Npc, NpcId, NpcKind, NpcPath};
use crate::npc_registry::NpcKindRegistry;
use crate::physics::{
    EYE_OFFSET_FROM_CENTRE, PLAYER_HALF_EXTENTS, apply_separation_push_swept, apply_walk_step,
    compute_actor_separation_pushes, rescue_embedded_actor,
};
use crate::inspect_panel::InspectPanelPlugin;
use crate::plans::{Plans, PlansClientPlugin};
use crate::player_mode::{PlayerMode, PlayerModePlugin};
use crate::preview::{PreviewBack, PreviewFront, PreviewPlugin};
use crate::target_outline::TargetOutlinePlugin;
use crate::items::{ItemRegistry, ItemSlot, PLAYER_CARRY_CAPACITY};
use crate::protocol::{
    Actor, Avatar, AvatarOnGround, AvatarPose, AvatarVelocity, BlockEdit, BlockManifest,
    Carrying, ChunkCoord, ChunkData, ChunkSnapshot, ChunkUnload, DepositRequest, DropRequest,
    EquippedTool, GameSet, MovementIntent, MovementMode, NpcAnimOverride, PickupRequest, PlanKind,
    WorldChannel, WorldClock, WorldClockSync, WorldItem,
};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap, EntryKind};

pub struct ClientPlugin;

impl Plugin for ClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(FlyCamPlugin)
            .add_plugins(PreviewPlugin)
            .add_plugins(PlayerModePlugin)
            .add_plugins(TargetOutlinePlugin)
            .add_plugins(PlansClientPlugin)
            .add_plugins(InspectPanelPlugin)
            // Frame interpolation smooths AvatarPose between FixedUpdate
            // ticks during PostUpdate render. Without it, on a high-refresh
            // display you see 64 Hz physics steps with the renderer drawing
            // the same position for multiple frames between ticks.
            .add_plugins(FrameInterpolationPlugin::<AvatarPose>::default())
            .add_plugins(crate::scripting::ClientScriptingPlugin)
            // BlockTexturesPlugin reads BlockRegistry in its build() to
            // generate one 16×16 procedural texture per slot, so it must
            // run after ClientScriptingPlugin (which inserts the registry).
            .add_plugins(BlockTexturesPlugin)
            .add_plugins(crate::debug::DebugClientPlugin);
        // ClientScriptingPlugin inserts BlockRegistry. Derive client-side
        // resources from it.
        let palette = {
            let reg = app.world().resource::<BlockRegistry>();
            // Destroy lives at index 0 so 1-key intuition (`top of the
            // bar = break`) matches the visual ordering.
            let mut entries: Vec<PaletteSlot> = vec![PaletteSlot::Destroy];
            entries.extend(reg.iter_placeable().map(PaletteSlot::Block));
            PlaceablePalette(entries)
        };
        let terrain_slots = TerrainSlots::from_registry(app.world().resource::<BlockRegistry>());
        app.insert_resource(palette);
        app.insert_resource(terrain_slots);
        app.init_resource::<ChunkMap>()
            .init_resource::<SelectedBlock>()
            .init_resource::<PlacementRotation>()
            .init_resource::<PlayerActionState>()
            .init_resource::<BlockEntities>()
            .init_resource::<PreviewState>()
            // Client mirror of the server's day/night clock. Starts at
            // 0.25 (sunrise) so the first frame after entering a session
            // — before any WorldClockSync arrives — has the world lit
            // rather than at midnight black.
            .insert_resource(WorldClock {
                day: 0,
                time_of_day: 0.25,
            })
            .add_observer(swap_preview_scene_materials)
            .add_observer(setup_npc_skeleton_anim)
            .add_observer(attach_world_item_visuals)
            // Scene setup runs when entering a game, not at process start.
            // Before InGame the screen is the main menu only.
            .add_systems(
                OnEnter(AppState::InGame),
                (setup_scene, setup_placement_preview),
            )
            .add_systems(
                Update,
                (
                    normal_mode_action_input,
                    drop_carry_input,
                    cycle_selected_or_rotation,
                    reset_rotation_on_selection_change,
                )
                    .in_set(GameSet::Input),
            )
            // Input replication: ActionState<MovementIntent> on the predicted
            // avatar gets written from the keyboard each fixed tick.
            // WriteClientInputs is the lightyear-defined set that ensures
            // the input is buffered before the simulation reads it.
            .add_systems(
                FixedPreUpdate,
                buffer_input
                    .in_set(client::input::InputSystems::WriteClientInputs)
                    .run_if(in_state(AppState::InGame)),
            )
            // Owner-side prediction: run the same controller the server
            // runs, against the same inputs, so we don't wait for the
            // server's reply to see ourselves move. Lightyear rolls back
            // and replays this when it receives a server correction.
            //
            // Soft actor separation runs after the predicted step so
            // the local player's prediction matches the server's
            // outcome when walking into NPCs (server runs the same
            // pass post-physics on its side). The client variant
            // only writes to *Predicted* actors — interpolated
            // remotes contribute to the pairwise push direction
            // (so the predicted owner gets pushed away from them
            // correctly) but their pose is left to the next
            // server snapshot, since locally mutating an
            // interpolated pose drifts the rendered position away
            // from the authoritative one tick by tick.
            .add_systems(
                FixedUpdate,
                (client_player_step, soft_separate_predicted_actors)
                    .chain()
                    .run_if(in_state(AppState::InGame)),
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
                    receive_world_clock,
                )
                    .chain()
                    .in_set(GameSet::Simulation),
            )
            .add_systems(
                Update,
                (advance_local_clock, update_day_night_lighting)
                    .chain()
                    .in_set(GameSet::PostSimulation)
                    .run_if(in_state(AppState::InGame)),
            )
            // Avatar transform sync runs in PostUpdate after frame
            // interpolation, so the camera Transform we hand to the
            // renderer is the smoothed value (lerped between the prior
            // and current fixed tick by the render-frame overstep).
            .add_systems(
                PostUpdate,
                sync_avatar_transforms
                    .after(FrameInterpolationSystems::Interpolate)
                    .run_if(in_state(AppState::InGame)),
            )
            // Split into multiple add_systems calls — Bevy 0.18 hits a
            // trait-resolution cap when the tuple's combined system
            // signatures grow large. `update_placement_preview` carries
            // ~17 params; lumping it with anything else here pushes us
            // over. Same workaround as the server's Simulation set.
            .add_systems(
                Update,
                update_placement_preview
                    .run_if(show_placement_preview)
                    .in_set(GameSet::PostSimulation),
            )
            .add_systems(
                Update,
                hide_preview_on_mode_change.in_set(GameSet::PostSimulation),
            )
            .add_systems(
                Update,
                (
                    mesh_chunks,
                    refresh_block_entities,
                    update_hotbar_highlight,
                    update_hotbar_visibility,
                    update_carry_hud,
                    update_tool_hud,
                    update_action_progress_ui,
                    draw_npc_paths,
                )
                    .in_set(GameSet::PostSimulation),
            )
            .add_systems(
                Update,
                (
                    attach_avatar_visuals,
                    attach_npc_visuals,
                    attach_npc_carry_icons,
                    update_npc_carry_icons,
                    start_npc_anim_idle,
                    drive_npc_animation,
                )
                    .in_set(GameSet::PostSimulation),
            )
            // The owner's predicted avatar arrives via replication after
            // connect; this observer wires its camera, input marker, and
            // headlamp once it's there.
            .add_observer(handle_predicted_spawn);
    }
}

/// Pre-built mesh + material for replicated player avatars. NPCs use
/// `CharacterAssets` (skinned glTF) instead. Shared so the renderer
/// allocates GPU resources once per session, not per spawn.
#[derive(Resource)]
struct AvatarAssets {
    mesh: Handle<Mesh>,
    avatar_material: Handle<StandardMaterial>,
}

/// Skinned character mesh + animation graph for NPCs. Built once when
/// the first session enters InGame; survives pause/unpause via the
/// same existence-check that gates `AvatarAssets`.
///
/// Clips and the character mesh live in *separate* glbs that share
/// the same rig skeleton. They retarget cleanly because Bevy keys
/// animation targets by hashed bone path, not by the file they came
/// from. With the animation registry now data-driven, the engine
/// loads every registered clip into one unified graph at session
/// start and caches `AnimationId → AnimationNodeIndex` in
/// `clip_nodes` — `drive_npc_animation` looks up nodes by name
/// instead of dispatching on a hardcoded enum.
///
/// `knight_scene` is the visible body. We currently only ship one
/// rig variant; per-kind body meshes land later as a `NpcKindDef`
/// field alongside the animation set.
#[derive(Resource)]
struct CharacterAssets {
    knight_scene: Handle<Scene>,
    anim_graph: Handle<AnimationGraph>,
    /// Resolved clip → graph-node index for every registered
    /// [`AnimationId`](block_junk_mod_api::animations::AnimationId).
    /// Keyed by the id string the wire carries, so
    /// [`NpcAnimOverride`] + [`NpcKindAnimations`] lookups are O(1).
    clip_nodes: HashMap<String, AnimationNodeIndex>,
}

/// Knight body asset path. The animation clips live in separate glbs
/// declared in `mods/vanilla/data.lua` via `engine.animations.register`.
const KAYKIT_KNIGHT_GLB: &str = "mods://vanilla/models/characters/Knight.glb";

/// Tracks the ECS entity rendering each placed block-entity (a block
/// whose `BlockDef.mesh` is set, e.g. furniture, doors). Indexed by world
/// cell with a parallel per-chunk set so we can despawn an entire chunk's
/// block entities cheaply on `ChunkUnload`.
#[derive(Resource, Default)]
pub struct BlockEntities {
    by_cell: HashMap<IVec3, Entity>,
    by_chunk: HashMap<ChunkCoord, HashSet<IVec3>>,
}

/// One hotbar entry. `Destroy` is the synthetic top slot whose L-click
/// tags the cursor cell for removal in Plan mode. `Block` is a regular
/// placeable. (Pre-2026-05-18, Build mode used the same slot to break
/// blocks directly; Normal-mode direct-destroy is now R-click and
/// bypasses the hotbar entirely.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteSlot {
    Destroy,
    Block(BlockSlot),
}

impl PaletteSlot {
    pub fn block(self) -> Option<BlockSlot> {
        match self {
            PaletteSlot::Destroy => None,
            PaletteSlot::Block(slot) => Some(slot),
        }
    }
}

/// Hotbar entries shown to the player. Always begins with a synthetic
/// [`PaletteSlot::Destroy`] slot at index 0; the remaining entries are
/// pulled from [`BlockRegistry::iter_placeable`]. Built once at startup;
/// if/when mods can add blocks at runtime this will need invalidation.
#[derive(Resource)]
pub struct PlaceablePalette(pub Vec<PaletteSlot>);

/// Index into [`PlaceablePalette`] of the currently selected entry.
/// Mouse wheel cycles. Read only by Plan-mode tagging — Normal mode
/// drives its verb from cursor context, not the hotbar.
#[derive(Resource, Default)]
pub struct SelectedBlock(pub usize);

impl SelectedBlock {
    pub fn current(&self, palette: &PlaceablePalette) -> PaletteSlot {
        palette.0[self.0]
    }

    /// Convenience: the selected entry as a placeable block, or `None`
    /// if the synthetic [`PaletteSlot::Destroy`] slot is selected.
    pub fn current_block(&self, palette: &PlaceablePalette) -> Option<BlockSlot> {
        self.current(palette).block()
    }
}

/// Manual orientation offset applied on top of the player's facing-derived
/// orientation at place time. Ctrl+MouseWheel advances/retreats one
/// cardinal step. Reset to the default ([`Cardinal::East`]) whenever the
/// hotbar selection changes — orientation context is per-item, so picking
/// a new item shouldn't carry forward the previous item's rotation.
#[derive(Resource, Default)]
pub struct PlacementRotation(pub Cardinal);

/// In-flight player action in Normal mode (L-click self-work or R-click
/// direct-destroy). The timer ticks up while the chosen button is held
/// against a stable target; at 1.0 the underlying `BlockEdit` is sent.
/// Releasing the button or aiming at a different cell drops the state
/// and the next frame starts fresh from 0.
#[derive(Resource, Default)]
pub struct PlayerActionState {
    pub active: Option<ActiveAction>,
}

#[derive(Clone, Copy, Debug)]
pub struct ActiveAction {
    pub target_cell: IVec3,
    pub kind: ActionKind,
    /// Normalised 0.0 → 1.0; reaches 1.0 in [`PLAYER_ACTION_DURATION_SECS`]
    /// of held time on the same target.
    pub progress: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Place,
    Break,
}

/// Real seconds to complete one Build-mode action (place or break) with
/// the timer path. Picked so a sweep across a 3-block-wide wall feels
/// deliberate without being tedious — tune as the feature settles.
pub const PLAYER_ACTION_DURATION_SECS: f32 = 0.6;

#[derive(Component)]
struct HotbarSlot(usize);

/// Marker on the absolute-positioned root Node holding the hotbar
/// column. Used to flip the whole column's `Visibility` in modes that
/// don't read from the hotbar (Normal).
#[derive(Component)]
struct HotbarRoot;

#[derive(Component)]
struct ActionProgressBar;

#[derive(Component)]
struct ActionProgressFill;

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
    mut anim_graphs: ResMut<Assets<AnimationGraph>>,
    asset_server: Res<AssetServer>,
    palette: Res<PlaceablePalette>,
    textures: Res<BlockTextures>,
    animations: Res<crate::npc_registry::AnimationRegistry>,
    existing: Option<Res<AvatarAssets>>,
) {
    // `OnEnter(InGame)` re-fires on every un-pause, but the scene
    // (lights, crosshair, hotbar UI, avatar meshes) outlives pause.
    // Re-running would spawn duplicate lights and a second hotbar UI.
    if existing.is_some() {
        return;
    }

    // Default ambient (80) leaves shadowed faces near-black. Bumping it
    // floods all surfaces with enough light to read geometry.
    ambient.brightness = 250.0;

    // Single shared cuboid + material for all remote-player avatars. Roughly
    // Minecraft proportions (0.6×1.8×0.6 m), centred on the avatar's
    // Transform so the world position matches the camera-eye height that
    // the owner reports to the server.
    commands.insert_resource(AvatarAssets {
        mesh: meshes.add(Cuboid::new(0.6, 1.8, 0.6)),
        avatar_material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.95, 0.55, 0.25),
            perceptual_roughness: 0.6,
            ..default()
        }),
    });

    // Build the unified animation graph from every registered
    // AnimationDef. The registry is populated by the scripting layer
    // at app build time — vanilla's data.lua calls
    // `engine.animations.register` for each rig clip, and mods add
    // their own. Order matches the registry's insertion order so
    // clip_nodes is deterministic across runs.
    let mut clip_handles: Vec<Handle<AnimationClip>> = Vec::with_capacity(animations.len());
    let mut clip_ids: Vec<String> = Vec::with_capacity(animations.len());
    for (id, def) in animations.iter() {
        let path = def.asset.clone();
        clip_handles.push(
            asset_server.load(GltfAssetLabel::Animation(def.clip_index as usize).from_asset(path)),
        );
        clip_ids.push(id.clone());
    }
    let (anim_graph, node_indices) = AnimationGraph::from_clips(clip_handles);
    let clip_nodes: HashMap<String, AnimationNodeIndex> =
        clip_ids.into_iter().zip(node_indices.into_iter()).collect();
    commands.insert_resource(CharacterAssets {
        knight_scene: asset_server
            .load(GltfAssetLabel::Scene(0).from_asset(KAYKIT_KNIGHT_GLB)),
        anim_graph: anim_graphs.add(anim_graph),
        clip_nodes,
    });


    // Camera + a point "headlamp" so the player can read shapes in the
    // shadow of nearby geometry without needing to fly around to find
    // a light angle that works.
    // The camera is no longer a free-floating local entity — it's
    // attached to the predicted avatar via `handle_predicted_spawn` once
    // the server replicates it. Until then (a few ms in solo mode, up
    // to ~200 ms over the network) the screen has no active 3D camera.

    // Sun light. `update_day_night_lighting` rotates it around X every
    // frame from the world clock; initial transform is "noon" so the
    // first frame before the first tick of the lighting system isn't
    // visibly off. Tagged with `SunLight` so the lighting system can
    // find it without relying on entity ordering.
    commands.spawn((
        DirectionalLight {
            color: Color::WHITE,
            illuminance: 10_000.0,
            shadows_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(
            EulerRot::XYZ,
            -std::f32::consts::FRAC_PI_2,
            0.0,
            0.0,
        )),
        SunLight,
    ));
    // Back light. Static cool fill that softens the side of geometry the
    // sun isn't on. Doesn't track the sun — it would just fight the key
    // light if it did. Lower illuminance so it never out-shines daylight.
    commands.spawn((
        DirectionalLight {
            color: Color::srgb(0.75, 0.85, 1.0),
            illuminance: 3_000.0,
            shadows_enabled: false,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, 0.5, 2.6, 0.0)),
    ));

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

    // Action-progress bar: shown while an L-hold Build action is in
    // flight (place or break). Absolute-anchored at screen centre + a
    // 14 px gap below the crosshair; negative left margin = half the
    // bar width so the centre lines up with the crosshair.
    commands
        .spawn((
            ActionProgressBar,
            Node {
                position_type: PositionType::Absolute,
                top: Val::Percent(50.0),
                left: Val::Percent(50.0),
                margin: UiRect {
                    top: Val::Px(14.0),
                    left: Val::Px(-40.0),
                    ..default()
                },
                width: Val::Px(80.0),
                height: Val::Px(6.0),
                border: UiRect::all(Val::Px(1.0)),
                ..default()
            },
            BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.7)),
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.4)),
            Visibility::Hidden,
        ))
        .with_children(|bar| {
            bar.spawn((
                ActionProgressFill,
                Node {
                    width: Val::Percent(0.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                BackgroundColor(Color::srgb(0.95, 0.95, 0.95)),
            ));
        });

    // Hotbar on the right edge: vertical column of slots. Selected slot
    // gets a white border via update_hotbar_highlight. The inner image
    // is the block's procedural 16×16 texture rendered at 32×32 with
    // nearest-neighbour sampling (configured on the source `Image` in
    // BlockTexturesPlugin) so the pattern reads as crisp pixel art.
    // The synthetic Destroy slot at index 0 carries the pickaxe icon
    // and a subtle red tint instead.
    let destroy_icon: Handle<Image> = asset_server.load("ui/mode_icons/tool_pickaxe.png");
    commands
        .spawn((
            HotbarRoot,
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                position_type: PositionType::Absolute,
                justify_content: JustifyContent::FlexEnd,
                align_items: AlignItems::Center,
                padding: UiRect::right(Val::Px(20.0)),
                ..default()
            },
        ))
        .with_children(|root| {
            root.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(4.0),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|column| {
                // Scroll-wheel hint sits at the top of the column so it
                // visually anchors what the cycling acts on.
                column
                    .spawn((
                        Node {
                            padding: UiRect::axes(Val::Px(6.0), Val::Px(2.0)),
                            margin: UiRect::bottom(Val::Px(2.0)),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            border: UiRect::all(Val::Px(1.0)),
                            border_radius: BorderRadius::all(Val::Px(3.0)),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(0.05, 0.05, 0.05, 0.72)),
                        BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.25)),
                    ))
                    .with_children(|cap| {
                        cap.spawn((
                            Text::new("scroll"),
                            TextFont {
                                font_size: 12.0,
                                ..default()
                            },
                            TextColor(Color::srgba(0.9, 0.9, 0.9, 1.0)),
                        ));
                    });
                for (i, entry) in palette.0.iter().enumerate() {
                    let (icon, bg) = match entry {
                        PaletteSlot::Destroy => (
                            destroy_icon.clone(),
                            Color::srgba(0.35, 0.10, 0.10, 0.7),
                        ),
                        PaletteSlot::Block(slot) => (
                            textures.icons[slot.0 as usize].clone(),
                            Color::srgba(0.1, 0.1, 0.1, 0.6),
                        ),
                    };
                    column
                        .spawn((
                            Node {
                                width: Val::Px(44.0),
                                height: Val::Px(44.0),
                                border: UiRect::all(Val::Px(2.0)),
                                justify_content: JustifyContent::Center,
                                align_items: AlignItems::Center,
                                ..default()
                            },
                            BorderColor::all(Color::BLACK),
                            BackgroundColor(bg),
                            HotbarSlot(i),
                        ))
                        .with_children(|slot_parent| {
                            slot_parent.spawn((
                                ImageNode::new(icon),
                                Node {
                                    width: Val::Px(32.0),
                                    height: Val::Px(32.0),
                                    ..default()
                                },
                            ));
                        });
                }
            });
        });

    spawn_carry_hud(&mut commands);
    spawn_tool_hud(&mut commands);
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
    mode: Res<PlayerMode>,
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
    // Wheel only cycles blocks in Plan mode (where the selection picks
    // what to tag); Normal has no hotbar visible.
    if !matches!(*mode, PlayerMode::Plan) {
        return;
    }
    let dy = scroll.delta.y;
    if dy.abs() < 0.5 {
        return;
    }
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    if ctrl {
        // No orientation to rotate when the Destroy slot is selected;
        // swallow the wheel rather than spinning a meaningless angle.
        if selected.current_block(&palette).is_none() {
            return;
        }
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

/// Root of the bottom-centre carry HUD chip. Visibility is Hidden when
/// the player isn't carrying anything; otherwise Inherited.
#[derive(Component)]
struct CarryHudRoot;

/// Marker on the colour swatch — `BackgroundColor` is updated to the
/// item def's `color` on every carry change.
#[derive(Component)]
struct CarryHudIcon;

/// Marker on the `n / cap` text. Updated whenever the carry count
/// changes.
#[derive(Component)]
struct CarryHudLabel;

fn spawn_carry_hud(commands: &mut Commands) {
    // Full-width row centred horizontally; the actual visible chip is
    // a child that sizes to its content.
    commands
        .spawn((
            CarryHudRoot,
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(24.0),
                width: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                ..default()
            },
            Visibility::Hidden,
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    padding: UiRect::axes(Val::Px(10.0), Val::Px(6.0)),
                    column_gap: Val::Px(8.0),
                    align_items: AlignItems::Center,
                    border: UiRect::all(Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Px(6.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.08, 0.08, 0.08, 0.78)),
                BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.25)),
            ))
            .with_children(|chip| {
                chip.spawn((
                    CarryHudIcon,
                    Node {
                        width: Val::Px(28.0),
                        height: Val::Px(28.0),
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(3.0)),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.5, 0.5, 0.5, 1.0)),
                    BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.35)),
                ));
                chip.spawn((
                    CarryHudLabel,
                    Text::new(""),
                    TextFont {
                        font_size: 16.0,
                        ..default()
                    },
                    TextColor(Color::WHITE),
                ));
            });
        });
}

/// Marker for the tool HUD root (matches CarryHudRoot's role for the
/// carry chip). Sits *above* the carry chip so a full carry + tool
/// loadout reads as two stacked chips at the bottom-centre.
#[derive(Component)]
struct ToolHudRoot;

#[derive(Component)]
struct ToolHudIcon;

#[derive(Component)]
struct ToolHudLabel;

fn spawn_tool_hud(commands: &mut Commands) {
    commands
        .spawn((
            ToolHudRoot,
            Node {
                position_type: PositionType::Absolute,
                // 64 px above the carry chip (which sits at 24 px).
                // Two vertically-stacked chips with breathing room.
                bottom: Val::Px(64.0),
                width: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                ..default()
            },
            Visibility::Hidden,
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    padding: UiRect::axes(Val::Px(10.0), Val::Px(6.0)),
                    column_gap: Val::Px(8.0),
                    align_items: AlignItems::Center,
                    border: UiRect::all(Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Px(6.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.08, 0.08, 0.08, 0.78)),
                BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.25)),
            ))
            .with_children(|chip| {
                chip.spawn((
                    ToolHudIcon,
                    Node {
                        width: Val::Px(28.0),
                        height: Val::Px(28.0),
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(3.0)),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.5, 0.5, 0.5, 1.0)),
                    BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.35)),
                ));
                chip.spawn((
                    ToolHudLabel,
                    Text::new(""),
                    TextFont {
                        font_size: 16.0,
                        ..default()
                    },
                    TextColor(Color::WHITE),
                ));
            });
        });
}

/// Mirror the local player's `EquippedTool` onto the tool HUD chip.
/// Same shape as `update_carry_hud` — reads from the Predicted avatar,
/// hides when the slot is empty, tints + labels otherwise.
fn update_tool_hud(
    local_tool: Query<&EquippedTool, (With<Avatar>, With<Predicted>)>,
    items: Res<ItemRegistry>,
    mut roots: Query<&mut Visibility, With<ToolHudRoot>>,
    mut icons: Query<&mut BackgroundColor, With<ToolHudIcon>>,
    mut labels: Query<&mut Text, With<ToolHudLabel>>,
) {
    let Ok(tool) = local_tool.single() else {
        for mut v in roots.iter_mut() {
            *v = Visibility::Hidden;
        }
        return;
    };
    let (vis, swatch, text) = match tool.item {
        Some(slot) => {
            let def = items.def(slot);
            let [r, g, b] = def.color;
            (
                Visibility::Inherited,
                Color::srgba(r, g, b, 1.0),
                def.display_name.clone(),
            )
        }
        None => (Visibility::Hidden, Color::srgba(0.5, 0.5, 0.5, 1.0), String::new()),
    };
    for mut v in roots.iter_mut() {
        *v = vis;
    }
    for mut bg in icons.iter_mut() {
        bg.0 = swatch;
    }
    for mut t in labels.iter_mut() {
        t.0 = text.clone();
    }
}

/// Mirror the local player's `Carrying` onto the HUD chip. Reads from
/// the Predicted avatar (the local player's authoritative copy on the
/// client side). Hides the chip when empty-handed.
fn update_carry_hud(
    local_carry: Query<&Carrying, (With<Avatar>, With<Predicted>)>,
    items: Res<ItemRegistry>,
    mut roots: Query<&mut Visibility, With<CarryHudRoot>>,
    mut icons: Query<&mut BackgroundColor, With<CarryHudIcon>>,
    mut labels: Query<&mut Text, With<CarryHudLabel>>,
) {
    // No predicted avatar yet → nothing to show.
    let Ok(carry) = local_carry.single() else {
        for mut v in roots.iter_mut() {
            *v = Visibility::Hidden;
        }
        return;
    };
    let (vis, swatch, text) = match (carry.item, carry.count) {
        (Some(slot), count) if count > 0 => {
            let def = items.def(slot);
            let [r, g, b] = def.color;
            let label = format!(
                "{}  {}/{}",
                def.display_name, count, PLAYER_CARRY_CAPACITY
            );
            (
                Visibility::Inherited,
                Color::srgba(r, g, b, 1.0),
                label,
            )
        }
        _ => (Visibility::Hidden, Color::srgba(0.5, 0.5, 0.5, 1.0), String::new()),
    };
    for mut v in roots.iter_mut() {
        *v = vis;
    }
    for mut bg in icons.iter_mut() {
        bg.0 = swatch;
    }
    for mut t in labels.iter_mut() {
        t.0 = text.clone();
    }
}

/// Show/hide the action progress bar and resize the fill child to
/// match `PlayerActionState.progress`. Visibility flips on the frame
/// the action starts or ends; the fill width updates every frame an
/// action is in flight.
fn update_action_progress_ui(
    action: Res<PlayerActionState>,
    mut bars: Query<&mut Visibility, With<ActionProgressBar>>,
    mut fills: Query<&mut Node, With<ActionProgressFill>>,
) {
    if !action.is_changed() {
        return;
    }
    let (vis, progress) = match action.active {
        Some(a) => (Visibility::Inherited, a.progress),
        None => (Visibility::Hidden, 0.0),
    };
    for mut v in bars.iter_mut() {
        *v = vis;
    }
    for mut node in fills.iter_mut() {
        node.width = Val::Percent(progress.clamp(0.0, 1.0) * 100.0);
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

/// Hide the hotbar column in Normal — the L-click verb in that mode
/// doesn't read the hotbar, so the chip would just be noise. Plan
/// drives its tagging off the hotbar selection and needs it visible.
///
/// Not gated by `mode.is_changed()` so the initial entry into the
/// scene (where the mode is the default Normal but the resource has
/// already been around since plugin build) still gets the right
/// Visibility on its first tick.
fn update_hotbar_visibility(
    mode: Res<PlayerMode>,
    mut roots: Query<&mut Visibility, With<HotbarRoot>>,
) {
    let target = match *mode {
        PlayerMode::Normal => Visibility::Hidden,
        PlayerMode::Plan => Visibility::Visible,
    };
    for mut v in roots.iter_mut() {
        if *v != target {
            *v = target;
        }
    }
}

/// Marker on the directional light that follows the world clock —
/// rotates around the world's east/west axis with `time_of_day` and is
/// dimmed/brightened to match the sun height. Letting the light find
/// itself by component query rather than `Resource<Entity>` keeps the
/// startup ordering loose (the lighting tick is a no-op until the
/// light spawns; we don't have to insert it before the first tick).
#[derive(Component)]
struct SunLight;

/// Receive replicated clock samples from the server. Snaps the local
/// `WorldClock` to the latest sample so any drift accumulated in
/// `advance_local_clock` is corrected each second. Out-of-order
/// delivery would cause the clock to step backwards, but the message
/// rides `WorldChannel` (ordered-reliable), so we only ever see
/// monotonically newer samples.
fn receive_world_clock(
    mut receivers: Query<&mut MessageReceiver<WorldClockSync>>,
    mut clock: ResMut<WorldClock>,
) {
    for mut receiver in receivers.iter_mut() {
        for sync in receiver.receive() {
            clock.day = sync.day;
            clock.time_of_day = sync.time_of_day;
        }
    }
}

/// Locally extrapolate the clock between server syncs so the sun's
/// rotation isn't visibly stepped at the 1 Hz sync cadence. Each
/// incoming `WorldClockSync` corrects whatever this drifted to.
fn advance_local_clock(time: Res<Time>, mut clock: ResMut<WorldClock>) {
    clock.advance(time.delta_secs());
}

/// Drive the sun light's rotation, the directional intensity, the
/// ambient brightness, and the sky colour from `WorldClock`. All four
/// are derived from one shared "sun angle" so the visual stays
/// coherent: sun overhead at noon, on the horizon at sunrise/sunset,
/// below ground at night with the lights dimmed to moonlight.
fn update_day_night_lighting(
    clock: Res<WorldClock>,
    mut ambient: ResMut<GlobalAmbientLight>,
    mut clear: ResMut<ClearColor>,
    mut sun: Query<(&mut Transform, &mut DirectionalLight), With<SunLight>>,
) {
    // Sun angle parameterised so `time_of_day` 0.25 → angle 0 (sunrise),
    // 0.5 → PI/2 (noon), 0.75 → PI (sunset), 0.0 → -PI/2 (midnight).
    let angle = (clock.time_of_day - 0.25) * core::f32::consts::TAU;
    // Height curve: -1 = directly below, 0 = horizon, +1 = overhead.
    // Matches `WorldClock::is_night` (height < 0 ⇔ night).
    let daylight = angle.sin().max(0.0);

    if let Ok((mut tf, mut light)) = sun.single_mut() {
        // Light direction: when the sun is overhead it points straight
        // down (rotation around X = -PI/2 puts the default -Z forward
        // pointing at -Y). At sunrise the light points horizontally
        // (rotation 0). The constant Y rotation (0.4 rad) gives the
        // shadow side a slight south/east bias so flat geometry still
        // reads as 3D rather than washing out at noon.
        let rot_x = -angle;
        tf.rotation = Quat::from_euler(EulerRot::XYZ, rot_x, 0.4, 0.0);
        // Direct sunlight scales with sun height. Night drops to 0 —
        // ambient + the static back light keep things readable.
        light.illuminance = 10_000.0 * daylight;
        // Disable shadow casting at low sun angles. The cascade-shadow
        // projection becomes extremely stretched when the sun is near
        // horizontal — small bias errors turn into visible stripes
        // across flat geometry. Below ~10° elevation the visual
        // benefit of shadows is small anyway (everything's dim),
        // so just drop them until the sun is well clear of the
        // horizon. Threshold 0.18 ≈ sin(10°).
        light.shadows_enabled = daylight > 0.18;
    }

    // Ambient floor at moonlight (30 lx) — pitch black is unplayable.
    // Ramps to the original flat-read value (250 lx) at noon.
    ambient.brightness = 30.0 + 220.0 * daylight;

    // Sky/clear colour: lerp between night blue and day blue. Sunset
    // warm tones would need a separate "twilight" channel; skip for
    // now since we don't have a skybox to layer it onto anyway.
    let day_sky = Vec3::new(0.55, 0.75, 1.0);
    let night_sky = Vec3::new(0.02, 0.03, 0.08);
    let mix = day_sky * daylight + night_sky * (1.0 - daylight);
    clear.0 = Color::srgb(mix.x, mix.y, mix.z);
}

/// Reach in world cells. Generous because the camera is a flying free-cam;
/// real survival reach (Minecraft-y ~5 blocks) lands when there's an avatar.
pub(crate) const RAYCAST_REACH: f32 = 256.0;

/// Convenience: compose the player's facing-derived orientation with the
/// manual rotation offset to get the orientation a place action would use.
pub(crate) fn placement_orientation(player_yaw: f32, manual: Cardinal) -> Cardinal {
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

/// Held-button action timer for Normal mode. Drives two verbs:
///
/// - **L-click hold** = *self-work* a player-tagged plan. If the cursor
///   is on a tagged cell (Remove tag on a solid block, or Build tag in
///   empty space behind the player's reach), advance the timer; on
///   completion apply the world mutation the plan describes (break the
///   block for Remove, place the recorded block for Build). Same
///   primitive an NPC's [`crate::npc::Goal::Working`] runs.
/// - **R-click hold** = *direct destroy* any solid block under the
///   cursor, bypassing the plan step. Reads as harvesting (chop a tree,
///   mine a stone). Drops happen via the same `auto_clear_stale_plans`
///   / drops bus the server uses for any other destroy.
///
/// If both buttons are held simultaneously, L wins (self-work is the
/// more deliberate action and lines up with the player's tagged queue).
/// Switching aim or releasing the held button drops in-flight progress.
///
/// The F3 [`crate::debug::InstantPlayerBuilds`] toggle short-circuits
/// the timer: on `just_pressed`, send the edit immediately and bypass
/// the state machine.
///
/// Plan mode doesn't run this system (mode gate); it has its own
/// `plan_mode_input` for tagging.
#[allow(clippy::too_many_arguments, reason = "input system spans many subsystems")]
/// Senders + local-player reads for `normal_mode_action_input`,
/// bundled to keep the system under Bevy 0.18's 16-SystemParam cap.
/// Adding tool gating to the function pushed it over; consolidating
/// the message senders + avatar queries into one slot apiece restores
/// headroom. Same pattern as `npc::HaulCtx` in the brain tick.
#[derive(bevy::ecs::system::SystemParam)]
struct NormalActionIo<'w, 's> {
    edit: Query<'w, 's, &'static mut MessageSender<BlockEdit>>,
    pickup: Query<'w, 's, &'static mut MessageSender<PickupRequest>>,
    deposit: Query<'w, 's, &'static mut MessageSender<DepositRequest>>,
}

#[derive(bevy::ecs::system::SystemParam)]
struct LocalPlayerState<'w, 's> {
    carry: Query<'w, 's, &'static Carrying, (With<Avatar>, With<Predicted>)>,
    tool: Query<'w, 's, &'static EquippedTool, (With<Avatar>, With<Predicted>)>,
}

fn normal_mode_action_input(
    mouse: Res<ButtonInput<MouseButton>>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    mode: Res<PlayerMode>,
    time: Res<Time>,
    instant_builds: Res<crate::debug::InstantPlayerBuilds>,
    cam: Query<&GlobalTransform, With<FlyCam>>,
    chunks: Query<(&Chunk, &ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    plans: Res<Plans>,
    registry: Res<BlockRegistry>,
    items: Res<ItemRegistry>,
    world_items: Query<&WorldItem>,
    local: LocalPlayerState,
    mut action: ResMut<PlayerActionState>,
    mut io: NormalActionIo,
) {
    // Only Normal uses the action timer. Switching modes or losing
    // cursor lock mid-action cancels the in-flight progress so
    // re-entering doesn't pick up a stale timer state.
    if *mode != PlayerMode::Normal {
        action.active = None;
        return;
    }
    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    if !locked {
        action.active = None;
        return;
    }
    let l_held = mouse.pressed(MouseButton::Left);
    let r_held = mouse.pressed(MouseButton::Right);
    if !l_held && !r_held {
        action.active = None;
        return;
    }

    let Ok(cam_t) = cam.single() else {
        return;
    };
    let Ok(mut sender) = io.edit.single_mut() else {
        return;
    };
    let cam_pos = cam_t.translation();
    let cam_dir = *cam_t.forward();

    // Two instant L-click fast-paths, in priority order:
    //   1. Pickup a WorldItem closer than any solid block.
    //   2. Deposit a carry unit into a Build plan that needs it.
    // Both consume the click before the self-work timer can latch.
    if mouse.just_pressed(MouseButton::Left) {
        let item_hit = raycast_world_items(cam_pos, cam_dir, RAYCAST_REACH, &world_items);
        let world_hit_for_compare = entity_aware_raycast(
            cam_pos,
            cam_dir,
            RAYCAST_REACH,
            &chunks,
            &chunk_map,
            &registry,
            None,
        );
        let world_hit_dist = world_hit_for_compare
            .as_ref()
            .map(|h| (h.cell.as_vec3() + Vec3::splat(0.5) - cam_pos).length());
        let carry = local.carry.single().copied().unwrap_or_default();

        // (1) Pickup.
        if let Some((item_translation, item_dist, item_slot)) = item_hit
            && world_hit_dist.map(|wd| item_dist < wd).unwrap_or(true)
        {
            // Tools route to the EquippedTool slot (server-side); the
            // client just sends the pickup unconditionally and lets
            // the server's swap-semantics handle slot occupancy.
            // Resources still gate on Carrying compatibility.
            let is_tool = !items.def(item_slot).tool_tags.is_empty();
            if is_tool || carry.can_accept(item_slot, PLAYER_CARRY_CAPACITY) {
                if let Ok(mut sender) = io.pickup.single_mut() {
                    sender.send::<WorldChannel>(PickupRequest {
                        target: item_translation,
                    });
                }
            }
            action.active = None;
            return;
        }

        // (2) Deposit. Resolve the targeted plan cell via the same
        // world-or-plan-raycast comparison `resolve_self_work` uses,
        // but only fire when (a) carry has matching item and (b) the
        // plan still needs more of it. Lets a player drop wood into
        // a wood-build plan without first walking onto it.
        if let Some(carry_item) = carry.item
            && carry.count > 0
        {
            let plan_hit = crate::plans::raycast_plans(cam_pos, cam_dir, RAYCAST_REACH, &plans);
            let world_tagged_dist = world_hit_for_compare
                .as_ref()
                .filter(|h| plans.get(h.cell).is_some())
                .map(|h| (h.cell.as_vec3() + Vec3::splat(0.5) - cam_pos, h.cell))
                .map(|(d, c)| (d.length(), c));
            let plan_target = match (world_tagged_dist, plan_hit) {
                (Some(a), Some(b)) if a.0 <= b.0 => Some(a.1),
                (Some(_), Some((_, c))) => Some(c),
                (Some(a), None) => Some(a.1),
                (None, Some((_, c))) => Some(c),
                (None, None) => None,
            };
            if let Some(cell) = plan_target
                && let Some(state) = plans.get(cell)
                && state.remaining_for(carry_item) > 0
            {
                if let Ok(mut sender) = io.deposit.single_mut() {
                    sender.send::<WorldChannel>(DepositRequest { cell });
                }
                action.active = None;
                return;
            }
        }
    }

    // World raycast hits whatever solid block (or block-entity) is
    // first along the ray. `skip_plan_remove = None`: in Normal mode
    // we *want* the cursor to land on a tagged-Remove cell so L-click
    // can self-work it; seeing through tagged cells is a Plan-mode
    // affordance for stacking Remove tags through walls.
    let world_hit = entity_aware_raycast(
        cam_pos,
        cam_dir,
        RAYCAST_REACH,
        &chunks,
        &chunk_map,
        &registry,
        None,
    );

    // Resolve the verb for this frame. L wins over R when both held.
    // For L we also need the plan-only raycast — Build tags float in
    // empty space and the world raycast can't see them.
    let resolved = if l_held {
        let plan_hit = crate::plans::raycast_plans(cam_pos, cam_dir, RAYCAST_REACH, &plans);
        resolve_self_work(cam_pos, &plans, world_hit.as_ref(), plan_hit)
    } else {
        resolve_direct_destroy(world_hit.as_ref())
    };
    let Some((target_cell, kind, edit, button)) = resolved else {
        action.active = None;
        return;
    };

    // Tool gate: if the targeted block's work_action requires a tool
    // tag the player isn't holding, the click is inert. Same logic
    // the outline uses to render grey, kept in sync via the shared
    // `live_block_slot` / `player_can_work_slot` helpers in
    // target_outline.rs. Symmetric with the outline so what the
    // player sees and what the click does always agree.
    let work_slot = match button {
        MouseButton::Right => {
            // direct-destroy: live block at cell
            crate::target_outline::live_block_slot(target_cell, &chunks, &chunk_map)
        }
        _ => {
            // L-click self-work: Build → block being placed;
            // Remove → live block being destroyed.
            match edit.slot.is_empty() {
                true => crate::target_outline::live_block_slot(target_cell, &chunks, &chunk_map),
                false => Some(edit.slot),
            }
        }
    };
    let tool = local.tool.single().copied().unwrap_or_default();
    if !crate::target_outline::player_can_work_slot(work_slot, &registry, &items, tool) {
        // Click inert. Don't tick the action timer — the player
        // will see the outline stay grey and try a different tool.
        action.active = None;
        return;
    }

    // Instant path: F3 toggle skips the timer. Single send on the
    // first frame of the chosen button; no state machine, no
    // progress bar.
    if instant_builds.0 {
        if mouse.just_pressed(button) {
            sender.send::<WorldChannel>(edit);
            action.active = None;
        }
        return;
    }

    // Timed path: accumulate progress against the same target across
    // frames, restart from zero if the target or verb changed (player
    // swept the cursor or swapped buttons).
    let step = time.delta_secs() / PLAYER_ACTION_DURATION_SECS;
    let progress = match action.active {
        Some(a) if a.target_cell == target_cell && a.kind == kind => a.progress + step,
        _ => step,
    };

    if progress >= 1.0 {
        sender.send::<WorldChannel>(edit);
        // Drop state. If the button is still held the next frame's
        // raycast will see the updated world (or, for ~1 tick before
        // the broadcast lands, the stale cell — server rejects the
        // duplicate). Held-sweep harvesting falls out naturally once
        // the world updates and the target cell changes.
        action.active = None;
    } else {
        action.active = Some(ActiveAction {
            target_cell,
            kind,
            progress,
        });
    }
}

/// Q key → send a `DropRequest`. Works in any mode (carry exists
/// across mode switches; dropping should too). Server handles the
/// no-op when the player is empty-handed.
fn drop_carry_input(
    keys: Res<ButtonInput<KeyCode>>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    mut sender: Query<&mut MessageSender<DropRequest>>,
) {
    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    if !locked {
        return;
    }
    if !keys.just_pressed(KeyCode::KeyQ) {
        return;
    }
    if let Ok(mut s) = sender.single_mut() {
        s.send::<WorldChannel>(DropRequest);
    }
}

/// Pick the cell the player's L-click would self-work this frame.
/// Prefers whichever tagged cell is *closer* to the camera between:
///   - the world-raycast hit cell if it itself carries a plan tag
///     (typically a Remove tag on a solid wall); or
///   - the nearest tagged-cell hit from a Plans-only raycast (typically
///     a Build tag floating in empty space, which the world raycast
///     misses because the cell is empty).
///
/// On a tie, the world-raycast hit wins — a tagged-solid right under
/// the cursor is the intuitive choice over a Build tag hovering at the
/// same distance.
///
/// Returns `(target_cell, ActionKind, BlockEdit, MouseButton::Left)` or
/// `None` if no tagged cell lies under the cursor.
fn resolve_self_work(
    cam_pos: Vec3,
    plans: &Plans,
    world_hit: Option<&EntityAwareHit>,
    plan_hit: Option<(f32, IVec3)>,
) -> Option<(IVec3, ActionKind, BlockEdit, MouseButton)> {
    // Helper: build a candidate tuple if the cell has a tag *and* its
    // materials are satisfied. Unsatisfied Build plans are inert to
    // L-click self-work (the player needs to deposit first).
    let materialize = |cell: IVec3, dist: f32| -> Option<(f32, IVec3, PlanKind)> {
        let state = plans.get(cell)?;
        if !state.is_satisfied() {
            return None;
        }
        Some((dist, cell, state.kind))
    };
    let world_candidate = world_hit.and_then(|h| {
        let dist = (h.cell.as_vec3() + Vec3::splat(0.5) - cam_pos).length();
        materialize(h.cell, dist)
    });
    let plan_candidate = plan_hit.and_then(|(dist, cell)| materialize(cell, dist));
    let (_, cell, kind) = match (world_candidate, plan_candidate) {
        (Some(a), Some(b)) if a.0 <= b.0 => a,
        (Some(_), Some(b)) => b,
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    let edit = plan_to_edit(cell, kind);
    let action_kind = match kind {
        PlanKind::Remove => ActionKind::Break,
        PlanKind::Build { .. } => ActionKind::Place,
    };
    Some((cell, action_kind, edit, MouseButton::Left))
}

/// Pick the cell the player's R-click would direct-destroy this frame.
/// Reads the world raycast's hit cell; if there's no hit, no action.
fn resolve_direct_destroy(
    world_hit: Option<&EntityAwareHit>,
) -> Option<(IVec3, ActionKind, BlockEdit, MouseButton)> {
    let hit = world_hit?;
    let edit = BlockEdit {
        anchor: hit.cell,
        slot: BlockSlot::EMPTY,
        orientation: Cardinal::default(),
    };
    Some((hit.cell, ActionKind::Break, edit, MouseButton::Right))
}

/// Translate a `PlanKind` into the `BlockEdit` that would satisfy it.
fn plan_to_edit(cell: IVec3, kind: PlanKind) -> BlockEdit {
    match kind {
        PlanKind::Remove => BlockEdit {
            anchor: cell,
            slot: BlockSlot::EMPTY,
            orientation: Cardinal::default(),
        },
        PlanKind::Build { slot, orientation } => BlockEdit {
            anchor: cell,
            slot,
            orientation,
        },
    }
}

/// Raycast hit for the place/break path. `cell` is the world cell that
/// would receive the action: for break, the cell whose block should be
/// affected; for place, the cell adjacent to the hit face.
pub(crate) struct EntityAwareHit {
    pub(crate) cell: IVec3,
    pub(crate) face_normal: IVec3,
}

/// Closest NPC AABB hit, paired with the ray distance to that hit so
/// callers can compare against a block raycast distance and pick the
/// closer one. The AABB matches the physics shape: centre =
/// `pose.translation - Y * EYE_OFFSET_FROM_CENTRE`, half-extents =
/// `PLAYER_HALF_EXTENTS`.
pub(crate) struct NpcRaycastHit {
    pub(crate) npc_id: NpcId,
    pub(crate) distance: f32,
}

/// Slab-test the camera ray against every NPC's body AABB; return the
/// closest hit within `max_distance`. Linear in NPC count — fine while
/// NPCs are counted in the low tens; will need spatial pruning if we
/// ever reach hundreds.
pub(crate) fn raycast_npcs(
    origin: Vec3,
    dir: Vec3,
    max_distance: f32,
    npcs: &Query<(&NpcId, &AvatarPose), With<Npc>>,
) -> Option<NpcRaycastHit> {
    let inv_dir = Vec3::ONE / dir;
    let mut best: Option<NpcRaycastHit> = None;
    for (id, pose) in npcs.iter() {
        let centre = pose.translation - Vec3::Y * EYE_OFFSET_FROM_CENTRE;
        let min = centre - PLAYER_HALF_EXTENTS;
        let max = centre + PLAYER_HALF_EXTENTS;
        let t1 = (min - origin) * inv_dir;
        let t2 = (max - origin) * inv_dir;
        let tmin_v = t1.min(t2);
        let tmax_v = t1.max(t2);
        let tmin = tmin_v.x.max(tmin_v.y).max(tmin_v.z);
        let tmax = tmax_v.x.min(tmax_v.y).min(tmax_v.z);
        if tmax < tmin || tmax < 0.0 {
            continue;
        }
        let t = if tmin >= 0.0 { tmin } else { tmax };
        if t > max_distance {
            continue;
        }
        if best.as_ref().map(|b| t < b.distance).unwrap_or(true) {
            best = Some(NpcRaycastHit {
                npc_id: *id,
                distance: t,
            });
        }
    }
    best
}

/// Slab-test the camera ray against every `WorldItem`'s approximate
/// AABB. Items have no declared bounding box yet, so we use a generous
/// fixed 0.6 m cube centred on the item's translation — wide enough
/// that a casual click hits, narrow enough that a stack of 5 doesn't
/// overlap into the next item over.
///
/// Returns `(translation, distance, item_slot)` for the closest hit
/// within `max_distance`, or `None` if nothing's in range. Linear in
/// item count; fine while the world holds dozens of loose items. Will
/// need spatial pruning if we ever push into the hundreds.
pub(crate) fn raycast_world_items(
    origin: Vec3,
    dir: Vec3,
    max_distance: f32,
    items: &Query<&WorldItem>,
) -> Option<(Vec3, f32, ItemSlot)> {
    const HALF: f32 = 0.3;
    let inv_dir = Vec3::ONE / dir;
    let mut best: Option<(Vec3, f32, ItemSlot)> = None;
    for wi in items.iter() {
        let min = wi.translation - Vec3::splat(HALF);
        let max = wi.translation + Vec3::splat(HALF);
        let t1 = (min - origin) * inv_dir;
        let t2 = (max - origin) * inv_dir;
        let tmin_v = t1.min(t2);
        let tmax_v = t1.max(t2);
        let tmin = tmin_v.x.max(tmin_v.y).max(tmin_v.z);
        let tmax = tmax_v.x.min(tmax_v.y).min(tmax_v.z);
        if tmax < tmin || tmax < 0.0 {
            continue;
        }
        let t = if tmin >= 0.0 { tmin } else { tmax };
        if t > max_distance {
            continue;
        }
        if best.as_ref().map(|(_, bd, _)| t < *bd).unwrap_or(true) {
            best = Some((wi.translation, t, wi.item));
        }
    }
    best
}

/// Walks world cells like the plain voxel raycast, but treats block-entity
/// cells specially: when the ray enters an entity cell, AABB-test against
/// the entity's declared (rotated) bounds. On miss, keep stepping past so
/// the ray "sees through" the airspace inside a partial-cell mesh and can
/// land on whatever is behind it. On hit, return the entity cell.
///
/// For non-entity cells the behaviour is identical to `world_raycast`.
///
/// `skip_plan_remove`: when `Some`, cells whose [`Plans`] entry is
/// [`PlanKind::Remove`] are treated as empty for the purpose of the cast.
/// Lets Plan-mode tag a block queued behind one already tagged for removal.
/// Callers that need to land on the tagged cell itself (Plan-mode Cancel,
/// Build/Destroy targeting) pass `None`.
pub(crate) fn entity_aware_raycast(
    origin: Vec3,
    dir: Vec3,
    max_distance: f32,
    chunks: &Query<(&Chunk, &ChunkEntities)>,
    chunk_map: &ChunkMap,
    registry: &BlockRegistry,
    skip_plan_remove: Option<&Plans>,
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
    let is_passable = |cell: IVec3, slot: BlockSlot| -> bool {
        slot.is_empty()
            || skip_plan_remove
                .is_some_and(|p| matches!(p.kind(cell), Some(PlanKind::Remove)))
    };

    // Two-pass: first find the nearest cell whose block-entity AABB
    // genuinely contains the ray, OR a non-entity solid cell (cube AABB).
    // Reuse the cube-stepping core; for each non-empty cell, decide
    // whether to accept based on entity kind + AABB test.
    let mut cell = origin.floor().as_ivec3();
    let mut entered_face = IVec3::ZERO;

    let (slot, kind) = lookup(cell);
    if !is_passable(cell, slot)
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
        if is_passable(cell, slot) {
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
    // `OnEnter(InGame)` re-fires on every un-pause. Without this guard
    // we'd spawn a fresh `PreviewCubeRoot` and overwrite `state.cube_root`,
    // orphaning the old cube as a permanent ghost in the world.
    if state.cube_root.is_some() {
        return;
    }

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

/// Run-condition: only show the placement preview ghost in Plan mode
/// with a real block in the hotbar. The Destroy slot has no place
/// preview — the cursor outline already indicates the target cell.
/// Plan-mode preview reads as "this is what the tag would build" while
/// the drag is paused on a single cell.
fn show_placement_preview(
    mode: Res<PlayerMode>,
    selected: Res<SelectedBlock>,
    palette: Res<PlaceablePalette>,
) -> bool {
    *mode == PlayerMode::Plan && selected.current_block(&palette).is_some()
}

/// Hide the placement preview ghost when the player just left Plan
/// mode, or when the hotbar selection flipped to the Destroy slot.
/// Without this the last-frame ghost would linger on screen — the main
/// preview system stops running thanks to the `run_if` gate, so
/// something has to actively flip Visibility on the transition.
fn hide_preview_on_mode_change(
    mode: Res<PlayerMode>,
    selected: Res<SelectedBlock>,
    palette: Res<PlaceablePalette>,
    state: Res<PreviewState>,
    mut vis: Query<&mut Visibility>,
) {
    // Re-run when either the mode or the selection changed; the
    // gate decides whether to hide based on the new state.
    if !mode.is_changed() && !selected.is_changed() {
        return;
    }
    let should_show =
        *mode == PlayerMode::Plan && selected.current_block(&palette).is_some();
    if should_show {
        return;
    }
    for entity in [state.cube_root, state.scene_root].into_iter().flatten() {
        if let Ok(mut v) = vis.get_mut(entity) {
            *v = Visibility::Hidden;
        }
    }
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
    cam: Query<(&GlobalTransform, &FlyCam, &AvatarPose)>,
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
    // `Res<PlayerMode>` would push this past the 16-param `SystemParam`
    // cap in Bevy 0.18. Mode gating lives in a `run_if` on the system
    // registration plus `hide_preview_on_mode_change` for the leave-Build
    // transition. Cursor-lock gating still happens here.
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

    let Ok((cam_t, fly, pose)) = cam.single() else {
        hide(state.cube_root, &mut roots);
        hide(state.scene_root, &mut roots);
        return;
    };
    let cam_pos = cam_t.translation();
    let cam_dir = *cam_t.forward();
    let visible_yaw = pose.yaw + fly.pending_dyaw;

    let Some(hit) = entity_aware_raycast(
        cam_pos,
        cam_dir,
        RAYCAST_REACH,
        &chunks,
        &chunk_map,
        &registry,
        None,
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

    // `show_placement_preview` already proves we have a block-typed
    // palette slot — anything else and this system wouldn't be running.
    let Some(slot) = selected.current_block(&palette) else {
        hide(state.cube_root, &mut roots);
        hide(state.scene_root, &mut roots);
        return;
    };
    let def = registry.def(slot);
    let orientation = placement_orientation(visible_yaw, rotation.0);
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

/// Read keyboard + camera yaw and write the next `MovementIntent` to the
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
    mut flycam: Query<&mut FlyCam>,
    mut q: Query<&mut ActionState<MovementIntent>, With<InputMarker<MovementIntent>>>,
    mut prev_toggle: Local<bool>,
) {
    let Ok(mut state) = q.single_mut() else {
        return;
    };

    // Skip input while the cursor is free (alt-tabbed, settings menu).
    // A default ActionState is what the controller treats as "no keys
    // held, no rotation" — so we still tick through cleanly.
    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    // Drain mouse-motion accumulator into this tick's dyaw. fly_cam_input
    // refills it at render rate; the next FixedUpdate's controller folds
    // the drained value into pose.yaw on both client (predicted) and
    // server (authoritative).
    let dyaw = flycam
        .single_mut()
        .map(|mut f| std::mem::take(&mut f.pending_dyaw))
        .unwrap_or(0.0);

    let mut input = MovementIntent {
        dyaw,
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

/// Client-side soft separation. Same pairwise overlap pass as
/// [`crate::physics::soft_separate_actors`], but only writes pushes
/// back to *Predicted* actors. Interpolated remote actors (other
/// players, NPCs) take part in the pairwise direction calculation
/// — without them the local owner would walk through bodies — but
/// they aren't moved locally, since their pose is owned by lightyear's
/// interpolation against authoritative server snapshots. Locally
/// pushing them would drift each tick away from the snapshot value
/// and stay drifted (a clean fix instead of "next snapshot corrects
/// it," which empirically didn't recover under sustained contact).
fn soft_separate_predicted_actors(
    chunks: Query<(&'static Chunk, &'static ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    mut actors: Query<(Entity, &mut AvatarPose), With<Actor>>,
    predicted_only: Query<(), With<Predicted>>,
) {
    let snapshot: Vec<(Entity, Vec3)> = actors
        .iter()
        .map(|(e, pose)| (e, pose.translation))
        .collect();
    let pushes = compute_actor_separation_pushes(&snapshot);
    let world = WorldCollision {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &registry,
    };
    for (i, (entity, _)) in snapshot.iter().enumerate() {
        let push = pushes[i];
        if push.x == 0.0 && push.y == 0.0 {
            continue;
        }
        // Skip the apply step for interpolated remotes (no
        // `Predicted` marker). They still contributed to the
        // pairwise direction so the local owner gets pushed off
        // them correctly.
        if predicted_only.get(*entity).is_err() {
            continue;
        }
        let Ok((_, mut pose)) = actors.get_mut(*entity) else {
            continue;
        };
        apply_separation_push_swept(&mut pose.translation, push, &world);
    }
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
            &ActionState<MovementIntent>,
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
        // Belt-and-braces: if the predicted avatar starts the tick
        // inside a solid cell (an NPC just built where we were
        // standing, the server's edit-driven pushout fired in
        // `Update` but the replicated pose hasn't reconciled here
        // yet), nudge them clear before the walk-step would otherwise
        // wedge them into the new wall.
        let rescue = rescue_embedded_actor(&mut pose.translation, &world);
        if rescue != Vec3::ZERO {
            // Lateral component of the rescue counts as a velocity
            // reset — we just teleported sideways, we don't want
            // the controller carrying a "moving into a wall" vector.
            vel.0.x = 0.0;
            vel.0.z = 0.0;
        }
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
        ActionState::<MovementIntent>::default(),
        InputMarker::<MovementIntent>::default(),
        // Smooth AvatarPose between FixedUpdate ticks at render-frame
        // resolution. Without this the camera position only changes 64×/s
        // even on a 144Hz display.
        FrameInterpolate::<AvatarPose>::default(),
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
            MeshMaterial3d(assets.avatar_material.clone()),
        ));
    }
}

/// Marker on the SceneRoot-bearing child entity that holds the NPC's
/// glTF body. Lets `setup_npc_skeleton_anim` filter the global
/// `SceneInstanceReady` stream to just our NPC scenes (the world also
/// has block-entity scenes like the bed, and the placement preview).
#[derive(Component)]
struct NpcSceneRoot;

/// Marker on the NPC root entity once we've spawned its visual rig
/// (SceneRoot child + future animation hookup). `attach_npc_visuals`
/// adds this to gate the once-only attach; `setup_npc_anim_once_loaded`
/// later fills `player` with the Entity carrying the auto-inserted
/// `AnimationPlayer` so the per-frame state driver can find it without
/// walking the hierarchy every tick. `current_clip` is the
/// [`AnimationId`](block_junk_mod_api::animations::AnimationId) currently
/// playing — used by `drive_npc_animation` to skip the
/// `AnimationTransitions::play` call when the target clip hasn't
/// changed (which is the common path; the override changes once per
/// goal transition).
///
/// Not a `Resource` because per-NPC: in the future, different NPC
/// kinds will use different scenes (and thus different player
/// entities) in the same world.
#[derive(Component, Default)]
struct NpcVisuals {
    player: Option<Entity>,
    current_clip: Option<String>,
}

/// NPC visual attach: spawn the KayKit character as a child of the NPC
/// root with a baked Y offset so the model's feet (at its glb origin)
/// land at the avatar's foot Y. The root's Transform is the *eye*
/// position (`sync_avatar_transforms`), so a fixed child offset of
/// `-(EYE_OFFSET_FROM_CENTRE + PLAYER_HALF_EXTENTS.y)` is the eye→foot
/// translation.
///
/// Animations are NOT auto-wired: Knight.glb ships zero animation
/// tracks (the clips live in separate Rig_Medium glbs), so Bevy's
/// loader skips the AnimationPlayer + AnimationTargetId pass entirely.
/// `setup_npc_skeleton_anim` replays that pass manually once the scene
/// instance is ready.
fn attach_npc_visuals(
    npcs: Query<Entity, (With<Npc>, Without<NpcVisuals>)>,
    assets: Res<CharacterAssets>,
    mut commands: Commands,
) {
    let foot_offset = -(EYE_OFFSET_FROM_CENTRE + PLAYER_HALF_EXTENTS.y);
    for entity in npcs.iter() {
        info!("npc entered view: {entity:?}");
        // Transform + Visibility on the NPC root: `sync_avatar_transforms`
        // queries for `&mut Transform`, and without it (and the propagation
        // siblings Visibility brings in) the child SceneRoot inherits from
        // a missing parent transform and renders the entire skeleton at
        // world origin. The old cuboid path got these for free via
        // `Mesh3d`'s required components; the SceneRoot now lives on the
        // child entity so the parent has to opt in explicitly.
        //
        // 180° Y rotation on the child: KayKit characters are authored
        // facing +Z in bind pose, but the avatar's logical forward
        // (the unit vector used by the brain's pure-pursuit aim) is -Z.
        // Without this flip the knight walks backwards along his path.
        commands
            .entity(entity)
            .insert((NpcVisuals::default(), Transform::default(), Visibility::default()))
            .with_child((
                NpcSceneRoot,
                SceneRoot(assets.knight_scene.clone()),
                Transform::from_xyz(0.0, foot_offset, 0.0)
                    .with_rotation(Quat::from_rotation_y(core::f32::consts::PI)),
            ));
    }
}

/// Marker on the NPC root entity once its carry icon has been
/// spawned. Gates the per-frame attach loop so each NPC only gets
/// one icon child even though the system runs every frame.
#[derive(Component)]
struct NpcCarryIconAttached;

/// Marker on the child entity that *is* the floating carry icon —
/// owns its own [`MeshMaterial3d`] handle so the per-NPC colour
/// update doesn't bleed across NPCs (sharing a material handle would
/// repaint every icon at once).
#[derive(Component)]
struct NpcCarryIcon;

/// Spawn a hidden floating cube above each NPC's head, one per NPC.
/// The cube is the MVP visual for "this NPC is carrying something" —
/// hand-IK is deferred per the Phase 4 plan. Runs every frame but is
/// idempotent against re-runs via the `NpcCarryIconAttached` marker.
fn attach_npc_carry_icons(
    npcs: Query<Entity, (With<Npc>, Without<NpcCarryIconAttached>)>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for entity in npcs.iter() {
        let mesh = meshes.add(Cuboid::new(0.25, 0.25, 0.25));
        let material = materials.add(StandardMaterial {
            base_color: Color::srgba(1.0, 1.0, 1.0, 1.0),
            unlit: true,
            ..default()
        });
        // Anchor at +1.0 m above the NPC root (which itself is at eye
        // height). Visible from above-the-head viewing angles, which
        // covers most gameplay camera positions.
        commands
            .entity(entity)
            .insert(NpcCarryIconAttached)
            .with_child((
                NpcCarryIcon,
                Mesh3d(mesh),
                MeshMaterial3d(material),
                Transform::from_xyz(0.0, 1.0, 0.0),
                Visibility::Hidden,
            ));
    }
}

/// Mirror each NPC's `Carrying` state onto its floating icon child:
/// hidden when empty, visible + tinted to the item def's color when
/// holding a stack. Walks every NPC every frame — cheap (the per-NPC
/// child lookup is one HashMap probe via the `Children` deref) and
/// avoids the change-detection bookkeeping for sub-frame correctness.
fn update_npc_carry_icons(
    npcs: Query<(&Carrying, &Children), With<Npc>>,
    mut icons: Query<
        (&MeshMaterial3d<StandardMaterial>, &mut Visibility),
        With<NpcCarryIcon>,
    >,
    items: Res<ItemRegistry>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (carry, children) in npcs.iter() {
        for child in children.iter() {
            let Ok((mat_handle, mut vis)) = icons.get_mut(child) else {
                continue;
            };
            match (carry.item, carry.count) {
                (Some(slot), c) if c > 0 => {
                    *vis = Visibility::Inherited;
                    if let Some(material) = materials.get_mut(&mat_handle.0) {
                        let [r, g, b] = items.def(slot).color;
                        material.base_color = Color::srgba(r, g, b, 1.0);
                    }
                }
                _ => {
                    *vis = Visibility::Hidden;
                }
            }
        }
    }
}

/// Attach the visible glTF scene + Transform when a `WorldItem`
/// component is added (whether by local spawn or by lightyear's
/// replication apply on the client). The component carries its own
/// translation — `Transform` is never replicated (the
/// networking-design rule), so we derive it from the component.
///
/// Idempotent against re-adds; if the entity already has a SceneRoot
/// we no-op, since multi-add only fires when an observer is registered
/// after the entity exists, not on duplicate inserts.
fn attach_world_item_visuals(
    trigger: On<Add, WorldItem>,
    items: Res<ItemRegistry>,
    asset_server: Res<AssetServer>,
    world_items: Query<&WorldItem>,
    existing: Query<(), With<SceneRoot>>,
    mut commands: Commands,
) {
    let entity = trigger.event_target();
    if existing.contains(entity) {
        return;
    }
    let Ok(world_item) = world_items.get(entity) else {
        return;
    };
    let def = items.def(world_item.item);
    let scene: Handle<Scene> = asset_server.load(format!("{}#Scene0", def.mesh));
    commands.entity(entity).insert((
        SceneRoot(scene),
        Transform::from_translation(world_item.translation),
        GlobalTransform::default(),
        Visibility::default(),
        Name::new(format!("WorldItem({})", def.id)),
    ));
}

/// Manually replay Bevy's animation-rigging pass when the Knight's
/// scene instance is ready. Bevy's glTF loader normally inserts an
/// `AnimationPlayer` on each "animation root" node and tags every
/// descendant with `AnimationTargetId` + `AnimatedBy(player_entity)`
/// — but only when the loaded glb contains animation tracks. Knight.glb
/// has zero (clips live in `Rig_Medium_*.glb`), so we do it here.
///
/// Retargeting works because `AnimationTargetId` is hashed from the
/// bone-name path, not the file the bone was loaded from. As long as
/// the path from the scene-root node down (here `["Rig_Medium", "root",
/// "hips", …]`) matches between the character and animation glbs, the
/// IDs collide and the rig clip drives the Knight's bones.
///
/// We treat the entity that bears `SceneRoot` as a "virtual scene root"
/// — its direct child is node[32] "Rig_Medium" in the glb, which gets
/// the `AnimationPlayer`. The follow-up system `start_npc_anim_idle`
/// watches `Added<AnimationPlayer>` to attach `AnimationTransitions`
/// and start the idle clip.
fn setup_npc_skeleton_anim(
    trigger: On<SceneInstanceReady>,
    npc_scene_roots: Query<(), With<NpcSceneRoot>>,
    children_q: Query<&Children>,
    names: Query<&Name>,
    assets: Res<CharacterAssets>,
    mut commands: Commands,
) {
    let scene_bearer = trigger.event_target();
    if !npc_scene_roots.contains(scene_bearer) {
        return;
    }
    // Bevy's glTF loader wraps the scene's top-level nodes in an
    // unnamed entity that carries the coordinate-system conversion
    // transform (see bevy_gltf loader.rs::world_root_id), so the named
    // "Rig_Medium" node from the glb is a *grandchild* of the SceneRoot
    // bearer, not a direct child. Search by name to be wrapper-agnostic.
    let Some(rig_root) = find_named_descendant(scene_bearer, "Rig_Medium", &children_q, &names)
    else {
        warn!("npc scene ready but no 'Rig_Medium' descendant: {scene_bearer:?}");
        return;
    };
    let rig_name = names.get(rig_root).map(|n| n.as_str()).unwrap_or("<unnamed>");
    // Walk the rig hierarchy, tagging every named entity with the same
    // (AnimationTargetId, AnimatedBy) pair the glTF loader would have
    // assigned if Knight.glb had its own animations.
    let mut path: Vec<Name> = Vec::new();
    let mut tagged: usize = 0;
    tag_animation_targets(
        rig_root,
        &mut path,
        rig_root,
        &children_q,
        &names,
        &mut commands,
        &mut tagged,
    );

    // Insert AnimationPlayer + graph here; AnimationTransitions and the
    // initial idle clip come next frame in `start_npc_anim_idle`. They
    // can't go in this observer because AnimationTransitions::play
    // needs `&mut AnimationPlayer`, which we can only borrow once the
    // component is actually in the world.
    commands.entity(rig_root).insert((
        AnimationPlayer::default(),
        AnimationGraphHandle(assets.anim_graph.clone()),
    ));
    info!(
        "npc skeleton rigged: bearer {scene_bearer:?} rig_root {rig_root:?} \
         rig_name={rig_name:?} tagged={tagged}"
    );
}

fn find_named_descendant(
    root: Entity,
    target: &str,
    children_q: &Query<&Children>,
    names: &Query<&Name>,
) -> Option<Entity> {
    let mut stack: Vec<Entity> = vec![root];
    while let Some(e) = stack.pop() {
        if let Ok(name) = names.get(e)
            && name.as_str() == target
        {
            return Some(e);
        }
        if let Ok(children) = children_q.get(e) {
            for c in children.iter() {
                stack.push(c);
            }
        }
    }
    None
}

fn tag_animation_targets(
    entity: Entity,
    path: &mut Vec<Name>,
    rig_root: Entity,
    children_q: &Query<&Children>,
    names: &Query<&Name>,
    commands: &mut Commands,
    tagged: &mut usize,
) {
    let Ok(name) = names.get(entity) else {
        // Skip unnamed entities (e.g. anonymous wrapper nodes); they
        // can't be addressed by AnimationTargetId anyway.
        return;
    };
    path.push(name.clone());
    commands.entity(entity).insert((
        AnimationTargetId::from_names(path.iter()),
        AnimatedBy(rig_root),
    ));
    *tagged += 1;
    if let Ok(children) = children_q.get(entity) {
        for child in children.iter() {
            tag_animation_targets(child, path, rig_root, children_q, names, commands, tagged);
        }
    }
    path.pop();
}

/// Frame after `setup_npc_skeleton_anim` inserts an `AnimationPlayer`,
/// attach `AnimationTransitions` and kick off the idle clip. Also
/// back-fill `NpcVisuals.player` on the NPC root so the per-frame
/// state driver finds the player in O(1).
///
/// Filter is `Without<AnimationTransitions>` rather than
/// `Added<AnimationPlayer>` because observer-inserted components don't
/// reliably trip the Added detection in the next Update — the
/// commands-buffer apply point can land before or after the Added flag
/// is cleared depending on schedule order. Querying for "lacks
/// transitions" is idempotent: once we add them, the filter excludes
/// the entity.
///
/// The hierarchy walk is bounded — KayKit scenes are a few levels deep
/// but malformed glbs could in principle loop; 16 hops is conservative.
fn start_npc_anim_idle(
    mut commands: Commands,
    assets: Res<CharacterAssets>,
    kinds: Res<NpcKindRegistry>,
    mut new_players: Query<(Entity, &mut AnimationPlayer), Without<AnimationTransitions>>,
    parents: Query<&ChildOf>,
    mut npc_visuals_q: Query<&mut NpcVisuals>,
    npc_kinds: Query<&NpcKind>,
) {
    for (player_entity, mut player) in new_players.iter_mut() {
        let Some(npc_root) = find_npc_ancestor(player_entity, &parents, &npc_visuals_q) else {
            // AnimationPlayer on something that isn't an NPC scene —
            // e.g. future player-character rigs. Leave it alone.
            continue;
        };
        // The first clip to play is the kind's authored idle. Skip
        // the start if the kind isn't replicated yet (would happen
        // if the AnimationPlayer's scene loaded before the NpcKind
        // component arrived via replication — defensive, drive_npc
        // _animation will pick it up next tick).
        let Ok(kind) = npc_kinds.get(npc_root) else {
            continue;
        };
        let Some(kind_def) = kinds.get(&kind.0) else {
            continue;
        };
        let Some(&idle_node) = assets.clip_nodes.get(&kind_def.animations.idle) else {
            continue;
        };
        let mut transitions = AnimationTransitions::new();
        transitions
            .play(&mut player, idle_node, Duration::ZERO)
            .repeat();
        commands.entity(player_entity).insert(transitions);
        if let Ok(mut visuals) = npc_visuals_q.get_mut(npc_root) {
            visuals.player = Some(player_entity);
            visuals.current_clip = Some(kind_def.animations.idle.clone());
        }
        info!("npc anim ready: root {npc_root:?} player {player_entity:?}");
    }
}

fn find_npc_ancestor(
    start: Entity,
    parents: &Query<&ChildOf>,
    npc_visuals_q: &Query<&mut NpcVisuals>,
) -> Option<Entity> {
    let mut cur = start;
    for _ in 0..16 {
        if npc_visuals_q.get(cur).is_ok() {
            return Some(cur);
        }
        cur = parents.get(cur).ok()?.0;
    }
    None
}

/// Pick the right clip for each NPC based on its replicated
/// Per-frame animation driver. Decides which clip plays via:
///
/// 1. **Server-set override.** [`NpcAnimOverride`] carries an
///    [`AnimationId`](block_junk_mod_api::animations::AnimationId)
///    when the server wants a specific clip — set on transitions
///    into Goal::Interacting (via the slot's animation) and
///    Goal::Working (via the kind's `animations.work`). When
///    present, this wins over any velocity-based default.
/// 2. **Velocity hysteresis against kind defaults.** No override ⇒
///    fall through to the NPC's kind animations: `walk` above the
///    WALK_ENTER threshold, `idle` below WALK_EXIT, hold otherwise.
///    Hysteresis prevents strobing at the threshold from
///    interpolation noise.
///
/// The chosen clip is resolved to an `AnimationNodeIndex` through
/// [`CharacterAssets::clip_nodes`]; if the registered id doesn't map
/// (mod typo, unloaded asset), the NPC keeps whatever it was
/// already playing.
fn drive_npc_animation(
    mut npcs: Query<
        (
            &AvatarVelocity,
            &NpcAnimOverride,
            &NpcKind,
            &mut NpcVisuals,
        ),
        With<Npc>,
    >,
    mut players: Query<(&mut AnimationPlayer, &mut AnimationTransitions)>,
    assets: Res<CharacterAssets>,
    kinds: Res<NpcKindRegistry>,
) {
    const WALK_ENTER: f32 = 0.5;
    const WALK_EXIT: f32 = 0.2;
    const CROSSFADE: Duration = Duration::from_millis(200);
    for (velocity, override_, kind, mut visuals) in npcs.iter_mut() {
        let Some(player_entity) = visuals.player else {
            continue;
        };
        let Some(kind_def) = kinds.get(&kind.0) else {
            continue;
        };
        let speed_xz = Vec2::new(velocity.0.x, velocity.0.z).length();
        // Resolve the clip id. Override always wins; falling through
        // to walk/idle uses the previous clip as the hysteresis hold.
        let target_clip = match &override_.0 {
            Some(id) => id.clone(),
            None => {
                let walk = kind_def.animations.walk.as_str();
                let idle = kind_def.animations.idle.as_str();
                match visuals.current_clip.as_deref() {
                    Some(c) if c == idle && speed_xz > WALK_ENTER => walk.to_string(),
                    Some(c) if c == walk && speed_xz < WALK_EXIT => idle.to_string(),
                    Some(c) if c == walk => walk.to_string(),
                    // Coming out of an override or starting fresh:
                    // pick idle until velocity climbs.
                    _ => idle.to_string(),
                }
            }
        };
        if visuals
            .current_clip
            .as_deref()
            .map(|c| c == target_clip)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(&node) = assets.clip_nodes.get(&target_clip) else {
            // Unregistered or unresolved id — leave the NPC on its
            // current clip rather than panic. Mods get a warning at
            // boot via the validator; a runtime miss here is the
            // fallback.
            continue;
        };
        let Ok((mut player, mut transitions)) = players.get_mut(player_entity) else {
            continue;
        };
        transitions.play(&mut player, node, CROSSFADE).repeat();
        visuals.current_clip = Some(target_clip);
    }
}

/// Debug overlay: draw each NPC's currently-planned A* path as cyan
/// line segments between cell centres, raised slightly above the
/// floor so they aren't z-fought into the surface. Empty paths
/// (Idle NPCs) draw nothing. Cheap — `Gizmos` is immediate-mode and
/// the path only changes on goal transitions.
fn draw_npc_paths(mut gizmos: Gizmos, paths: Query<&NpcPath>) {
    let raise = Vec3::new(0.5, 0.15, 0.5);
    let color = Color::srgb(0.0, 1.0, 1.0);
    for path in paths.iter() {
        for window in path.0.windows(2) {
            let a = window[0].as_vec3() + raise;
            let b = window[1].as_vec3() + raise;
            gizmos.line(a, b, color);
        }
    }
}

fn mesh_chunks(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ChunkMaterial>>,
    registry: Res<BlockRegistry>,
    textures: Res<BlockTextures>,
    chunks: Query<(Entity, &Chunk, Option<&MeshMaterial3d<ChunkMaterial>>), Changed<Chunk>>,
) {
    for (entity, chunk, material) in chunks.iter() {
        let Some(mesh) = chunk.build_mesh(&registry) else {
            continue;
        };
        let mesh_handle = meshes.add(mesh);
        let mut e = commands.entity(entity);
        e.insert(Mesh3d(mesh_handle));
        if material.is_none() {
            // base_color WHITE so the texture-array sample (which the
            // extension writes into pbr_input.material.base_color) is
            // unmodulated. PBR still applies sun + ambient on top.
            e.insert(MeshMaterial3d(materials.add(ChunkMaterial {
                base: StandardMaterial {
                    base_color: Color::WHITE,
                    perceptual_roughness: 0.9,
                    ..default()
                },
                extension: BlockTextureExt {
                    atlas: textures.array_handle.clone(),
                    mask_atlas: textures.mask_atlas.clone(),
                    ramp_atlas: textures.ramp_atlas.clone(),
                    stacks: textures.stacks.clone(),
                },
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
