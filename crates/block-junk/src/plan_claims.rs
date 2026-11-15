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

use crate::reservations::{Lease, ReservationTable};
use block_junk_mod_api::authority::ReservationOwner;

use bevy::prelude::*;

use crate::npc::NpcId;

/// Cell → claimant. Claims do NOT survive save/load — the brain resets
/// to Idle on load so any in-flight work restarts from scratch and the
/// claim is implicitly dropped.
#[derive(Resource, Default, Debug, Clone)]
pub struct PlanClaims {
    by_cell: ReservationTable<IVec3>,
}

impl PlanClaims {
    pub fn matches(&self, cell: IVec3, lease: &Lease) -> bool {
        self.by_cell.holder(&cell) == Some(lease)
    }
    pub fn revoke(&mut self, cell: IVec3) {
        self.by_cell.revoke(&cell);
    }
    pub fn holder(&self, cell: IVec3) -> Option<NpcId> {
        match &self.by_cell.holder(&cell)?.owner {
            ReservationOwner::Npc { id } => Some(NpcId(*id)),
            _ => None,
        }
    }
    pub fn claim_owned(&mut self, cell: IVec3, owner: ReservationOwner) -> Option<Lease> {
        self.by_cell.acquire_all(&[cell], owner)?.pop()
    }
    pub fn release_owned(&mut self, cell: IVec3, lease: &Lease) -> bool {
        self.by_cell.release(&cell, lease)
    }
    pub fn try_claim(&mut self, cell: IVec3, npc: NpcId) -> bool {
        self.claim_owned(cell, ReservationOwner::Npc { id: npc.0 })
            .is_some()
    }
    pub fn release(&mut self, cell: IVec3, npc: NpcId) {
        if let Some(lease) = self
            .by_cell
            .holder(&cell)
            .filter(|lease| lease.owner == ReservationOwner::Npc { id: npc.0 })
            .cloned()
        {
            self.release_owned(cell, &lease);
        }
    }
    pub fn release_all_for(&mut self, npc: NpcId) {
        self.by_cell
            .release_owner(&ReservationOwner::Npc { id: npc.0 });
    }
    pub fn is_taken_by_other(&self, cell: IVec3, npc: NpcId) -> bool {
        self.by_cell
            .holder(&cell)
            .is_some_and(|lease| lease.owner != ReservationOwner::Npc { id: npc.0 })
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
