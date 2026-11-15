//! Settlement visualization. Membership comes exclusively from exact facility
//! relationships; these display bounds never assign a room or furnishing.
use bevy::prelude::*;
use block_junk_mod_api::{architecture::PlacementId, civilization::CivilizationParams};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClusterBboxReplica {
    pub min: IVec3,
    pub max: IVec3,
}
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct CivilizationParamsRes(pub CivilizationParams);
pub struct CivilizationPlugin;
impl Plugin for CivilizationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<crate::settlements::Settlements>()
            .init_resource::<CivilizationParamsRes>()
            .add_systems(
                PostUpdate,
                sync_cluster_entities.after(crate::settlements::refresh),
            );
    }
}
fn sync_cluster_entities(
    settlements: Res<crate::settlements::Settlements>,
    params: Res<CivilizationParamsRes>,
    mut entities: Local<BTreeMap<PlacementId, Entity>>,
    mut replicas: Query<(&mut ClusterBboxReplica, &mut crate::spatial::SpatialScope)>,
    mut commands: Commands,
) {
    let stale: Vec<_> = entities
        .keys()
        .filter(|id| !settlements.groups.contains_key(id))
        .copied()
        .collect();
    for id in stale {
        commands.entity(entities.remove(&id).unwrap()).despawn();
    }
    let pad = IVec3::splat(params.0.buffer_cells.max(0));
    for (id, group) in &settlements.groups {
        let next = ClusterBboxReplica {
            min: group.min - pad,
            max: group.max + pad,
        };
        let scope = crate::spatial::SpatialScope::Bounds {
            min: next.min.as_vec3(),
            max: (next.max + IVec3::ONE).as_vec3(),
        };
        if let Some(entity) = entities.get(id) {
            if let Ok((mut bbox, mut visibility)) = replicas.get_mut(*entity) {
                bbox.set_if_neq(next);
                visibility.set_if_neq(scope);
            }
        } else {
            let entity = commands
                .spawn((
                    next,
                    crate::spatial::ordinary_spatial_replica(scope),
                    Name::new(format!("Settlement({})", id.0)),
                ))
                .id();
            entities.insert(*id, entity);
        }
    }
}
