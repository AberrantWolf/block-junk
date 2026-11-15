//! Durable material delivery. A blocked or unloaded destination retains stock;
//! item counts never determine the number of ECS entities or work per tick.
use bevy::prelude::*;
use block_junk_mod_api::items::ItemId;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const DELIVERIES_PER_TICK: usize = 64;

/// Exchange exactly one ground tool. Callers commit the returned remainder and
/// displaced tool alongside this equipment change; duplicate same-tool requests
/// are idempotent and an empty stack cannot mint equipment.
pub fn exchange_tool(
    equipped: &mut crate::protocol::EquippedTool,
    item: crate::items::ItemSlot,
    count: u32,
) -> Option<(u32, Option<crate::items::ItemSlot>)> {
    if count == 0 || equipped.item == Some(item) {
        return None;
    }
    let displaced = equipped.item.replace(item);
    Some((count - 1, displaced))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PendingItem {
    pub origin: [i32; 3],
    pub item: ItemId,
    pub count: u32,
}

#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct ItemPlacementQueue(pub VecDeque<PendingItem>);

impl ItemPlacementQueue {
    pub fn enqueue(&mut self, origin: IVec3, item: ItemId, count: u32) {
        if count > 0 {
            self.0.push_back(PendingItem {
                origin: origin.to_array(),
                item,
                count,
            });
        }
    }
    pub fn validate(&self, items: &crate::items::ItemRegistry) -> Result<(), String> {
        for pending in &self.0 {
            if pending.count == 0
                || items.slot_of(&pending.item).is_none()
                || pending.origin.iter().any(|v| v.unsigned_abs() > 1_000_000)
            {
                return Err(format!("invalid pending item delivery: {:?}", pending));
            }
        }
        Ok(())
    }
}

pub fn destination(walk: &crate::npc::WorldWalk<'_, '_, '_>, origin: IVec3) -> Option<Vec3> {
    [
        IVec3::ZERO,
        IVec3::Y,
        IVec3::X,
        IVec3::NEG_X,
        IVec3::Z,
        IVec3::NEG_Z,
    ]
    .into_iter()
    .find_map(|offset| {
        let from = origin + offset;
        let translation =
            crate::server::settled_translation(walk, from.as_vec3() + Vec3::new(0.5, 0.05, 0.5));
        let (coord, local) = crate::voxel::world_to_chunk(translation.floor().as_ivec3());
        walk.chunk_map
            .0
            .get(&coord)
            .and_then(|&entity| walk.chunks.get(entity).ok())
            .filter(|chunk| chunk.get(local).is_empty())
            .map(|_| translation)
    })
}

pub fn spawn_stack(
    commands: &mut Commands,
    item: crate::items::ItemSlot,
    count: u32,
    translation: Vec3,
) {
    if count == 0 {
        return;
    }
    commands.spawn((
        crate::protocol::WorldItem {
            item,
            count,
            translation,
        },
        Transform::from_translation(translation),
        GlobalTransform::default(),
        crate::spatial::ordinary_spatial_replica(crate::spatial::SpatialScope::Point(translation)),
        Name::new(format!("WorldItem(stack:{}x{})", item.0, count)),
    ));
}

pub fn deliver(
    mut queue: ResMut<ItemPlacementQueue>,
    sidecars: Query<&'static crate::voxel::ChunkEntities>,
    chunks: Query<&'static crate::voxel::Chunk>,
    map: Res<crate::voxel::ChunkMap>,
    blocks: Res<crate::blocks::BlockRegistry>,
    items: Res<crate::items::ItemRegistry>,
    mut commands: Commands,
) {
    let walk = crate::npc::WorldWalk {
        chunks: &chunks,
        sidecars: &sidecars,
        chunk_map: &map,
        registry: &blocks,
    };
    // Rotate unsuccessful deliveries to the back: a blocked first item cannot
    // starve later deliveries. Newly enqueued work waits until a later turn.
    for _ in 0..queue.0.len().min(DELIVERIES_PER_TICK) {
        let pending = queue.0.pop_front().expect("bounded queue snapshot");
        let target = items
            .slot_of(&pending.item)
            .zip(destination(&walk, IVec3::from_array(pending.origin)));
        if let Some((item, translation)) = target {
            spawn_stack(&mut commands, item, pending.count, translation);
        } else {
            queue.0.push_back(pending);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_exchange_conserves_stacks_and_duplicate_requests() {
        let axe = crate::items::ItemSlot(0);
        let hammer = crate::items::ItemSlot(1);
        let mut equipped = crate::protocol::EquippedTool { item: Some(axe) };
        assert_eq!(exchange_tool(&mut equipped, hammer, 0), None);
        assert_eq!(equipped.item, Some(axe));
        assert_eq!(
            exchange_tool(&mut equipped, hammer, 7),
            Some((6, Some(axe)))
        );
        assert_eq!(equipped.item, Some(hammer));
        assert_eq!(exchange_tool(&mut equipped, hammer, 6), None);
    }

    #[test]
    fn blocked_deliveries_round_trip_and_do_not_starve_large_stacks() {
        let mut app = App::new();
        let items = crate::items::ItemRegistry::build(
            serde_json::from_value(serde_json::json!([
                {"id":"test:stone","display_name":"Stone","mesh":"unused.gltf"}
            ]))
            .unwrap(),
        )
        .unwrap();
        let stone = items.slot_of(&"test:stone".into()).unwrap();
        let blocks = crate::blocks::BlockRegistry::build(Vec::new()).unwrap().0;
        app.add_plugins(MinimalPlugins)
            .insert_resource(items)
            .insert_resource(blocks)
            .init_resource::<ItemPlacementQueue>()
            .init_resource::<crate::voxel::ChunkMap>()
            .add_systems(Update, deliver);
        for _ in 0..DELIVERIES_PER_TICK {
            app.world_mut()
                .resource_mut::<ItemPlacementQueue>()
                .enqueue(IVec3::splat(1000), "test:stone".into(), 2);
        }
        app.world_mut()
            .resource_mut::<ItemPlacementQueue>()
            .enqueue(IVec3::splat(2), "test:stone".into(), u32::MAX);
        // All geometry is unavailable: no obligations are consumed.
        app.update();
        assert_eq!(app.world().resource::<ItemPlacementQueue>().0.len(), 65);
        let encoded = serde_json::to_vec(app.world().resource::<ItemPlacementQueue>()).unwrap();
        let restored: ItemPlacementQueue = serde_json::from_slice(&encoded).unwrap();
        restored
            .validate(app.world().resource::<crate::items::ItemRegistry>())
            .unwrap();
        app.insert_resource(restored);
        let chunk = app
            .world_mut()
            .spawn(crate::voxel::Chunk {
                revision: 0,
                blocks: vec![
                    crate::blocks::BlockSlot::EMPTY;
                    (crate::protocol::CHUNK_PADDED as usize).pow(3)
                ],
            })
            .id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO), chunk);
        app.update();
        app.update();
        let world = app.world_mut();
        let stacks: Vec<_> = world
            .query::<&crate::protocol::WorldItem>()
            .iter(world)
            .collect();
        assert_eq!(stacks.len(), 1);
        assert_eq!((stacks[0].item, stacks[0].count), (stone, u32::MAX));
        assert_eq!(world.resource::<ItemPlacementQueue>().0.len(), 64);
        let mut invalid = world.resource::<ItemPlacementQueue>().clone();
        invalid.0.front_mut().unwrap().count = 0;
        assert!(
            invalid
                .validate(world.resource::<crate::items::ItemRegistry>())
                .is_err()
        );
    }
}
