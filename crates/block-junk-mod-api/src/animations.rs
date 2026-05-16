//! Animation clip registry — named skeletal animations that NPC kinds
//! and block use-slots can reference by id.
//!
//! Animations are global: mods register them once at boot via
//! `engine.animations.register`, and any [`NpcKindDef`](crate::npcs::NpcKindDef)
//! or [`UseSlot`](crate::blocks::UseSlot) can then point to one by id.
//! This lets the engine stay action-agnostic — "what clip plays when
//! an NPC sits in this chair / lies in this bed / works at this forge"
//! is purely data, not engine code.
//!
//! Each [`AnimationDef`] resolves to a single clip inside a glTF asset.
//! The same asset can host many clips at different indices, which
//! matches how KayKit-style packs ship — one `Rig_Medium_General.glb`
//! with idle, sit, wave, etc.

use serde::{Deserialize, Serialize};

/// Stable string identifier for a registered animation clip,
/// "namespace:name" by convention. The namespace matches the mod that
/// registered the clip (`vanilla`, `mymod`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AnimationId(pub String);

impl AnimationId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for AnimationId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for AnimationId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl core::fmt::Display for AnimationId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Registered animation clip. Pairs an [`AnimationId`] with the glTF
/// asset and per-asset clip index needed to resolve it on the client.
///
/// The server does not load animation assets — it only validates that
/// every referenced id (from `NpcKindDef.animations`,
/// `UseSlot.animation`, …) is registered. The client iterates the
/// registry at boot, loads each `asset` once, and builds a unified
/// `AnimationGraph` keyed by [`AnimationId`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnimationDef {
    pub id: AnimationId,
    /// Asset path resolved by the engine's asset server. Use the
    /// `mods://` source — e.g. `"mods://vanilla/animations/general.glb"`.
    pub asset: String,
    /// Which animation clip inside the glTF asset this id refers to.
    /// glTF clips are indexed 0..N in author order; check the asset's
    /// embedded clip list to find the right one.
    pub clip_index: u32,
}
