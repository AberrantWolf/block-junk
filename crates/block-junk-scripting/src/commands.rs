//! Bounded callback outbox. The engine validates and commits each batch.
use crate::{LoadContext, state::StateScope};
use mlua::{Lua, LuaSerdeExt, Table, Value};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

pub const MAX_COMMANDS: usize = 64;
pub const MAX_BATCH_BYTES: usize = 128 * 1024;
pub const MAX_QUEUED_BATCHES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryBank {
    Storage,
    Crafting,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Deposit,
    Withdraw,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    SetPortal {
        placement_id: u64,
        revision: u64,
        key: String,
        open: bool,
    },
    ReadSpace {
        space_id: u64,
    },
    ReadSpaceLineage {
        space_id: u64,
        revision: u64,
        direction: block_junk_mod_api::architecture::LineageDirection,
        after_id: u64,
        limit: u8,
    },
    ResolveRetiredSpaceState {
        space_id: u64,
        state_revision: u64,
    },
    MovePlacement {
        placement_id: u64,
        revision: u64,
        anchor: [i32; 3],
        orientation: block_junk_mod_api::blocks::Cardinal,
    },
    StartInteraction {
        placement_id: u64,
        revision: u64,
        npc_id: u64,
        slot_key: String,
        action: String,
        approach: [i32; 3],
    },
    ReadInteractionJob {
        job_id: u64,
    },
    ReadInteractionJobs {
        after_id: u64,
        limit: u8,
    },
    AcknowledgeInteractionJob {
        job_id: u64,
        revision: u64,
    },
    StartConstruction {
        project_id: u64,
        revision: u64,
        npc_id: u64,
    },
    ReadConstructionJob {
        job_id: u64,
    },
    ReadConstructionJobs {
        after_id: u64,
        limit: u8,
    },
    AcknowledgeConstructionJob {
        job_id: u64,
        revision: u64,
    },
    AcknowledgeCraftJob {
        job_id: u64,
        revision: u64,
    },
    ReadCraftJobs {
        after_id: u64,
        limit: u8,
    },
    StartCraft {
        placement_id: u64,
        revision: u64,
        npc_id: u64,
        order_id: u64,
        order_revision: u64,
    },
    ReadCraftJob {
        job_id: u64,
    },
    SpawnNpc {
        npc_kind: String,
        position: [f32; 3],
    },
    RemoveNpc {
        npc_id: u64,
        revision: u64,
    },
    QueueCraft {
        placement_id: u64,
        revision: u64,
        recipe: String,
        quantity: u32,
    },
    CancelCraft {
        placement_id: u64,
        revision: u64,
        order_id: u64,
        order_revision: u64,
    },
    TransferFacility {
        placement_id: u64,
        revision: u64,
        npc_id: u64,
        item: String,
        count: u32,
        bank: InventoryBank,
        direction: TransferDirection,
    },
    ReadPlacement {
        cell: [i32; 3],
    },
    RemovePlacement {
        placement_id: u64,
        revision: u64,
    },
    ReadCell {
        cell: [i32; 3],
    },
    EditTerrain {
        cell: [i32; 3],
        expected_revision: u64,
        block: Option<String>,
        #[serde(default)]
        orientation: block_junk_mod_api::blocks::Cardinal,
    },
    Designate {
        cell: [i32; 3],
        block: Option<String>,
        #[serde(default)]
        orientation: block_junk_mod_api::blocks::Cardinal,
    },
    CancelProject {
        project_id: u64,
        revision: u64,
    },
    DeliverProject {
        project_id: u64,
        revision: u64,
        npc_id: u64,
        item: String,
        count: u32,
    },
    SetState {
        scope: StateScope,
        expected_revision: u64,
        json: String,
    },
}
#[derive(Clone, Debug, Serialize)]
pub struct CommandBatch {
    pub id: u64,
    pub mod_id: String,
    pub behavior: String,
    pub cause: Option<crate::events::EventCause>,
    pub commands: Vec<Command>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandReceipt {
    Space {
        space: block_junk_mod_api::architecture::SpaceInfo,
    },
    SpaceLineage {
        space_id: u64,
        revision: u64,
        direction: block_junk_mod_api::architecture::LineageDirection,
        space_ids: Vec<u64>,
        has_more: bool,
    },
    Jobs {
        jobs: Vec<block_junk_mod_api::authority::JobRecord>,
        has_more: bool,
    },
    Job {
        job: block_junk_mod_api::authority::JobRecord,
    },
    Npc {
        npc_id: u64,
        revision: u64,
    },
    CraftOrder {
        placement_id: u64,
        order_id: u64,
        revision: u64,
    },
    InventoryTransfer {
        placement_id: u64,
        npc_id: u64,
        item: String,
        count: u32,
    },
    Placement {
        cell: [i32; 3],
        placement_id: Option<u64>,
        revision: Option<u64>,
        use_slots: Vec<block_junk_mod_api::architecture::UseSlotId>,
    },
    Materials {
        project_id: u64,
        accepted: u32,
    },
    Cell {
        cell: [i32; 3],
        block: Option<String>,
        revision: u64,
    },
}
#[derive(Clone, Debug, Serialize)]
pub struct CommandResult {
    pub receipts: Vec<CommandReceipt>,
    pub batch_id: u64,
    pub accepted: bool,
    pub reason: Option<String>,
}
#[derive(Clone, Default)]
pub struct CommandOutbox {
    pub batches: Arc<Mutex<VecDeque<CommandBatch>>>,
    pub results: Arc<Mutex<std::collections::BTreeMap<String, VecDeque<CommandResult>>>>,
    next: Arc<AtomicU64>,
}
#[derive(Clone)]
pub(crate) struct Invocation {
    owner: String,
    pub behavior: Arc<Mutex<String>>,
    pub pending: Arc<Mutex<Vec<Command>>>,
    pub results_read: Arc<Mutex<usize>>,
    outbox: CommandOutbox,
    cause: Arc<Mutex<Option<crate::events::EventCause>>>,
}
impl Invocation {
    pub fn publish(&self, from: usize) -> mlua::Result<()> {
        let mut pending = self.pending.lock().unwrap();
        if pending.len() != from {
            let mut batches = self.outbox.batches.lock().unwrap();
            if batches.len() >= MAX_QUEUED_BATCHES {
                return Err(mlua::Error::external("command outbox is full"));
            }
            let id = self
                .outbox
                .next
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map_err(|_| mlua::Error::external("command identity space exhausted"))?;
            batches.push_back(CommandBatch {
                id,
                mod_id: self.owner.clone(),
                behavior: self.behavior.lock().unwrap().clone(),
                cause: *self.cause.lock().unwrap(),
                commands: pending.drain(from..).collect(),
            });
        }
        let mut read = self.results_read.lock().unwrap();
        let mut results = self.outbox.results.lock().unwrap();
        if let Some(queue) = results.get_mut(&self.owner) {
            queue.drain(..(*read).min(queue.len()));
        }
        *read = 0;
        Ok(())
    }
}

pub(crate) fn install(
    lua: &Lua,
    side: block_junk_mod_api::Side,
    ctx: &LoadContext,
    owner: &str,
) -> mlua::Result<()> {
    let invocation = Invocation {
        owner: owner.into(),
        behavior: Arc::new(Mutex::new("runtime".into())),
        pending: Default::default(),
        results_read: Default::default(),
        outbox: ctx.commands.clone(),
        cause: Default::default(),
    };
    lua.set_app_data(invocation.clone());
    let engine: Table = lua.globals().get("engine")?;
    let table = lua.create_table()?;
    let server = matches!(side, block_junk_mod_api::Side::Server);
    let phase = ctx.phase.clone();
    let pending = invocation.pending.clone();
    table.set(
        "push",
        lua.create_function(move |lua, value: Value| {
            if !server {
                return Err(mlua::Error::external(
                    "authoritative world commands require a server callback",
                ));
            }
            if phase.load(Ordering::Relaxed) != 3 {
                return Err(mlua::Error::external(
                    "world commands are only available during runtime callbacks",
                ));
            }
            crate::definitions::validate_input(&value)?;
            let command: Command = lua.from_value(value)?;
            let mut pending = pending.lock().unwrap();
            if pending.len() >= MAX_COMMANDS {
                return Err(mlua::Error::external("command count limit exceeded"));
            }
            let bytes = pending
                .iter()
                .chain(std::iter::once(&command))
                .try_fold(0usize, |total, c| {
                    serde_json::to_vec(c).map(|bytes| total + bytes.len())
                })
                .map_err(mlua::Error::external)?;
            if bytes > MAX_BATCH_BYTES {
                return Err(mlua::Error::external("command byte limit exceeded"));
            }
            pending.push(command);
            Ok(())
        })?,
    )?;
    let read = invocation.results_read.clone();
    let results = ctx.commands.results.clone();
    let id = owner.to_owned();
    table.set(
        "results",
        lua.create_function(move |lua, (): ()| {
            let mut read = read.lock().unwrap();
            let results = results
                .lock()
                .unwrap()
                .entry(id.clone())
                .or_default()
                .iter()
                .skip(*read)
                .cloned()
                .collect::<Vec<_>>();
            *read += results.len();
            lua.to_value_with(&results, crate::lua_event_options())
        })?,
    )?;
    engine.set("commands", table)?;
    Ok(())
}

pub(crate) fn behavior(lua: &Lua, id: &str) {
    if let Some(invocation) = lua.app_data_ref::<Invocation>() {
        *invocation.behavior.lock().unwrap() = id.into();
    }
}

pub(crate) fn cause(lua: &Lua, cause: Option<crate::events::EventCause>) {
    if let Some(invocation) = lua.app_data_ref::<Invocation>() {
        *invocation.cause.lock().unwrap() = cause;
    }
}

#[cfg(test)]
mod side_tests {
    use super::*;
    #[test]
    fn failed_callbacks_and_invalid_returns_preserve_unread_results() {
        let lua = Lua::new();
        lua.globals()
            .set("engine", lua.create_table().unwrap())
            .unwrap();
        let context = LoadContext::new();
        context.phase.store(3, Ordering::Relaxed);
        install(&lua, block_junk_mod_api::Side::Server, &context, "test").unwrap();
        context
            .commands
            .results
            .lock()
            .unwrap()
            .entry("test".into())
            .or_default()
            .push_back(CommandResult {
                receipts: vec![],
                batch_id: 42,
                accepted: true,
                reason: None,
            });
        for body in [
            "assert(#engine.commands.results() == 1); error('failed callback')",
            "assert(#engine.commands.results() == 1); return {}",
        ] {
            let callback = lua
                .load(format!("return function() {body} end"))
                .eval()
                .unwrap();
            assert!(crate::sandbox::call::<u64>(&lua, callback, ()).is_err());
            assert_eq!(context.commands.results.lock().unwrap()["test"].len(), 1);
        }
        let callback = lua.load("return function() assert(#engine.commands.results() == 1); assert(#engine.commands.results() == 0); return 7 end").eval().unwrap();
        assert_eq!(crate::sandbox::call::<u64>(&lua, callback, ()).unwrap(), 7);
        assert!(context.commands.results.lock().unwrap()["test"].is_empty());
        assert!(context.commands.batches.lock().unwrap().is_empty());
    }
    #[test]
    fn client_callbacks_reject_world_commands_instead_of_accumulating_an_unused_outbox() {
        let lua = Lua::new();
        lua.globals()
            .set("engine", lua.create_table().unwrap())
            .unwrap();
        let context = LoadContext::new();
        context.phase.store(3, Ordering::Relaxed);
        install(&lua, block_junk_mod_api::Side::Client, &context, "test").unwrap();
        let error = lua
            .load("engine.commands.push {kind='read_cell',cell={0,0,0}}")
            .exec()
            .unwrap_err();
        assert!(error.to_string().contains("server callback"));
        assert!(context.commands.batches.lock().unwrap().is_empty());
        assert!(
            lua.app_data_ref::<Invocation>()
                .unwrap()
                .pending
                .lock()
                .unwrap()
                .is_empty()
        );
    }
}
