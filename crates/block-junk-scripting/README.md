# Content composition (mod API 0.6)

A mod declares a stable `id`, display `name`, semantic `version`, and
`api_version = "0.6.0"` in `manifest.toml`. Directory names do not determine
precedence. For example:

```toml
id = "example_balance"
name = "Example balance"
version = "1.0.0"
api_version = "0.6.0"
resources = ["helpers.lua"]

[dependencies]
vanilla = "^0.1"
```

`dependencies` are required version constraints. `optional_dependencies` impose
ordering and version constraints when installed. `incompatibilities` reject
matching installed versions. An `overrides = ["another_mod"]` edge explicitly
orders an installed target without a version constraint. Cycles, missing required
mods, duplicate identities and incompatible versions fail before scripts run.
Unrelated ready nodes sort by stable mod ID.

Every `data.lua` runs before any `patches.lua`. Data registers definitions;
patches explicitly replace or patch existing definitions. A write over another
mod requires a dependency or override relationship with the last writer.
Diagnostics identify both owners. A patch merges object fields and replaces
lists as whole values:

```lua
-- patches.lua
engine.items.patch { id = "vanilla:wood_log", bulk = 1 }
```

Blocks, items, rooms, recipes, needs, NPC kinds and animations expose `register`,
`replace` and `patch`. Definition `id` is mandatory. Settings expose their normal
`set_*` entry point in data and a `replace_*` equivalent in patches, such as
`engine.npcs.replace_work_defaults(...)`. Runtime cannot mutate definitions.
Source ownership history is retained in `LoadContext.provenance`.

The game composes content, validates engine references, then activates server
`events.lua`. A planner replacement uses
`engine.npcs.replace_planner("vanilla:wanderer", callback)` and requires the same
explicit ownership relationship. The replacement callback is selected by the
host. Native job scheduling is still present until Milestone 6, so replacing a
planner does not yet control every NPC job.

Each mod has a separate Lua state. Defaults allow 32 MiB of Lua memory and one
million instructions per entry script or callback; host callers can configure
`ScriptLimits`. Execution runs in a host-owned coroutine. Budget exhaustion
yields out to Rust and cannot be swallowed by `pcall`. File/system/debug/package
and coroutine libraries are unavailable. `require` can import only declared Lua
resources inside the mod directory, including canonical symlink containment.
Queued toast effects have count/size bounds and are discarded if the callback
fails. World commands have a separate staged transaction outbox.

Air belongs to the engine as `engine:air`, slot zero. Content agreement includes
canonical composed definitions and policy settings, including NPC kinds, needs
and animations. Current schemas are mod API 0.3, save 34 and protocol 13.

## Remaining Milestone 3 gate

Milestone 3's integrated gate has passed. Live topology/capabilities and Lua-first
NPC scheduling are the active subsequent milestones; the ledger records their
specific remaining failures.

World presets, structural metadata, stable callback/subscription ownership and
bounded command outboxes are part of API 0.3. See `examples/mods/README.md` for
independent bootstrap, explicit planner replacement and namespaced state examples.
`validate-mods` uses the production loader without rendering or networking. It
accepts `--mods-dir`, `--preset`, `--save` (saved world name), and `--json`.

Saved state migrates before runtime hook registration. All migrations stage their
results and reject the whole restoration on failure. Runtime writes use revisions
and join a construction/state batch. The current engine implementation supports
terrain reads/edits, construction/crafting commitments, material delivery and inventory
transfers, furnishing queries/removal, population, interaction jobs and namespaced
world/NPC/placement/space state. Each batch stages its immediate changes before commit.

Saved content compatibility compares referenced geometry and movement separately
from balance/planner content. Unsupported structural changes reject explicitly.
`engine.world_upgrades.register` declares an exact source/destination structural
transition; `validate-mods --json` reports the contract digests. See
`examples/mods/addons/wide_bed` for a furnishing expansion and slot rename.

An upgrade contains a namespaced `id`, optional `blocks` and `movement` lists,
and an optional `generation` transition. Block entries name `block`,
`from_contract`, `to_contract`, `footprint` (`preserve` or `rebuild_at_anchor`)
and `slot_renames`. Movement entries name `profile` and both contracts.
Generation names the destination `preset`, both contracts and
`terrain_policy="preserve_retained_regions"`. Missing or conflicting transitions
reject; the destination digest must match the final composed content.

The engine stages upgrades before mod-state migration and runtime hooks. It
preserves placement IDs and material commitments, validates new footprints and
approaches, and rejects collisions. Generation upgrades retain existing edited
chunks and actor neighborhoods; unexplored procedural regions use the new
configuration. Validation never writes the original save. Missing referenced
content and unsupported identity changes still reject explicitly.

`read_placement` returns a durable placement ID/revision and use-slot IDs (placement
ID plus authored key). `remove_placement` requires that ID and revision. Destruction
retires the identity; placing another furnishing at the same cell creates a new ID.
Owned state remains attached to the retired object and is never copied to its
replacement. Live space identities and lifecycle integration remain M4 work.

`transfer_facility` addresses a `placement_id` and its `revision`, an `npc_id`,
`item`, positive `count`, `bank` (`storage` or `crafting`) and `direction`
(`deposit` or `withdraw`). The complete requested bundle must fit; partial
transfers reject the batch. Crafting commitments cannot be withdrawn. Retirement,
old-stock delivery and deposits into a replacement participate in the same staged
transaction. Blocked item deliveries persist until loaded geometry permits them.

`queue_craft` takes the facility's `placement_id`/`revision`, a `recipe` ID and
`quantity` (1–99). Its receipt returns the globally unique `order_id` and initial
order `revision`. `cancel_craft` takes the facility identity, `order_id` and
`order_revision`; stale or missing orders reject the batch. Duplicate recipe
orders remain distinct and cancellation refunds only the selected active order.

