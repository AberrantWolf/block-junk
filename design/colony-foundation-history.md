# Colony foundation implementation history

Historical prototype notes; not requirements or milestone completion evidence.

## Implementation record

2026-09-08, first slice:

- Added `crates/block-junk-spaces`, an engine-independent finite-world reference
  detector with 19 structural-space regression tests and an SVG fixture renderer.
  See that crate's README for commands, supported semantics, and remaining geometry
  experiments. The live game still uses the existing room detector.
- Reference tests cover furniture-independent area, exact placement contacts,
  roof coverage versus enclosure, empty-probe dependencies, unknown observations,
  wide explicit portals, separate elevations, irregular membership, deterministic
  rebuilds, and area beyond the old flood cap.
- Implemented the unambiguous duplicate-requirement portion of Phase B in the
  current loader: repeated item IDs in recipe inputs or construction materials
  fail with the definition ID and both entry locations. Five regression tests
  include validation of the shipped Lua content.
- Remaining before the Phase A gate: content-derived partition metadata, automatic
  framed openings, natural vertical geometry/grouping, and outdoor activity areas.
  Transactional construction, spatial lifecycle, and mod-platform replacements
  have not been implemented. Existing plan-cancellation material loss is not fixed
  by rejecting duplicate requirements.

2026-09-08, second slice:

- Added an atomic experimental content adapter for oriented structural cells,
  furniture, partitions, and authored steps. It is not yet a Lua schema or live
  block-registry adapter.
- Added conservative automatic rectangular-frame recognition, positive inspection
  evidence, authored-partition precedence, and a derived-scene preparation API.
  Tested multiple opening widths/heights, frame damage, unknown support, corridor
  false positives, furniture independence, and authored window precedence.
- Authored step connections now require structural swept headroom and disappear
  on rebuild when obstructed by a beam or partition. Four orientations are tested.
- Added 14 tests (33 spatial tests total) and changed the fixture sheet's doorway
  to use inferred geometry. Natural step inference, thick/irregular opening rules,
  atrium grouping, outdoor activity areas, and production integration remain open.


## Replacement Milestone 1 — material ownership

Replaced live designation/cancellation with a construction command service and durable project ledger. Projects reserve the complete rotated footprint; identical repaint preserves escrow and progress. Replacement retires the previous identity, retains delivered materials for refund, and releases NPC/haul claims immediately. Completion validates the captured identity and worker, and consumes escrow only when the shared terrain edit commits. Multi-cell destruction validates all chunks before writing.

Craft starts validate and consume the whole input bundle atomically. Cancellation and destruction use that committed bundle; deposits reserve refund capacity and reject overflow. Fixed player deposits performed only inside `debug_assert!` in optimized builds. Blocked craft output retains committed work.

Save v19 includes project identities, footprints, progress, pending refunds and actual craft commitments. Protocol v9 carries changed spatial records and removes unused client-supplied material lists. Save snapshots run in `Last` after gameplay/deferred spawns. Earlier saves remain untouched and incompatible formats are rejected.

Verification: workspace tests, formatting and strict Clippy pass. The two-App UDP fixture executes production designation, deposit, repaint, cancel, player work, NPC completion, crafting and disconnect handlers; it checks spatial project replication, blocked completion, stale/unreserved NPC results, duplicate starts/cancels, paused craft commitments and deferred refund delivery. Saved-generation tests cover retained obligations, progress, exact inputs and corrupt ownership/refund rejection. Authentication is seeded after UDP connection in this fixture; identity challenge/signature tests own the authentication gate.

## Milestone 2 — finite reference contract

Completed the isolated architecture model and the fixed 30-case suite. Every case checks literal area/volume expectations in four rotations and both insertion orders, plus support removal/restoration. Focused tests cover roof and frame edits, threshold ownership, stepped halls, galleries, stair doors, outdoor ledges, and unknown continuation. Generated `target/space-contract.svg` and individual `target/contract-panels/*.svg` with asserted expectations; inspected rasterized panels, including unknown frame geometry. Opening rules and the connection/membership distinction are documented in `crates/block-junk-spaces/README.md`. Workspace tests, formatting and strict Clippy pass. This is the synchronous reference model; no claim of live integration or complete incremental dependency tracing is made.

## Milestone 3 — loader portion (gate still open)

