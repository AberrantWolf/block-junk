//! Client-side chunk rendering and block-entity scene refresh.

use bevy::platform::collections::{HashMap, HashSet};
use bevy::prelude::*;
use block_junk_mod_api::blocks::Cardinal;

use crate::block_textures::{BlockTextureExt, BlockTextures, ChunkMaterial};
use crate::blocks::BlockRegistry;
use crate::protocol::ChunkCoord;
use crate::voxel::{Chunk, ChunkEntities, ChunkMap, EntryKind};

/// Tracks the ECS entity rendering each placed block-entity (a block
/// whose `BlockDef.mesh` is set, e.g. furniture, doors). Indexed by world
/// cell with a parallel per-chunk set so we can despawn an entire chunk's
/// block entities cheaply on `ChunkUnload`.
#[derive(Resource, Default)]
pub struct BlockEntities {
    by_cell: HashMap<IVec3, Entity>,
    by_chunk: HashMap<ChunkCoord, HashSet<IVec3>>,
}

type ChangedChunkMeshQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Chunk,
        Option<&'static MeshMaterial3d<ChunkMaterial>>,
    ),
    Changed<Chunk>,
>;

type ChangedBlockEntityChunks<'w, 's> = Query<
    'w,
    's,
    (&'static Chunk, &'static ChunkEntities, &'static ChunkCoord),
    Or<(Changed<Chunk>, Changed<ChunkEntities>)>,
>;

pub(crate) fn mesh_chunks(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ChunkMaterial>>,
    registry: Res<BlockRegistry>,
    textures: Res<BlockTextures>,
    chunks: ChangedChunkMeshQuery,
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
                    tiles: textures.tiles.clone(),
                    blocks: textures.blocks.clone(),
                    textures: textures.textures.clone(),
                    layers: textures.layers.clone(),
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
pub(crate) fn refresh_block_entities(
    chunks_changed: ChangedBlockEntityChunks,
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
            let scene: Handle<WorldAsset> = asset_server.load(format!("{mesh_path}#Scene0"));
            let translation = cell.as_vec3() + Vec3::new(0.5, 0.0, 0.5);
            let rotation = Quat::from_rotation_y(orientation.yaw());
            let entity = commands
                .spawn((
                    WorldAssetRoot(scene),
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