`spawn_npc` takes a registered `npc_kind` and a finite `position` in loaded,
unobstructed geometry. Its receipt returns `npc_id` and lifecycle `revision` 1.
`remove_npc` requires that identity/revision; repeated removal rejects. Carried
items and tools become durable delivery obligations, while owned state remains
under the retired NPC identity. ID allocation and state writes roll back together
when any command in the batch fails.


`start_craft` takes `placement_id`, placement `revision`, `npc_id`, `order_id`
and `order_revision`. It accepts a reserved job and commits the actual recipe
inputs atomically with the rest of the batch. A subsequent server tick binds the
worker after deferred actor creation; spawning and starting in one batch is valid.
The actor must be available and within interaction reach. Accepted jobs can fail
validation before work begins; their input commitment remains refundable.

`read_craft_job` takes `job_id` and returns its state, revision, actor, controller,
target and failure reason. `read_craft_jobs` pages the calling mod's retained jobs
with `after_id` and `limit` (1–64). Reading does not acknowledge an outcome.
`acknowledge_craft_job` requires its terminal revision and owning mod; only then
may later job creation retire the result. The current save schema retains job outcomes; active
jobs become interrupted on load and require validated resumption. Disabling a
controlling mod releases reservations and preserves committed materials.

`engine.commands.results()` consumes receipts only when its callback successfully
returns a valid result. Failed callbacks retain them for retry. Authoritative
commands are server-only; client callbacks receive an explicit error.


`start_construction` takes `project_id`, project `revision`, and `npc_id`. It
reserves a satisfied project and accepts a timed job. The next tick validates the
worker, loaded footprint, tool and reach before beginning work. Cancellation,
replacement, interruption and failed completion leave progress or refund escrow
under the construction service. `read_construction_job`, `read_construction_jobs`
and `acknowledge_construction_job` follow the crafting result/query conventions.
Job IDs are scoped to their owning service; each result's typed target identifies
that service. One actor cannot accept conflicting construction, crafting or facility interaction jobs in a batch.

Save 33 persists the service job ledgers, queued event callbacks and accumulated causal
budgets. Timed jobs retain their initiating cause; construction completion emits
its geometry events under that cause. Pending callbacks, command batches and
unacknowledged jobs protect their causal budget from eviction. A full set of
protected causes returns explicit backpressure rather than resetting a budget.


`start_interaction` takes `placement_id`, placement `revision`, `npc_id`,
`slot_key`, `action`, and an authored world-space `approach` cell. It validates
and reserves the named slot in the same staged batch as other commands. The actor
must already be at that approach; travel intent selection remains the policy
milestone. Deferred activation checks the approach again and applies an optional
slot pose. Omitted poses mean standing use. Poses specify the model's feet origin,
relative to the anchor's bottom centre; the engine converts to the actor eye pose.

The facility service advances timed actions and validates the original placement
revision, slot lease, approach and actor position. Completion atomically consumes
any declared stock or depletion and restores the declared need. Missing stock,
a destroyed/replaced furnishing or interrupted use grants no benefit. Separate
named exclusive slots on one placement reserve independently. Explicitly
nonexclusive slots support competing users; stock is still consumed exactly once.

`read_interaction_job`, `read_interaction_jobs` and
`acknowledge_interaction_job` use the same result ownership, revision and bounded
paging rules as crafting. Save/load retains progress and interrupts transient
leases. A later validated request for the same actor, controller and target can
resume that saved progress once. Failed callbacks release their owned leases;
unacknowledged outcomes and causal budgets remain available.

`move_placement` takes `placement_id`, `revision`, `anchor` and `orientation`.
It preserves identity and contents, interrupts current facility users, and commits
its complete footprint atomically. An identical transform is a no-op; stale
revisions or an occupied destination reject the whole command batch.

Architectural spaces now replace the prototype room detector. `on_space_event`
receives bounded Created, Updated, Split, Merged and Removed headers. Use
`read_space` and paged `read_space_lineage` to inspect a revision. Retired space
state stays with its original owner until `resolve_retired_space_state` commits;
mods may transfer selected state in that same command batch. The engine never
copies arbitrary state or inventories into split children.

`set_portal` takes `placement_id`, placement `revision`, `key` and `open`.
Opening and closing preserve architectural separation and threshold ownership;
only traversal and explicitly authored visuals change. Closing through an actor
rejects the batch. Furnishing IDs, portal state and NPC home-facility references
persist in save 38. API 0.5 replaces `snapshot.nearby_rooms` with
`nearby_spaces`: stable architectural ID, spatial revision, nearest floor anchor
and distance. Empty caves do not become destinations merely by being detected.

A voxel-rendered door can explicitly hide its leaf cells while open:

```lua
portal_visuals = {
    {portal = "passage", hidden_cells_when_open = {{0,0,0}, {0,1,0}}}
}
```

Cells must belong to the placement footprint and may appear in only one visual
entry. The portal key must exist. This setting affects voxel rendering only;
custom mesh animation is authored separately. It never supplies structural frame
or shelter evidence. First-person inspection selects the closest authored portal
and shows `[E] Open` or `[E] Close`; requests still pass through the server service.

Facility settlement participation is explicit: `settlement = "seed"`, `"joiner"`
or `"none"` (default). Participants require named slots and approach surfaces.
Seeds activate components; joiners connect through shared architectural spaces,
portals and the selected world's bounded reachable outdoor links. Mere cave
recognition contributes no population or work. Open/closed portal state does not
change indoor settlement membership. Unknown outdoor paths keep the result pending.
