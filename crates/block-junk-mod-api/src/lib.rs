//! Public extension surface for block-junk mods.
//!
//! This crate has *no* dependency on Bevy or other engine internals. It is
//! versioned independently — bumping the engine should never force a mod
//! recompile unless the surface here changes. Treat every public item like
//! a public ABI.
//!
//! Mods organize their scripts around two fixed entry points:
//!
//! - **`data.lua`** runs on both sides and is where mods register
//!   declarative content (blocks, room patterns, future recipes / NPC
//!   kinds). Both sides need the same registries so slot ordering and
//!   pattern IDs agree across the wire.
//! - **`events.lua`** runs on the server only. This is where mods
//!   subscribe to runtime hooks (`engine.on_block_placed`,
//!   `engine.on_room_event`, …) that fire against the authoritative
//!   world. A mod that only adds content can omit it entirely.
//!
//! At least one of the two must exist. Items below are organized by
//! the side they're relevant to:
//!
//! - [`shared`] — types and hooks available in either entry point.
//! - [`server`] — types and hooks for `events.lua` on the server.
//! - [`client`] — types and hooks that would be exposed in client-side
//!   contexts. None today; reserved.
//! - [`blocks`] — block registry types (id, def, flags, tags). Side-agnostic.
//! - [`items`] — item registry types (id, def, drops). Side-agnostic.
//! - [`npcs`] — NPC kind + need registry types and the planner goal
//!   surface. Side-agnostic (the same kind registers on both sides);
//!   planner callbacks live in server-only `events.lua`.
//! - [`rooms`] — room pattern registry types. Side-agnostic.

pub mod animations;
pub mod blocks;
pub mod items;
pub mod npcs;
pub mod recipes;
pub mod rooms;
pub mod textures;

use serde::{Deserialize, Serialize};

/// API version this crate exposes. Mods declare a target version in their
/// manifest; the host refuses to load mods whose target is incompatible.
pub const API_VERSION: ApiVersion = ApiVersion {
    major: 0,
    minor: 1,
    patch: 0,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl ApiVersion {
    pub fn parse(s: &str) -> Result<Self, &'static str> {
        let mut parts = s.split('.');
        let major: u32 = parts
            .next()
            .ok_or("missing major")?
            .parse()
            .map_err(|_| "invalid major")?;
        let minor: u32 = parts
            .next()
            .ok_or("missing minor")?
            .parse()
            .map_err(|_| "invalid minor")?;
        let patch: u32 = match parts.next() {
            Some(p) => p.parse().map_err(|_| "invalid patch")?,
            None => 0,
        };
        if parts.next().is_some() {
            return Err("too many components");
        }
        Ok(Self {
            major,
            minor,
            patch,
        })
    }

    /// Pre-1.0 we treat every minor bump as breaking; post-1.0 we'd allow
    /// equal major and host minor >= target minor.
    pub fn is_compatible_with(target: Self, host: Self) -> bool {
        if host.major == 0 {
            target.major == 0 && target.minor == host.minor
        } else {
            target.major == host.major && target.minor <= host.minor
        }
    }
}

impl core::fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// On-disk mod metadata. Lives next to the entry scripts as `manifest.toml`.
///
/// Entry scripts live at fixed filenames inside the mod's directory —
/// see the crate-level docs for the `data.lua` / `events.lua` split.
/// The manifest is metadata only; no script paths.
#[derive(Clone, Debug, Deserialize)]
pub struct ModManifest {
    pub name: String,
    pub version: String,
    /// Target API version, e.g. "0.1.0".
    pub api_version: String,
}

/// Which side of the engine a mod registry runs on. Determines which scripts
/// from the manifest are loaded and which hooks the engine table exposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Client,
    Server,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Client => "client",
            Side::Server => "server",
        }
    }
}

pub mod shared {
    use super::*;

    #[derive(Clone, Copy, Debug, Serialize, Deserialize)]
    pub struct BlockPos {
        pub x: i32,
        pub y: i32,
        pub z: i32,
    }
}

pub mod server {
    use super::*;

    /// Fired after a place-or-break edit has been applied to the authoritative
    /// world. Server-only because it sees the canonical post-edit state.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct BlockPlacedEvent {
        pub pos: shared::BlockPos,
        pub block: blocks::BlockId,
    }
}

pub mod client {
    // No client-side hooks yet — placeholder so the partition is visible to
    // mod authors. Add hooks here as systems that need them appear.
}
