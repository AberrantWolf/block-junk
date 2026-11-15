//! Authored architecture is independent of collision and visual meshes.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlacementId(pub u64);

/// Settlement participation is explicit content policy, never inferred from
/// recognizing an empty cave or from a furnishing's visual mesh.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementParticipation {
    #[default]
    None,
    Seed,
    Joiner,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UseSlotId {
    pub placement: PlacementId,
    pub key: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StructuralMetadata {
    pub solid_cells: Vec<[i32; 3]>,
    pub supports: Vec<[i32; 3]>,
    pub shelter_cells: Vec<[i32; 3]>,
    pub partitions: Vec<PartitionFace>,
    pub portals: Vec<PortalGeometry>,
    pub stairs: Vec<SurfaceLink>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartitionFace {
    pub cell: [i32; 3],
    pub normal: [i32; 3],
    pub window: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortalGeometry {
    pub key: String,
    pub faces: Vec<PartitionFace>,
    pub threshold: Vec<[i32; 3]>,
    pub volume: Vec<[i32; 3]>,
    pub initially_open: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceLink {
    pub from: [i32; 3],
    pub to: [i32; 3],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FacilitySlot {
    pub key: String,
    /// Omit for standing use from an approach surface; supply for a bed/chair snap.
    #[serde(default)]
    pub pose: Option<[f32; 3]>,
    pub yaw: f32,
    pub approaches: Vec<[i32; 3]>,
    pub exclusive: bool,
    pub interactions: Vec<String>,
    #[serde(default)]
    pub animation: Option<String>,
}

/// The behavior advertised by a named use slot. Recipe and inventory actions use
/// their existing transaction services; timed uses declare their effects as data.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FacilityAction {
    Timed {
        duration_secs: f32,
        #[serde(default)]
        need_restore: Option<crate::blocks::NeedRestore>,
        #[serde(default)]
        consumption: Option<FacilityConsumption>,
    },
    Crafting,
    Storage,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FacilityConsumption {
    Stock {
        count: u32,
        #[serde(default)]
        tag: Option<crate::blocks::TagId>,
    },
    Deplete,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MovementProfile {
    pub id: String,
    pub height: f32,
    pub radius: f32,
    pub step_up: u32,
    pub max_drop: u32,
    pub stairs: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SettlementDefinition {
    pub id: String,
    pub outdoor_link_range: u32,
    pub movement_profile: String,
}

#[derive(Clone, Debug)]
pub struct CompiledArchitecture {
    pub structure: StructuralMetadata,
    pub slots: Vec<FacilitySlot>,
}
impl CompiledArchitecture {
    pub fn compile(def: &crate::blocks::BlockDef) -> Result<[Self; 4], String> {
        def.structure.validate(&def.footprint)?;
        if def.settlement != SettlementParticipation::None && def.facility_slots.is_empty() {
            return Err(
                "settlement participants require named facility slots and approach surfaces".into(),
            );
        }
        for (key, action) in &def.facility_actions {
            if key.is_empty() || key.len() > 128 {
                return Err("facility action requires a bounded nonempty key".into());
            }
            match action {
                FacilityAction::Timed {
                    duration_secs,
                    consumption,
                    ..
                } => {
                    if !duration_secs.is_finite() || *duration_secs <= 0. {
                        return Err(format!(
                            "facility action {key} duration must be finite and positive"
                        ));
                    }
                    match consumption {
                        Some(FacilityConsumption::Stock { count, .. })
                            if *count == 0 || def.container.is_none() =>
                        {
                            return Err(format!(
                                "facility action {key} stock consumption requires a container and positive count"
                            ));
                        }
                        Some(FacilityConsumption::Deplete) if def.depleted_block.is_none() => {
                            return Err(format!(
                                "facility action {key} depletion requires depleted_block"
                            ));
                        }
                        _ => {}
                    }
                }
                FacilityAction::Crafting if def.station_tag.is_none() => {
                    return Err(format!("facility action {key} requires a crafting station"));
                }
                FacilityAction::Storage if def.container.is_none() => {
                    return Err(format!("facility action {key} requires a container"));
                }
                _ => {}
            }
        }
        let mut keys = std::collections::BTreeSet::new();
        for (index, slot) in def.facility_slots.iter().enumerate() {
            if slot.key.is_empty()
                || slot.key.len() > 128
                || !keys.insert(&slot.key)
                || slot.approaches.is_empty()
                || !slot.yaw.is_finite()
                || slot
                    .pose
                    .iter()
                    .flatten()
                    .any(|v| !v.is_finite() || v.abs() > 64.)
                || slot.interactions.is_empty()
            {
                return Err(
                    "facility slots require unique keys, finite poses, approaches and interactions"
                        .into(),
                );
            }
            if slot.pose.is_some()
                && def.facility_slots[..index]
                    .iter()
                    .any(|other| other.pose == slot.pose && (other.exclusive || slot.exclusive))
            {
                return Err(format!(
                    "facility slot {} duplicates an exclusive use pose; put alternative actions on one slot",
                    slot.key
                ));
            }
            let interactions: std::collections::BTreeSet<_> = slot.interactions.iter().collect();
            if interactions.len() != slot.interactions.len()
                || slot
                    .interactions
                    .iter()
                    .any(|key| !def.facility_actions.contains_key(key))
            {
                return Err(format!(
                    "facility slot {} interactions must reference distinct authored actions",
                    slot.key
                ));
            }
            let unique: std::collections::BTreeSet<_> = slot.approaches.iter().collect();
            if unique.len() != slot.approaches.len()
                || slot.approaches.iter().any(|cell| {
                    def.footprint.contains(cell) || cell.iter().any(|v| v.unsigned_abs() > 64)
                })
            {
                return Err(
                    "facility approaches must be unique, nearby and outside its footprint".into(),
                );
            }
        }
        Ok(crate::blocks::Cardinal::ALL.map(|orientation| {
            let mut structure = def.structure.clone();
            let rotate = |cells: &mut Vec<[i32; 3]>| {
                for cell in cells {
                    *cell = orientation.rotate_offset(*cell);
                }
            };
            let face = |face: &mut PartitionFace| {
                face.cell = orientation.rotate_offset(face.cell);
                face.normal = orientation.rotate_offset(face.normal);
            };
            rotate(&mut structure.solid_cells);
            rotate(&mut structure.supports);
            rotate(&mut structure.shelter_cells);
            structure.partitions.iter_mut().for_each(face);
            for portal in &mut structure.portals {
                portal.faces.iter_mut().for_each(face);
                rotate(&mut portal.threshold);
                rotate(&mut portal.volume);
            }
            for stair in &mut structure.stairs {
                stair.from = orientation.rotate_offset(stair.from);
                stair.to = orientation.rotate_offset(stair.to);
            }
            let mut slots = def.facility_slots.clone();
            for slot in &mut slots {
                slot.pose = slot.pose.map(|pose| orientation.rotate_point(pose));
                slot.yaw += orientation.yaw();
                rotate(&mut slot.approaches);
            }
            Self { structure, slots }
        }))
    }
}

impl StructuralMetadata {
    pub fn validate(&self, footprint: &[[i32; 3]]) -> Result<(), String> {
        if self.portals.len() > 64 {
            return Err("a placement supports at most 64 named portals".into());
        }
        if footprint.is_empty() || footprint.iter().flatten().any(|v| v.unsigned_abs() > 64) {
            return Err(
                "structural footprint must be nonempty and within 64 cells of its anchor".into(),
            );
        }
        let footprint: std::collections::BTreeSet<_> = footprint.iter().copied().collect();
        for (label, cells) in [
            ("solid_cells", &self.solid_cells),
            ("supports", &self.supports),
            ("shelter_cells", &self.shelter_cells),
        ] {
            let unique: std::collections::BTreeSet<_> = cells.iter().copied().collect();
            if unique.len() != cells.len() || !unique.is_subset(&footprint) {
                return Err(format!("{label} must contain unique footprint cells"));
            }
        }
        let valid_face = |face: &PartitionFace| {
            footprint.contains(&face.cell)
                && matches!(face.normal, [1, 0, 0] | [-1, 0, 0] | [0, 0, 1] | [0, 0, -1])
        };
        if self.partitions.iter().any(|face| !valid_face(face)) {
            return Err("partitions must be vertical adjacent faces within the footprint".into());
        }
        let mut keys = std::collections::BTreeSet::new();
        let mut occupied_portals = std::collections::BTreeSet::new();
        let mut partition_faces = std::collections::BTreeSet::new();
        for face in &self.partitions {
            if !partition_faces.insert(canonical_face(face)) {
                return Err("duplicate architectural partition face".into());
            }
        }
        for portal in &self.portals {
            if portal.key.is_empty()
                || !keys.insert(&portal.key)
                || portal.faces.is_empty()
                || portal
                    .faces
                    .iter()
                    .any(|face| !valid_face(face) || face.window)
            {
                return Err("portal requires unique key and valid passage faces".into());
            }
            let volume: std::collections::BTreeSet<_> = portal.volume.iter().copied().collect();
            let threshold: std::collections::BTreeSet<_> =
                portal.threshold.iter().copied().collect();
            // A face-only authored door has no passage volume to assign to
            // either space. Thick portals must still declare both exact sets.
            if volume.is_empty() && threshold.is_empty() {
                for face in &portal.faces {
                    if !partition_faces.insert(canonical_face(face)) {
                        return Err("portal face conflicts with another partition".into());
                    }
                }
                continue;
            }
            if volume.is_empty()
                || threshold.is_empty()
                || threshold.len() != portal.threshold.len()
                || !volume.is_subset(&footprint)
                || volume.len() != portal.volume.len()
                || portal.threshold.iter().any(|cell| !volume.contains(cell))
                || portal
                    .volume
                    .iter()
                    .any(|cell| self.solid_cells.contains(cell))
            {
                return Err(
                    "portal volume/threshold must fit its footprint without structural obstruction"
                        .into(),
                );
            }
            let bottoms: std::collections::BTreeSet<_> = volume
                .iter()
                .copied()
                .filter(|cell| !volume.contains(&[cell[0], cell[1] - 1, cell[2]]))
                .collect();
            if bottoms != threshold {
                return Err(format!(
                    "portal {} threshold must describe every passage floor cell",
                    portal.key
                ));
            }
            let mut reached = std::collections::BTreeSet::new();
            let mut pending = vec![*volume.first().expect("nonempty volume")];
            while let Some(cell) = pending.pop() {
                if !reached.insert(cell) {
                    continue;
                }
                for offset in [
                    [1, 0, 0],
                    [-1, 0, 0],
                    [0, 1, 0],
                    [0, -1, 0],
                    [0, 0, 1],
                    [0, 0, -1],
                ] {
                    let next = std::array::from_fn(|axis| cell[axis] + offset[axis]);
                    if volume.contains(&next) && !reached.contains(&next) {
                        pending.push(next);
                    }
                }
            }
            if reached != volume || !occupied_portals.is_disjoint(&volume) {
                return Err(format!(
                    "portal {} has disconnected or overlapping passage geometry",
                    portal.key
                ));
            }
            occupied_portals.extend(volume.iter().copied());
            for face in &portal.faces {
                let adjacent = std::array::from_fn(|axis| face.cell[axis] + face.normal[axis]);
                if !(volume.contains(&face.cell) || volume.contains(&adjacent))
                    || !partition_faces.insert(canonical_face(face))
                {
                    return Err(format!(
                        "portal {} face is detached or conflicts with a partition",
                        portal.key
                    ));
                }
            }
        }
        if self.stairs.iter().any(|link| {
            link.from == link.to
                || link
                    .from
                    .iter()
                    .chain(&link.to)
                    .any(|v| v.unsigned_abs() > 64)
        }) {
            return Err("invalid authored stair link".into());
        }
        Ok(())
    }
}

fn canonical_face(face: &PartitionFace) -> ([i32; 3], [i32; 3]) {
    let next = std::array::from_fn(|axis| face.cell[axis] + face.normal[axis]);
    if face.cell < next {
        (face.cell, next)
    } else {
        (next, face.cell)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn doorway() -> StructuralMetadata {
        StructuralMetadata {
            portals: vec![PortalGeometry {
                key: "door".into(),
                faces: vec![PartitionFace {
                    cell: [0, 0, 0],
                    normal: [1, 0, 0],
                    window: false,
                }],
                threshold: vec![[0, 0, 0]],
                volume: vec![[0, 0, 0], [0, 1, 0]],
                initially_open: true,
            }],
            ..Default::default()
        }
    }
    #[test]
    fn portal_threshold_and_connected_volume_are_required() {
        let footprint = [[0, 0, 0], [0, 1, 0], [0, 3, 0], [1, 0, 0]];
        let mut structure = doorway();
        structure.validate(&footprint).unwrap();
        structure.portals[0].volume.push([0, 3, 0]);
        structure.portals[0].threshold.push([0, 3, 0]);
        assert!(
            structure
                .validate(&footprint)
                .unwrap_err()
                .contains("disconnected")
        );
        structure = doorway();
        structure.portals[0].threshold.push([0, 1, 0]);
        assert!(
            structure
                .validate(&footprint)
                .unwrap_err()
                .contains("floor")
        );
        structure = doorway();
        structure.portals[0].threshold.clear();
        assert!(structure.validate(&footprint).is_err());
        structure = doorway();
        structure.partitions.push(PartitionFace {
            cell: [1, 0, 0],
            normal: [-1, 0, 0],
            window: true,
        });
        assert!(
            structure
                .validate(&footprint)
                .unwrap_err()
                .contains("conflicts")
        );
    }
    #[test]
    fn portal_geometry_does_not_accept_duplicate_floor_cells_or_solid_passage() {
        let footprint = [[0, 0, 0], [0, 1, 0]];
        let mut structure = doorway();
        structure.portals[0].threshold.push([0, 0, 0]);
        assert!(structure.validate(&footprint).is_err());
        structure = doorway();
        structure.solid_cells.push([0, 1, 0]);
        assert!(structure.validate(&footprint).is_err());
        structure = doorway();
        let mut second = structure.portals[0].clone();
        second.key = "other".into();
        structure.portals.push(second);
        assert!(
            structure
                .validate(&footprint)
                .unwrap_err()
                .contains("overlapping")
        );
    }
}

/// A bounded notification. Complete lineage is queried in pages by stable ID;
/// splitting a large hall does not create an unbounded Lua event argument.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpaceChangeKind {
    Created,
    Updated,
    Removed,
    Split,
    Merged,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceLifecycleEvent {
    pub kind: SpaceChangeKind,
    pub spatial_revision: u64,
    pub space_id: u64,
    pub space_revision: u64,
    pub predecessors: u64,
    pub successors: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LineageDirection {
    Predecessors,
    Successors,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GeometryVolume {
    Bounded { cells: u64 },
    Open,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceInfo {
    pub space_id: u64,
    pub revision: u64,
    pub active: bool,
    pub structural_area: u64,
    pub covered_area: Option<u64>,
    pub volume: GeometryVolume,
    pub pending: bool,
    pub predecessors: u64,
    pub successors: u64,
}
