//! Engine-side room pattern registry. Validates patterns mods register and
//! exposes them as a `Resource` for the (forthcoming) detector to match
//! against. The detector itself isn't in this module yet — only the
//! catalogue is. Detection lands when [`crate::blocks::BlockRegistry`]'s
//! consumers are ready to read the matched [`RoomEvent`]s (TBD).

use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use block_junk_mod_api::rooms::{PatternDomain, RoomPattern, RoomPatternId};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RoomBootstrapError {
    #[error("duplicate room pattern id {0}")]
    Duplicate(RoomPatternId),
    #[error("pattern {child} declares unknown parent {parent}")]
    UnknownParent {
        child: RoomPatternId,
        parent: RoomPatternId,
    },
    #[error(
        "pattern {child} (domain={child_domain:?}) inherits from {parent} (domain={parent_domain:?})"
    )]
    DomainMismatch {
        child: RoomPatternId,
        child_domain: PatternDomain,
        parent: RoomPatternId,
        parent_domain: PatternDomain,
    },
    #[error("cycle in pattern parent chain involving {0}")]
    Cycle(RoomPatternId),
}

#[derive(Resource)]
pub struct RoomPatternRegistry {
    patterns: Vec<RoomPattern>,
    by_id: HashMap<RoomPatternId, usize>,
    /// Depth of each pattern in its inheritance tree. Roots are 0; used by
    /// the matcher to pick the *deepest* matching node.
    depths: Vec<u32>,
}

#[allow(
    dead_code,
    reason = "get/depth_of/iter are the surface the room detector will read once it lands"
)]
impl RoomPatternRegistry {
    pub fn build(pending: Vec<RoomPattern>) -> Result<Self, RoomBootstrapError> {
        let mut by_id = HashMap::with_capacity(pending.len());
        for (i, p) in pending.iter().enumerate() {
            if by_id.insert(p.id.clone(), i).is_some() {
                return Err(RoomBootstrapError::Duplicate(p.id.clone()));
            }
        }

        let mut depths = vec![0u32; pending.len()];
        for i in 0..pending.len() {
            let mut depth = 0u32;
            let mut seen: HashSet<RoomPatternId> = HashSet::new();
            seen.insert(pending[i].id.clone());
            let mut current = &pending[i];
            while let Some(parent_id) = &current.parent {
                let &parent_idx =
                    by_id
                        .get(parent_id)
                        .ok_or_else(|| RoomBootstrapError::UnknownParent {
                            child: current.id.clone(),
                            parent: parent_id.clone(),
                        })?;
                let parent = &pending[parent_idx];
                if parent.domain != current.domain {
                    return Err(RoomBootstrapError::DomainMismatch {
                        child: current.id.clone(),
                        child_domain: current.domain,
                        parent: parent.id.clone(),
                        parent_domain: parent.domain,
                    });
                }
                if !seen.insert(parent.id.clone()) {
                    return Err(RoomBootstrapError::Cycle(pending[i].id.clone()));
                }
                depth += 1;
                current = parent;
            }
            depths[i] = depth;
        }

        Ok(Self {
            patterns: pending,
            by_id,
            depths,
        })
    }

    pub fn get(&self, id: &RoomPatternId) -> Option<&RoomPattern> {
        self.by_id.get(id).map(|&i| &self.patterns[i])
    }

    pub fn depth_of(&self, id: &RoomPatternId) -> Option<u32> {
        self.by_id.get(id).map(|&i| self.depths[i])
    }

    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &RoomPattern> + '_ {
        self.patterns.iter()
    }
}
