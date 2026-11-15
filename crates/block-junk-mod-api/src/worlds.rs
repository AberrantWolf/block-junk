//! Content-owned world bootstrap. Resolved presets are saved with their worlds.
use crate::blocks::BlockId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorldPreset {
    pub id: String,
    pub display_name: String,
    pub terrain: TerrainRoles,
    pub generation: GenerationConfig,
    pub player_spawn: [f32; 3],
    #[serde(default)]
    pub population: Vec<PopulationEntry>,
    pub movement_profile: String,
    pub settlement: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PopulationEntry {
    pub kind: String,
    pub position: [f32; 3],
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TerrainRoles {
    pub stone: BlockId,
    pub dirt: BlockId,
    pub grass: BlockId,
    pub wood: BlockId,
    pub leaves: BlockId,
    pub gravel: BlockId,
    pub forage: BlockId,
}
impl TerrainRoles {
    pub fn entries(&self) -> [(&'static str, &BlockId); 7] {
        [
            ("stone", &self.stone),
            ("dirt", &self.dirt),
            ("grass", &self.grass),
            ("wood", &self.wood),
            ("leaves", &self.leaves),
            ("gravel", &self.gravel),
            ("forage", &self.forage),
        ]
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum GenerationAlgorithm {
    #[default]
    SineHeightmap,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SurfaceFeatures {
    /// One candidate in this many columns. Zero disables the feature.
    pub tree_one_in: u32,
    pub forage_one_in: u32,
    pub gravel_one_in: u32,
    pub tree_height: [u32; 2],
    pub canopy_radius: u32,
    pub gravel_radius: [u32; 2],
}
impl Default for SurfaceFeatures {
    fn default() -> Self {
        Self {
            tree_one_in: 32,
            forage_one_in: 64,
            gravel_one_in: 512,
            tree_height: [4, 5],
            canopy_radius: 2,
            gravel_radius: [1, 2],
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GenerationConfig {
    #[serde(default)]
    pub algorithm: GenerationAlgorithm,
    #[serde(default)]
    pub features: SurfaceFeatures,
    pub version: u32,
    pub seed: u32,
    pub base_height: f32,
    pub amplitude: [f32; 2],
    pub frequency: [f32; 2],
    pub soil_depth: u32,
    pub trees: bool,
    pub forage: bool,
    pub gravel: bool,
}
impl Default for GenerationConfig {
    fn default() -> Self {
        Self {
            algorithm: Default::default(),
            features: Default::default(),
            version: 1,
            seed: 0,
            base_height: 8.,
            amplitude: [4., 4.],
            frequency: [0.07, 0.05],
            soil_depth: 4,
            trees: true,
            forage: true,
            gravel: true,
        }
    }
}
impl WorldPreset {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() || self.generation.version != 1 {
            return Err(format!(
                "preset {}: unsupported generation version {}",
                self.id, self.generation.version
            ));
        }
        let g = self.generation;
        if !g.base_height.is_finite()
            || g.base_height.abs() > 1_000_000.
            || g.amplitude
                .iter()
                .any(|v| !v.is_finite() || !(0. ..=1000.).contains(v))
            || g.frequency
                .iter()
                .any(|v| !v.is_finite() || !(0. ..=1.).contains(v))
            || g.soil_depth > 1024
            || g.features.tree_height[0] == 0
            || g.features.tree_height[0] > g.features.tree_height[1]
            || g.features.tree_height[1] > 64
            || g.features.canopy_radius > 8
            || g.features.gravel_radius[0] > g.features.gravel_radius[1]
            || g.features.gravel_radius[1] > 8
        {
            return Err(format!("preset {}: invalid generation parameters", self.id));
        }
        if self.population.len() > 1000
            || self
                .population
                .iter()
                .any(|entry| entry.kind.is_empty() || !valid_position(entry.position))
            || !valid_position(self.player_spawn)
        {
            return Err(format!(
                "preset {}: invalid bootstrap population or position",
                self.id
            ));
        }
        Ok(())
    }
}
fn valid_position(position: [f32; 3]) -> bool {
    position
        .iter()
        .all(|v| v.is_finite() && v.abs() <= 1_000_000.)
}
