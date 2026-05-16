//! Per-cell reservation table for player-tagged plans (server-only).
//!
//! Mirrors [`crate::sleepers::BedClaims`] for the work axis: when an NPC
//! commits to a [`crate::protocol::PlanKind`] tag, it claims that cell
//! so two NPCs don't path to the same plan and collide on arrival.
//! Claims are released on every transition out of `Goal::Working`
//! (clean completion, abandonment via stuck-detection, BrainDisabled).
//!
//! Keyed per-cell because plans are recorded per-cell in [`Plans`];
//! the snapshot builder filters available plans through
//! `is_taken_by_other` so a planner doesn't see ones already in flight.

use std::collections::HashMap;

use bevy::prelude::*;

use crate::npc::NpcId;

/// Cell → claimant. Claims do NOT survive save/load — the brain resets
/// to Idle on load so any in-flight work restarts from scratch and the
/// claim is implicitly dropped.
#[derive(Resource, Default, Debug)]
pub struct PlanClaims {
    by_cell: HashMap<IVec3, NpcId>,
}

impl PlanClaims {
    /// Try to claim `cell` for `npc`. Succeeds if the slot is empty or
    /// already held by the same NPC (re-claim is a no-op rather than a
    /// contention failure — the brain may re-call this if a goal
    /// restarts mid-flight).
    pub fn try_claim(&mut self, cell: IVec3, npc: NpcId) -> bool {
        match self.by_cell.get(&cell) {
            Some(holder) if holder.0 == npc.0 => true,
            Some(_) => false,
            None => {
                self.by_cell.insert(cell, npc);
                true
            }
        }
    }

    /// Release `cell`'s claim if `npc` holds it. Releasing a claim not
    /// held by `npc` is silently a no-op — protects against double-
    /// release on transitions where multiple paths dispatch the same
    /// release (e.g. abandon + arrive racing).
    pub fn release(&mut self, cell: IVec3, npc: NpcId) {
        if let Some(holder) = self.by_cell.get(&cell)
            && holder.0 == npc.0
        {
            self.by_cell.remove(&cell);
        }
    }

    /// Drop every claim held by `npc`. Called on NPC despawn / brain-
    /// disable so a single NPC can't permanently lock a plan by
    /// failing in some unanticipated way.
    pub fn release_all_for(&mut self, npc: NpcId) {
        self.by_cell.retain(|_, holder| holder.0 != npc.0);
    }

    /// True if `cell` is currently claimed by anyone other than `npc`.
    /// Used by the snapshot builder to filter "available plans" without
    /// taking the claim — taking happens later, atomically, when the
    /// brain commits to a Work goal.
    pub fn is_taken_by_other(&self, cell: IVec3, npc: NpcId) -> bool {
        match self.by_cell.get(&cell) {
            Some(holder) => holder.0 != npc.0,
            None => false,
        }
    }
}

pub struct PlanClaimsPlugin;

impl Plugin for PlanClaimsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PlanClaims>();
    }
}