Replaced vanilla-first directory ordering with discovery and semantic dependency/override resolution. Definitions register before an explicit patch phase, with typed replacements, object patches, list replacement and source-owner conflict diagnostics. Runtime activation follows engine validation; data is frozen. Planner replacements now select the authorized replacement callback. Removed superseded single-writer registration code. Added configurable Lua memory/instruction budgets, host-owned coroutine yielding (including protected-call runaway regression), declared local imports, bounded toast effects and rollback on callback failure. Canonical composed definitions/configuration now participate in the live content handshake. Air is engine-owned and no longer registered by vanilla; save v20/protocol v10/mod API 0.2 reject older incompatible content explicitly. No existing save files were edited.

This does not satisfy the full M3 gate: world presets, independent-base bootstrap, structural metadata, persisted mod state, general authoritative commands, reservation cleanup, final-validation provenance and headless validation/examples remain. M4–M7 have not started.

Verification for the delivered M3 portion: full workspace/all-target tests, strict workspace/all-target Clippy, formatting and diff whitespace checks pass. Focused loader tests additionally pass after the final same-owner override adjustment. The full milestone acceptance gate remains open as listed above.


## Milestone 3 continuation — 2026-09-08

Implemented shared production/headless validation, mod-defined presets and terrain
configuration, menu/CLI selection, independent content, preset handshake and one-time
population initialization. Added explicit callback owners, named subscriptions,
keyed list patches, frozen query snapshots and field provenance. Added separate
structural metadata with precompiled rotations and ported vanilla declarations.

State schemas and staged one-step migrations now restore before runtime hooks.
Added bounded command outboxes and lock-held state transactions with staged
construction designation/cancellation. Callback return decoding was moved inside
rollback after finding that malformed planner results could previously publish
commands before rejection. Failed batches preserve state and construction escrow.
NPC state scopes now validate authoritative identities.

Added saved referenced-structure/movement contracts without locking ordinary balance
or planner content. Structural upgrades currently reject pending an explicit migration
executor. Save schema is 24; mod API is 0.3; protocol is 11. Unsupported old saves remain
untouched, and failed load preflight prevents runtime hooks/population startup.

Found and replaced an unrestricted texture-data Lua evaluator with the shared sandbox,
including rejection of metatables and nonfinite/cyclic data. Texture parsing retains
its Lua owner for the complete parse; a first attempt dropping that owner was caught
by the texture regression suite and corrected. Texture load order now comes from
resolved dependencies rather than another directory scan.

Shipped independent-base and planner/balance add-on fixtures. Production add-on testing
caught replacement of an unset optional setting; the fixture now patches an existing
recipe's duration and verifies the explicit planner owner. No vanilla file is edited
by the add-on. Current evidence and remaining integrated failures are in the ledger;
M3 remains open and M4–7 have not been started.

## Milestone 3 continued execution — 2026-09-08

Added actual imported-script/line provenance, delayed bounded event delivery with
retained command causality and cumulative chain limits, structured behavior failure
reporting, and NPC claim/booking cleanup on behavior disablement. Added stable-mod-ID
asset resolution and validated/fingerprinted transitive glTF resource graphs.

Replaced production terrain placement/removal with staged geometry transactions.
Script batches now read terrain and perform revision-checked edits together with
construction and state commands. Failed depleted replacements retain the original
block. Fixed cancellation of an intervening project by later unrelated staged edits.
Chunk revisions persist and travel with snapshots/deltas; clients reject malformed
revision coverage and ignore stale edits. Save schema 25, protocol 12 and terrain
dataset revision 2 reflect those contracts. Added per-preset surface feature balance
and explicit generation algorithm selection while retaining the heightmap generator.

Focused regressions cover late batch rollback, same-batch geometry, stale revisions,
wide-object replication, asset graph changes, callback budget exhaustion and claim
cleanup preserving escrow. Fixed overflowing container deposits to reject without
losing items. Workspace checks are rerunning; the integrated M3 gate is still open.

Further M3 integration: strengthened portal/threshold consistency checks and
retained derived-drop provenance. Fingerprints now use canonical final frozen
definitions and survive startup-buffer draining. Asset validation now checks
buffer/view/accessor and sparse-index ranges and decodes images with bounded
memory/dimensions; JPEG is enabled in the rendering loader as well as validation.
Loaded/moving NPCs request bounded active terrain neighborhoods without clients.

