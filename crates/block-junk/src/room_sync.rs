//! Room-state replication: matched rooms mirrored to clients.
//!
//! Detection stays entirely server-side (`rooms.rs`); this module ships
//! the *results* so the player actually sees them — before this, room
//! events reached Lua mods and the server log and nothing else, which
//! read in play as "detection doesn't work."
//!
//! Wire shape follows the plans/stations convention: [`RoomsFullSync`]
//! once on connect, then [`RoomSync`] upserts / [`RoomRemove`] deletes,
//! all on [`StateSyncChannel`] so full-sync-before-delta ordering holds.
//! Only *matched* rooms cross the wire — unmatched regions are detector
//! bookkeeping.

use bevy::prelude::*;
use lightyear::prelude::*;
use std::collections::HashMap;

use crate::menu::AppState;
use crate::protocol::{
    GameReady, GameSet, RoomRemove, RoomSummary, RoomSync, RoomsFullSync, StateSyncChannel,
};
use crate::rooms::{RoomEventMsg, RoomMap, RoomPatternRegistry};
use crate::worldspace_toast::{PendingToasts, SpawnToast};
use block_junk_mod_api::rooms::{RoomEvent, RoomPatternId};

// ---------- server ----------

pub struct RoomSyncServerPlugin;

impl Plugin for RoomSyncServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(send_rooms_full_sync_on_connect);
        // PostSimulation: `process_dirty` writes RoomEventMsg during
        // Simulation, so the broadcast sees same-tick events (same
        // scheduling as `dispatch_room_events` for mods).
        app.add_systems(
            Update,
            broadcast_room_events.in_set(GameSet::PostSimulation),
        );
    }
}

/// On a new client connect, push every currently matched room. Empty
/// syncs still send — the receive side treats the message as "the full
/// state is now exactly this."
fn send_rooms_full_sync_on_connect(
    trigger: On<Add, GameReady>,
    rooms: Res<RoomMap>,
    mut senders: Query<&mut MessageSender<RoomsFullSync>>,
) {
    let Ok(mut sender) = senders.get_mut(trigger.entity) else {
        return;
    };
    sender.send::<StateSyncChannel>(RoomsFullSync {
        rooms: rooms.matched_summaries(),
    });
}

/// Forward detector events to all clients as mirror deltas. Reads the
/// same local bus the Lua dispatch does; both readers see every event.
fn broadcast_room_events(
    mut reader: MessageReader<RoomEventMsg>,
    rooms: Res<RoomMap>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for RoomEventMsg(event) in reader.read() {
        let result = match event {
            RoomEvent::Created { room, .. } | RoomEvent::Changed { room, .. } => {
                // `Changed { to: None }` never surfaces publicly (the
                // detector emits Destroyed instead), so a missing
                // summary here means the room vanished again within the
                // same batch — skip; the Destroyed event handles it.
                let Some(summary) = rooms.summary_of(*room) else {
                    continue;
                };
                broadcast.send::<RoomSync, StateSyncChannel>(
                    &RoomSync { room: summary },
                    server,
                    &NetworkTarget::All,
                )
            }
            RoomEvent::Destroyed { room } => broadcast.send::<RoomRemove, StateSyncChannel>(
                &RoomRemove { room_id: room.0 },
                server,
                &NetworkTarget::All,
            ),
        };
        if let Err(err) = result {
            warn!("room sync broadcast failed: {err}");
        }
    }
}

// ---------- client ----------

/// Client mirror of the server's matched rooms. Read by the inspect
/// panel ("Room: Small house") and whatever debug/overlay wants bboxes.
#[derive(Resource, Default, Debug)]
pub struct ClientRooms {
    rooms: HashMap<u32, RoomSummary>,
}

impl ClientRooms {
    /// Smallest matched room whose bbox contains `cell` — smallest so a
    /// bedroom inside a walled compound reports the bedroom, not the
    /// compound.
    pub fn room_at(&self, cell: IVec3) -> Option<&RoomSummary> {
        self.rooms
            .values()
            .filter(|r| cell.cmpge(r.bbox_min).all() && cell.cmple(r.bbox_max).all())
            .min_by_key(|r| {
                let d = r.bbox_max - r.bbox_min + IVec3::ONE;
                d.x as i64 * d.y as i64 * d.z as i64
            })
    }
}

pub struct RoomSyncClientPlugin;

impl Plugin for RoomSyncClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ClientRooms>();
        // Full-sync before deltas within a tick, same as plans.
        app.add_systems(
            Update,
            (
                receive_rooms_full_sync,
                receive_room_syncs,
                receive_room_removes,
            )
                .chain()
                .in_set(GameSet::Simulation)
                .run_if(in_state(AppState::InGame)),
        );
    }
}

/// Resolve a pattern id to its display name via the client's own
/// registry; fall back to the raw id so a gap never hides the toast.
fn display_name(patterns: &RoomPatternRegistry, pattern: &str) -> String {
    patterns
        .get(&RoomPatternId::from(pattern))
        .map(|p| p.display_name.clone())
        .unwrap_or_else(|| pattern.to_string())
}

/// Join-time snapshot replaces the mirror wholesale, silently — a
/// joining player shouldn't get a toast storm for every room the
/// settlement already had.
fn receive_rooms_full_sync(
    mut receivers: Query<&mut MessageReceiver<RoomsFullSync>>,
    mut rooms: ResMut<ClientRooms>,
) {
    for mut receiver in receivers.iter_mut() {
        for sync in receiver.receive() {
            rooms.rooms = sync
                .rooms
                .into_iter()
                .map(|room| (room.room_id, room))
                .collect();
        }
    }
}

/// Upsert deltas: a room appearing or changing type toasts at its
/// anchor. Geometry-only updates (same pattern, walls moved) stay
/// silent.
fn receive_room_syncs(
    mut receivers: Query<&mut MessageReceiver<RoomSync>>,
    mut rooms: ResMut<ClientRooms>,
    patterns: Res<RoomPatternRegistry>,
    mut toasts: ResMut<PendingToasts>,
) {
    for mut receiver in receivers.iter_mut() {
        for RoomSync { room } in receiver.receive() {
            let name = display_name(&patterns, &room.pattern);
            let toast_text = match rooms.rooms.get(&room.room_id) {
                None => Some(name),
                Some(prev) if prev.pattern != room.pattern => Some(format!("Now: {name}")),
                _ => None,
            };
            if let Some(text) = toast_text {
                toasts.push(SpawnToast {
                    cell: room.anchor,
                    text,
                });
            }
            rooms.rooms.insert(room.room_id, room);
        }
    }
}

/// A room stopped matching (or was destroyed): drop the mirror entry
/// and toast at its last known anchor.
fn receive_room_removes(
    mut receivers: Query<&mut MessageReceiver<RoomRemove>>,
    mut rooms: ResMut<ClientRooms>,
    mut toasts: ResMut<PendingToasts>,
) {
    for mut receiver in receivers.iter_mut() {
        for remove in receiver.receive() {
            if let Some(prev) = rooms.rooms.remove(&remove.room_id) {
                toasts.push(SpawnToast {
                    cell: prev.anchor,
                    text: "No longer a room".to_string(),
                });
            }
        }
    }
}
