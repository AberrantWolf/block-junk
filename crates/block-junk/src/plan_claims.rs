//! Per-cell reservation table for player-tagged plans (server-only).
//!
//! Mirrors [`crate::interactables::InteractionClaims`] for the work axis: when an NPC
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

#[cfg(test)]
mod tests {
    use super::*;

    const CELL_A: IVec3 = IVec3::new(0, 0, 0);
    const CELL_B: IVec3 = IVec3::new(1, 2, 3);
    const NPC_1: NpcId = NpcId(1);
    const NPC_2: NpcId = NpcId(2);

    #[test]
    fn first_claim_succeeds_second_fails() {
        let mut claims = PlanClaims::default();
        assert!(claims.try_claim(CELL_A, NPC_1));
        assert!(!claims.try_claim(CELL_A, NPC_2));
    }

    #[test]
    fn reclaim_by_same_npc_is_idempotent() {
        let mut claims = PlanClaims::default();
        assert!(claims.try_claim(CELL_A, NPC_1));
        assert!(claims.try_claim(CELL_A, NPC_1));
    }

    #[test]
    fn release_lets_another_npc_claim() {
        let mut claims = PlanClaims::default();
        claims.try_claim(CELL_A, NPC_1);
        claims.release(CELL_A, NPC_1);
        assert!(claims.try_claim(CELL_A, NPC_2));
    }

    #[test]
    fn release_by_non_owner_is_a_no_op() {
        let mut claims = PlanClaims::default();
        claims.try_claim(CELL_A, NPC_1);
        // NPC_2 tries to release a claim it doesn't hold — nothing happens.
        claims.release(CELL_A, NPC_2);
        assert!(!claims.try_claim(CELL_A, NPC_2));
        // Original owner still holds it.
        assert!(claims.try_claim(CELL_A, NPC_1));
    }

    #[test]
    fn release_all_for_drops_only_that_npc_claims() {
        let mut claims = PlanClaims::default();
        claims.try_claim(CELL_A, NPC_1);
        claims.try_claim(CELL_B, NPC_2);
        claims.release_all_for(NPC_1);
        // NPC_1's cell is free, NPC_2's cell is still held.
        assert!(claims.try_claim(CELL_A, NPC_2));
        assert!(!claims.try_claim(CELL_B, NPC_1));
    }

    #[test]
    fn is_taken_by_other_excludes_self() {
        let mut claims = PlanClaims::default();
        claims.try_claim(CELL_A, NPC_1);
        assert!(claims.is_taken_by_other(CELL_A, NPC_2));
        assert!(!claims.is_taken_by_other(CELL_A, NPC_1));
        assert!(!claims.is_taken_by_other(CELL_B, NPC_1));
    }
}