Player deposits, NPC haul arrivals and scripted deliveries now share one material
transfer primitive. Scripted delivery stages carried inventory with construction
escrow, terrain and namespaced state; a late conflict rolls everything back. The
engine/UDP suites and focused carry/escrow rollback test pass. Full checks are
rerunning after the final asset MIME/runtime feature changes.

Added real UDP-triggered callback-batch rejection assertions to the production
construction scenario: failed commands leave no project or state write behind.
NPC self-consumption now stages geometry and need recovery together and suppresses
harvest drops explicitly. Regrowth uses atomic replacement, retains blocked work
for bounded retry, and does not mistake ghost cells for separate plants.

Introduced durable furnishing identity records as the prerequisite for stable
command targets. Normal terrain edits, consumption and regrowth stage identity
changes with geometry; scripted queries/removal and placement-scoped state use
those IDs. Destruction retires an identity and a replacement gets a new one.
Save schema 26 stores these records and rebuilds indexes during validated load.
This is command/identity groundwork, not completion of live spaces in M4.

### 2026-09-09 — Facility inventory transactions and durable item delivery

Added `TransferFacility` commands keyed by placement ID/revision and explicit
storage/crafting bank, with NPC reach, item, carry capacity and refund-headroom
checks. Native NPC and player station transfers use the same material operations.
A regression caught and corrected the NPC container withdrawal adapter before
integration: the carry and stock must change together.

Introduced touched-bank staging and moved facility retirement into authoritative
terrain transactions. This removes the delayed-observer race that could erase new
inventory after same-cell replacement. A production App regression covers old
stock, active inputs, new deposits, late batch rejection, identity replacement and
pending delivery serialization. Added fair bounded delivery of stacked items,
including a maximum-count stack behind 64 unavailable destinations. Save 27 adds
the pending item queue; older saves are rejected without modification. Construction
refunds and crafting output reuse the stack-placement primitive. Engine tests and
strict workspace Clippy pass; M3 is still open for its remaining services and gate.

### 2026-09-09 — Craft order identities

Replaced recipe-name cancellation with globally allocated order IDs and revisions.
Active work records its exact order, so a later same-recipe order can be canceled
without touching the running craft. Queueing/cancellation enter shared services
from player UDP and staged mod commands; late rejection also rolls back ID
allocation. The UI displays progress only on the active order. Save 28, protocol
13 and station dataset v3 carry the contract; imported IDs/revisions and active
work ownership are validated. All 179 engine tests, including real UDP duplicate
cancellation and disconnect regressions, pass. Remaining M3 gates are unchanged.

### 2026-09-09 — Population transactions

Added staged `spawn_npc` and `remove_npc` commands. Creation validates kind and
loaded body clearance and shares preset bootstrap's actor construction. Removal
retains carry/tool materials through the pending-item service, retires identity,
and releases construction, haul, interaction and crafting claims at commit.
NPC-owned state remains attached to its retired identity. Save 29 persists the
allocator and retirement set instead of rebuilding IDs from living actors. The
rollback/removal regression uses production replication plugins; it and strict
workspace Clippy pass. Full workspace verification continues.

### 2026-09-09 — Typed reservation ownership

Introduced common stable owner/lease semantics and atomic set acquisition.
Construction and interaction claims use the reservation table. Player construction
now acquires and releases generation-checked leases in the same table as NPC work,
with deterministic player ordering and lifecycle cleanup. Crafting replaces the
unconditional worker overwrite with typed NPC/player ownership, validates in-flight
NPC bookings against player work, and prevents concurrent work by one actor at two
stations. Existing engine/UDP tests pass; hauling and durable timed-job ownership
still require integration before M3 closes.


### Durable crafting jobs and callback outcomes

Added staged `start_craft`, bounded job result queries and owner/revision-checked
acknowledgment. Spawn-and-start rollback, disable/resume without a second input
consumption, exactly-once output and saved-job interruption are covered by the
production-command App regression. Failed callbacks and invalid return conversion
retain command receipts. Server-only command enforcement prevents a client-only
outbox from silently accumulating mutations. Save contract advanced to 30.

