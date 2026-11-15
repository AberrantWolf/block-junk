# Foundation content examples

`independent/foundation` is an independent base-content fixture. Its IDs, scripts
and embedded glTF asset do not refer to vanilla. Validate it without loading
vanilla:

```sh
cargo run -p block-junk -- validate-mods --mods-dir examples/mods/independent --preset foundation:plain --json
```

Start a session with the same `--mods-dir` and `--preset` options. Joining clients
must load the same content. New-world creation selects the only available preset
automatically; with several presets it requires a selection. Population is
initialized once when the world is created, independently of connections.

`addons/quiet_colony` requires vanilla and explicitly replaces its planner. To
try the composition, copy the `vanilla` and `quiet_colony` directories into a
separate mod root and select `vanilla:colony`. No vanilla files need editing.
It halves the plank recipe duration, makes rest outrank available work for its
configured need range, disables hauling for the patched NPC kind, and selects
facilities by additive capability. Lua receives bounded read-only candidates;
the engine reserves and executes only the selected stable candidate.

State registers with `engine.state.register(version, default)` in data.lua.
Migrations register with `engine.state.migrate(previous_version, function(old)
... return replacement end)`. Migrations cannot issue world commands. Runtime
world-state writes use `engine.commands.push` with `kind="set_state"`,
`scope="world"`, `expected_revision=engine.state.revision("world")`, and JSON text
in `json`. Read values with `engine.state.get("world")`. State writes and the
terrain and construction commands commit or reject together; results
are read with `engine.commands.results()`. NPC state writes validate live NPC
identities. Placement state writes validate durable furnishing IDs. Space state
writes use durable architectural identities and explicit lineage resolution.

The deterministic colony scenario is exercised by production-service and
two-App UDP acceptance tests; these directories are also usable examples.

World generation uses `algorithm="sine_heightmap"` and `version=1`. The preset's
`features` table controls `tree_one_in`, `forage_one_in`, `gravel_one_in`,
`tree_height={min,max}`, `canopy_radius`, and `gravel_radius={min,max}`. A density
of zero disables that feature. These values are saved and checked before clients
reconstruct procedural chunks.

Runtime terrain commands use `read_cell` and `edit_terrain`, with a world `cell`
and the expected chunk `revision`; reads and edits return cell receipts through
`engine.commands.results()`. `deliver_project` names a project ID/revision,
NPC ID, item ID and count. It transfers the NPC's existing carried material only
when the NPC can reach the project. A failed later command rejects the whole
batch, including terrain, escrow, carry and state changes.

`addons/wide_bed` demonstrates a compatible structural save upgrade for the
independent base. It expands a bed and renames its slot while retaining placement
identity, escrow and timed-use progress. Its README explains contract digests and
read-only save validation. Occupied destination cells reject the upgrade.
