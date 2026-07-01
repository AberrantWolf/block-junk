//! UI extension surface — the `engine.ui` table.
//!
//! Deliberately small. Mods don't get windows or widgets (the engine's
//! UI toolkit churns too much to freeze into an ABI this early); they
//! get the two content-level hooks that cover most "my mod wants to
//! tell the player something" cases:
//!
//! - **`engine.ui.toast(pos, text)`** — both sides. Queues a transient
//!   worldspace toast anchored at `pos` (a `{x=, y=, z=}` cell table).
//!   Called server-side (e.g. from `events.lua` hooks or NPC planners)
//!   the engine broadcasts it to every connected client; called
//!   client-side it renders locally only.
//! - **`engine.ui.on_inspect(fn(target) -> string|nil)`** — registers a
//!   callback consulted when the player's hover-inspect panel renders.
//!   Return a string to append it to the panel body, or `nil` to add
//!   nothing. Client-side; registering in `data.lua` is fine (the
//!   server copy of the hook is simply never called).

use serde::{Deserialize, Serialize};

use crate::shared::BlockPos;

/// One queued `engine.ui.toast` request. The engine drains the queue
/// every tick; ordering within a tick follows call order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiToast {
    /// World cell the toast floats above.
    pub pos: BlockPos,
    pub text: String,
}

/// What the player is hovering, handed to `engine.ui.on_inspect`
/// callbacks whenever the inspect panel re-renders.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InspectTarget {
    /// One of `"block"`, `"item"`, `"npc"`, `"plan"`.
    pub kind: String,
    /// Registry id of the hovered thing: block id / item id / NPC kind
    /// id. For `"plan"`, the build-target block id (empty string for a
    /// Remove tag).
    pub id: String,
    /// Hovered cell for blocks and plans; `nil` for items and NPCs.
    pub pos: Option<BlockPos>,
}