Verification: `cargo test --workspace --all-targets` and
`cargo clippy --workspace --all-targets -- -D warnings` passed for job acknowledgment;
subsequent hauling ownership and UDP station mirror checks are tracked in the
current ledger. These are M3 increments, not completion of its integrated gate.


### Timed construction and durable causal execution

Added construction acceptance/activation/outcomes, exact lease cleanup, replacement
claim reconciliation and scoped result queries. The UDP regression now begins a
scripted construction job, submits its engine completion, checks exactly-once
placement and checks the originating cause on emitted geometry edits. A stale
completion previously released a current claim; revision validation now precedes
release. Existing native brain timing remains the execution path, to be replaced
with policy-driven job execution in M6.

Populated job-ledger binary round trips exposed internally tagged API types that
bincode cannot deserialize. Added an explicit typed save representation, retaining
readable tagged Lua objects. Save 31 introduced construction jobs; save 32 adds
queued callbacks and retained event-chain budgets. Pending work prevents budget
eviction; exhausted protected capacity returns backpressure. Runtime hook activation
validates restored subscription ownership before callbacks can run.

Verification: `cargo test --workspace --all-targets` passed; focused construction
UDP and causal queue/reload regressions passed. Strict Clippy reruns follow current
edits. No milestone gate was closed by these increments.


### Superseded continuation ledger notes

Moved here to keep the current ledger concise. Statements about checks in progress
or outstanding work below reflect their point in the implementation sequence;
the current ledger supersedes them.


- Production loaders validate presets and terrain/population references. Joining
  clients accept the server-selected preset only after installed-content agreement.
  Two-App UDP tests use the actual authenticated handshake and reject mismatches.
- Independent content bootstraps one settler without any connection. The persisted
  initialization marker prevents an empty colony from respawning. A complete
  rendered independent colony scenario has **not** been demonstrated. Loaded and
  moving NPCs now request their active neighborhoods through shared chunk tasks
  without connected viewers; unrequested generated terrain is excluded.
- Named subscriptions and planner replacements have explicit owners; disabling a
  replacement does not reactivate earlier callbacks. Runtime queries read frozen
  definitions after startup buffers are drained. List patches reject duplicate or
  missing keys; finite/plain-data checks reject cycles and metatables.
- Runtime callbacks stage bounded command outboxes. Invalid planner/inspection
  results, errors and instruction-limit exhaustion roll back before publication.
  Current authoritative commands include staged terrain reads/edits, designation,
  revision-checked cancellation, world/NPC state writes and construction delivery
  from NPC carry. Players, NPC arrivals and scripts share the material-transfer
  operation; batch rollback restores both carried stock and escrow. A late batch conflict retains a supplied project's
  escrow and leaves state unchanged; committed results are available to policy.
  Terrain revisions persist and replicate; stale deltas cannot roll back newer
  snapshots. Remaining command classes and unified player/NPC routing are open.
- Saved state is versioned, bounded and migrated on staged copies before hooks.
  NPC and placement scopes validate durable identities. Placement records and
  named use-slot IDs commit with terrain; destruction retires the old identity
  and replacement allocates a new one. Space scopes await live architectural
  identities in M4. Saved compatibility rejects changed referenced structure/movement/generation
  and permits ordinary balance changes; **there is no compatible structural
  migration executor yet**. Older save files are not modified.
- Structural metadata is compiled in four orientations and vanilla authors it, but
  production room detection still uses the old path. Portal validation rejects
  disconnected/overlapping volumes, inconsistent floor thresholds and duplicate
  or detached partition faces. Interaction-reference validation and live movement
  profile use remain open.
- The headless validator supports preset selection, saved-world preflight and
  human/JSON results. Texture resources use the shared sandbox and dependency order.
  Stable-ID asset resolution validates referenced glTFs and animation indices,
  hashes external buffers/images and loader metadata, and rejects escaped paths.
  Buffer/view/accessor and sparse-index ranges are validated, images decode under
  dimension/memory limits, and image MIME types must match. Imported registration
  provenance identifies helper scripts and lines; generated drops record their
  material/multiplier dependencies. Final frozen content determines fingerprints,
  independently of drained startup buffers. Surface feature density and dimensions
  are preset configuration.

- Event callbacks run on subsequent ticks with bounded payloads, queue work, chain
  depth and cumulative breadth. Behavior failure releases NPC claims/bookings and
  retains supplied construction and committed crafting inputs. Generalized durable
  reservation owners and timed-action causal retention remain open.

Verification commands for this work:

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p block-junk -- validate-mods --mods-dir examples/mods/independent --preset foundation:plain --json
cargo run -p block-junk -- validate-mods --preset vanilla:colony --json
```

Focused acceptance tests live in `scripting::foundation_tests`,
`server::foundation_udp`, the scripting composition/state tests, and the retained
construction service/UDP and geometry oracle suites. M3 does not pass merely because
these implemented portions pass; the remaining gate failures above are required.

Current continuation: the workspace/all-target suite passed after final-definition
fingerprinting and portal checks; strict Clippy passed after buffer/image validation
and NPC simulation demand. Engine and UDP suites passed after shared construction
delivery; the late-failure delivery regression also passes. Full checks are rerunning
after JPEG runtime support and MIME validation. M3 and M4–7 remain open.

Current authority integration additionally stages NPC resource consumption with
need recovery, and regrowth uses an atomic replacement with bounded retries when
blocked. Direct chunk mutation now belongs to committed terrain transactions.
Save schema 26 persists furnishing identity records and rebuilds their cell indexes;
load/validator preflight checks record geometry against saved or procedural chunks.
Identity and scoped-state rollback/replacement tests are being verified. M3 remains
open; these prerequisites do not close the M4 spatial integration gate.

The current M3 continuation adds revision-checked facility inventory commands,
shared player/NPC transfer operations, and transactional inventory retirement.
Removing and replacing a stocked workstation in one batch preserves the old stock
and committed inputs, permits deposits into the new identity, and rolls everything
back on a later failure. Delayed cell observers no longer own inventory cleanup.
Save schema 27 stores pending item deliveries by stable content ID; unavailable
geometry retains them and a fair per-tick budget delivers stacks without per-unit
entity creation. Construction refunds and crafting output share stack placement.
The engine suite (178 tests, including two-App UDP regressions) and strict workspace
Clippy pass after this change. Full workspace verification continues; the remaining
M3 gate failures remain open.

Craft queue/cancel commands now share the production order services. World-owned
monotonic order IDs survive empty queues and facility replacement; cancellation
checks order revision and refunds only that order's active input commitment.
The first-person craft UI and UDP cancellation requests use the same identity.
Save 28 persists the allocator/order identity; protocol 13 and station dataset v3
carry the revised contract. Save and wire validation reject duplicate identities,
stale allocators, inconsistent revisions and orphaned active work. Engine tests
(179, including UDP) pass; strict Clippy and full workspace verification are running.
Timed interaction/job commands and generalized reservation ownership remain
required M3 work.

Population commands use staged allocation and the bootstrap actor-creation service.
Removal preserves carried materials/tools in pending delivery, releases reservations
at commit and retains NPC-owned mod state under a retired identity. Save 29 stores
the complete NPC allocator and retired IDs; load rejects inconsistent living/retired
records. A production-command App regression passes with real replication plugins,
covering rollback, removal, repeated requests, claim cleanup and identity reuse.
Strict workspace Clippy passes. Full workspace verification is running after this
addition; reservation ownership and timed actions remain open.

The reservation core now uses stable player/NPC/behavior owner types, atomic
multi-target acquisition and generation-checked release. Live construction and
interaction claims consume it; authenticated player construction shares the same
claim table as NPC construction, including stop/timeout/disconnect cleanup.
Crafting bookings and active workers share typed ownership, rejecting NPC/player
contention and one actor powering multiple stations. Engine and UDP regressions
pass. Hauling ownership, behavior-controlled timed jobs and durable job outcomes
remain required; this does not close the generalized-reservation gate.


Crafting timed commands now stage reservations, actual inputs and job acceptance
with the whole batch, including newly spawned actors. Deferred activation validates
actor/tool/reach and records working, completed, interrupted or failed results.
Save 30 persists the job ledger; load suspends transient work without consuming
inputs again. Owning mods explicitly acknowledge terminal results; failed callback
reads retain receipts. The workspace/all-target suite and strict Clippy passed
for this change. Hauling uses typed reservation owners and scripted crafting
rejects actors with existing haul assignments. Construction/interaction timed
commands, retained timed-event causality and the full M3 integration gate remain
open. Craft job results currently serve server policy; client station replication
continues to carry committed order/material/progress state.


Construction timed jobs now accept through staged claims/projects, activate after
actor creation, and report committed completion or interruption. Replacement
preserves the new worker's claim; stale completion does not release current work.
Service and real two-App UDP regressions pass. Binary save round trips now include
populated job records (a codec issue missed by earlier empty-ledger tests was fixed).
Save 32 persists construction/crafting outcomes, pending callbacks and causal work
budgets. Delayed jobs retain their cause through authoritative geometry publication;
queue draining and reload do not reset event-chain limits. The full workspace and
all-target test suite passes after these changes; strict Clippy is being rerun.
Remaining M3 failures include authoritative interaction intents, structural save
migration execution, remaining metadata validation and full entrypoint/gate coverage.
M4–7 remain pending; live room detection has not been replaced.


## 2026-09-09 — Named-slot authoritative interaction execution

Added typed facility action definitions, rotated slot use, original placement
revision checks, exact exclusive leases, service-owned timers, bounded job queries
and explicit acknowledgement. Both native NPC interaction dispatch and script
commands now enter this service. Removed the old NPC depletion message/consumer
and direct brain-side stock/need completion. A failed or stale action gives no
benefit; stock/geometry and need changes commit together. Save 33 stores interaction
jobs and remaining progress, with revalidated one-time resumption after load.
Causal retention includes interaction jobs. A transient occupancy record retains
an exit approach when behavior disablement removes the active job.

Focused tests pass for per-slot contention, nonexclusive last-stock consumption,
blocked replacement geometry, duplicate completions, binary reload/resumption,
rejected spawn/start batches and full timed service execution. The full workspace
passed before the timer extraction; final workspace/Clippy, headless validators
and the extended real UDP inspection test are being rerun. M3 remains open for
its remaining routing/cleanup gate and compatible structural save migrations.


## 2026-09-09 — Compatible save upgrades and larger retained worlds

Added `world_upgrades` definitions with exact BLAKE3 source and destination
contracts, using the ordinary composition/provenance pipeline. Startup and headless
validation now stage compatible upgrades before state restoration and runtime
hooks. Footprint rewriting uses the terrain transaction with old/new geometry
registries; identities, material escrow, crafting commitments and slot progress
remain owned records. Explicit slot renames cannot merge distinct use identities.
Movement changes validate retained actors; generation upgrades preserve edited
chunks and old terrain around retained actors/placements before adopting the new
preset. Collision and missing-contract tests leave the source save unchanged.

The 1 MiB save limit blocked retained neighborhoods. The encoded bound is now
64 MiB and the decoder remains bounded; a 64-chunk disk regression passes. Writes
preflight schema invariants before creating a new generation. Structural and
generation upgrade tests pass; production validator/add-on and complete owning
failure tests are still being completed. M3 is not closed by these subsystem tests.

### M3 save-upgrade and mutation-route acceptance

- Added `wide_bed`, an independent-base add-on with exact structural contracts,
  footprint expansion and a named-slot rename. The production headless validator
  accepts its disk-save upgrade, rejects an occupied destination, and rejects a
  later mod-state migration failure. Each validation leaves the original save
  generation unchanged. Generation and movement transitions preserve retained
  terrain and actor neighborhoods; explicit destination preset selection is honored.
- Reload now retains the exit context of an active snapped interaction and runs
  exit cleanup before policy activation. Interrupted historical jobs do not move
  actors on later reloads; durable progress remains available for revalidation.
- Audited terrain, construction, crafting, facility stock/use, population and
  material-transfer adapters. Found and fixed tool-stack loss in player/NPC pickup
  through one shared exchange operation. Player carry/tool drops now use durable
  bounded item delivery. Real UDP checks cover duplicate tool pickup and duplicate
  maximum-sized carry drops as well as the existing construction/crafting failures.
- Verification logs for this slice: `/tmp/m3-upgrade-state-failure-test.log`,
  `/tmp/m3-transfer-audit-udp.log`; final workspace/Clippy gate is recorded in the
  ledger after completion. Native candidate scheduling remains the explicit M6
  replacement; legacy live room ownership remains M4 work.

### M4 authoritative structural view and identities

- Live chunk structural summaries now run under a deterministic cell budget with
  fair quanta. Reads use rotated authored structural metadata independently of
  collision/rendering. Missing anchors and chunk reads retain dependency stamps;
  demanded elements materialize durable placement IDs. Unrequested generated chunks
  do not enter this queue. Focused tests pass; full spatial topology is still pending.
- Added atomic `move_placement`: preserves placement ID, authored state, facility
  stock, order identities and committed crafting inputs; releases transient users
  and suppresses destruction drops. The service/batch/save-index regression passes.
- Added compact exact membership records and durable space identity reconciliation:
  one-to-one changes emit Updated, splits/merges allocate successors with explicit
  overlap lineage, and retired records remain intact. Save schema 34 includes these
  records and space-owned state validates against them. Focused lifecycle tests pass;
  no production topology publication or completed M4 gate is claimed yet.

### M4 — cooperative geometry and revisioned transport

Added an independent production solver with deterministic yielding at geometry
reads, exact floor/air assembly and opening evidence. Reference parity covers all
thirty fixtures across four rotations. Live publication tests cover furniture
neutrality, skylight updates, durable identity rebuilding and stopping demand.

Added shared revision framing and dataset 6, with bounded pages, checksums, explicit
bases and typed validation before acceptance. Server transmission shares the
existing subscription service, ordered spatial channel and bandwidth token bucket.
Clients retain the last accepted state on rejection, request replacement snapshots,
and expose geometry only against matching terrain revisions. Authenticated UDP
acceptance covers terrain ordering, recovery and subscription removal. Protocol
bumped to 14. Canonical binary hashing replaces debug-format geometry fingerprints.

These are integrated M4 slices, not completion of its gate. Remaining work is
listed in the ledger, including live fixture translation, incremental topology
caches, bounded causal events, settlement replacement and large-cave acceptance.

### M4 — live fixture parity and reusable local topology

Translated all thirty fixed contract spaces into live chunks, metadata definitions
and placement sidecars. Four rotations and both insertion orders preserve exact
structural membership, fixed area/volume expectations, roof coverage, furnishing
counts and connection geometry/evidence. The finite oracle's observation perimeter
is not introduced as an artificial boundary in the live world.

Added reusable chunk-local floor patches and horizontal air sections, with
cross-chunk reconciliation and deterministic yielding. Randomized roof/wall edits
match the uncached full rebuild and retain unchanged neighboring cache entries.
Demanded structural summaries use reverse dependency indexes. Missing reads from
supported frame candidates remain explicit dependencies; downward probes handle
demanded chunks with no visible floor. Demand selection retains split lineage but
excludes unrelated spaces discovered inside dependency chunks.

Placement tracking now includes meshless partitions/stairs and multi-cell blocks.
Terrain edits retain their orientation sidecars, and load/generation rebuilds
sidecars from durable placement identities. Save version 35 rejects earlier
placement-coverage semantics; existing files are untouched.

### M4 — bounded cave continuation and durable spatial events

Cached opening candidates retain exact read dependencies and defer ambiguity
filtering until the complete set is assembled. The requested 64-chunk cave
converges to area 10,230 and volume 30,690 at 36,758 ticks with the default
8,192-operation budget. Sixteen unrelated cave chunks create no work or identities.
The measured latency remains a performance concern, not a claim of finished scale.

Spatial revisions now retain a durable event journal with delivery backpressure.
Coalesced edits join causal budgets without resetting ancestry. Save 36 persists
that journal and ancestry. Mod API 0.4 adds bounded space event headers, space
queries, paged lineage and explicit owner resolution of retired space state.
Resolution and selected successor writes share a staged transaction; rejected
batches preserve the parent state and stale recreation cannot undo resolution.
Workspace tests passed in `/tmp/m4-events-workspace.log`, with focused event
restore/backpressure and lineage atomicity coverage. The milestone remains open
for portal traversal, settlement replacement and the remaining lifecycle gate.

## M4 continuation — portal authority, facility settlements, production replacement

Delivered revisioned portal state through shared player/script services, collision caches,
and signed two-App UDP handling. Added budgeted outdoor surface traversal and deterministic
facility settlement components; persisted NPC home facility IDs (save 38). Removed old
production detector/dirty queue and bbox ownership; retained the prototype geometry oracle
under tests. Added exact footprint placement dataset 7 (protocol 16), architectural
inspection, and portal interaction controls. Procedural identity materialization now merges
live sidecar updates per chunk. Focused sidecar/settlement/UDP tests pass. The replacement
workspace run exposed geometry-only upgrade validation incorrectly requiring gameplay slots;
that validation boundary is repaired and gates are being rerun. No M4 completion claimed.

## M4 integrated gate passed; M5 begins

Final workspace/all-target tests, formatting and strict Clippy pass, as do both
production base-content validators. Added live journal backpressure/resumption,
split/merge and reload checks, fresh-App authenticated UDP reconnect, exact
placement snapshot revision guards, settlement cache reconstruction and home-ID
upgrade/serialization checks. Explicit voxel portal visuals and first-person
revisioned E-key controls are integrated. Save 38, protocol 16, API 0.5. The known
64-chunk convergence measurement remains an M7 performance concern. M5 now begins;
no capability or overall rework completion is claimed.

## M5 additive rules and live facility capacity

Added typed, bounded capability expressions to the production content pipeline,
with immutable definitions, reference validation and retained evidence. Independent
sleeping, dining, crafting, storage and shelter rules replace vanilla's exclusive
room registrations. Dining requires a distinct adjacent table. The independent
base defines its own four facility functions. The `area_comfort` add-on owns its
32-area-per-sleeper requirement and combines it with usable sleeping slots.

Authored approaches now have budgeted physical-clearance observations for each
movement profile. Installed, usable and available counts remain separate and
aggregate unique placement/use-slot IDs. The production mixed-room test passes:
adding beds preserves 64 cells of structural area; blocking approaches with
furniture reduces usable capacity; reservations affect availability only; adding
new floor expands the same space to 96 cells and increases add-on capacity.
An earlier expansion fixture legitimately merged an existing exterior terrace;
the one-to-one expansion fixture now creates new floor rather than asserting an
incorrect identity outcome for a merge. Log: `/tmp/m5-mixed-room-tests.log`.

Outdoor reachability and evidence are being integrated with dependency tracking;
this is not yet an M5 gate pass. Network publication, inspector/preview evidence,
shared authoritative interaction validation, scenic fixtures, persistence/reconnect
acceptance and generated rule documentation remain outstanding.

## M5–M7 integrated completion

Added typed additive capability rules with bounded evidence, unique installed,
usable and available slot accounting, authored approaches, outdoor service areas
and scenic environmental observations. Dataset 8 publishes coherent capability
revisions after their source spatial revision. First-person inspection shows exact
space measurements, thresholds, capacity evidence and blocked contributors.
Placement preview reports source space/capability revisions and stays explicitly
pending until a hypothetical furnishing has authoritative access geometry.

Replaced native craft/haul selection before Lua with bounded, deterministic,
read-only candidates. NPC kinds explicitly opt into job classes. Policy selects a
stable ID/revision, then shared services validate and reserve it. Accepted,
rejected, interrupted, failed and completed results return to policy and survive
saves. Same-tick contention resolves by stable NPC ID. Cleanup releases transient
reservations while preserving carry, escrow and committed craft inputs. The
`quiet_colony` add-on demonstrates rest priority, no hauling and capability-based
facility selection. Save schema is 39, protocol 18 and mod API 0.6.0.

The deterministic scenario now includes partial supply, atomic repaint/refund,
cancellation, additive mixed facilities, obstruction/restoration, one-to-one
expansion, area effects, portal damage/state/replacement and mid-craft persistence
of the committed input bundle and durable job. Authenticated two-App UDP tests
cover construction idempotence, conservation, capability/space revisions, portal
authority, reconnect and content negotiation. Independent, vanilla and
vanilla-plus-replacement content pass the production validator.

The reproducible scale report records 1/10/100 spaces crossed with 10/50/100
policy actors, queue convergence, percentiles, allocation calls/bytes and actual
serialized initial revision bytes. The 64-chunk demanded cave converges without
processing sixteen unrequested caves; its high latency is reported as the current
measured scale limit.

Final verification passed: `cargo fmt --all -- --check`,
`cargo test --workspace --all-targets`, and
`cargo clippy --workspace --all-targets -- -D warnings`. Production validation
passed for vanilla, the independent base, and vanilla plus `quiet_colony`.
