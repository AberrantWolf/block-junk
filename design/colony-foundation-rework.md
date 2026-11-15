# Colony foundation rework — replacement implementation plan

## 1. Objective, current state, and completion rule

Deliver a working foundation for a friendly, first-person colony simulation with automatic architectural spaces, additive capabilities, reliable construction accounting, and a replaceable vanilla mod.

This replaces the previous phases. Preserve useful prototype code and tests, but do not treat passing prototype tests as completion of a gameplay feature.

The following current-state inventory is the baseline at plan authorization. The
[milestone ledger](colony-foundation-ledger.md) records implementation status.

**Already implemented at authorization**

- An isolated structural-space reference crate, including exact furnishing membership, explicit partitions, rectangular-opening inference, and authored stair connections.
- Duplicate recipe ingredients and construction material entries fail at load time.
- Regression tests and a generated fixture sheet.

**Not implemented at adoption**

The live game still uses the old room detector, construction records, NPC scheduling policy, and mod platform. Construction cancellation still loses delivered materials.

**Execution rules**

- Implement the milestones below in order. Each ends with integrated behavior and explicit acceptance tests.
- Integrate persistence, networking, and cleanup with each owning milestone; do not postpone them to a final hardening phase.
- Keep experimental geometry work confined to Milestone 2. Its required outcomes are specified below; it is not an open-ended investigation.
- Continue through authorized milestones without requesting another “proceed.” Ask only when evidence reveals a genuine conflict with the accepted product rules.
- Record milestone status, delivered behavior, verification, and remaining failures. Test counts alone are not progress criteria.
- Replace obsolete production paths when their replacements pass. Retain the simple detector as a test oracle.

The entire rework is complete when the final colony scenario passes in solo and multiplayer using both vanilla and an independent base-content mod.

## 2. Locked behavior and architecture

### Architectural spaces, facilities, and natural caves

Maintain separate representations for:

1. **Structural geometry:** supports, walls, partitions, openings, and roof coverage.
2. **Architectural spaces:** exact geometry and connections.
3. **Facilities:** uniquely identified furnishings and usable interaction slots.
4. **Capabilities:** mod-defined functions and attributes derived from those facilities and their surroundings.

Furniture never changes structural room area or footprint. It can obstruct access and reduce usable capacity.

An open hall with stepped floors or an upper gallery is **one architectural space with distinct floor surfaces**. Stairs through a separating doorway connect different spaces. Mere walkability between two points does not establish shared room membership.

Natural caves may form architectural spaces. Recognition alone must not create settlement membership, jobs, population, or functional benefits. Facilities and explicit mod behavior activate those systems.

No global cave census is required. Compute geometry for requested chunks and their necessary dependencies, using existing player subscriptions and active simulation demands. Exploration history and fog of war are separate future features.

### Boundaries and openings

- Structural walls and windows separate spaces.
- Declared doors, gates, and arches preserve separation regardless of open/closed state; their traversal state changes independently.
- Automatically recognize rectangular openings and stepped voxel arches, including openings through thick walls.
- A stepped arch must have continuous structural jambs and a connected overhead profile whose height stays level or rises toward the middle, then stays level or falls toward the opposite jamb. The passage must continue through the wall thickness.
- Furniture cannot supply structural frame evidence.
- An unframed breach joins spaces where the surrounding geometry otherwise connects them.
- Ambiguous crossing frames receive no inferred partition. Authored architectural elements express unusual arrangements.
- Keep passage geometry and adjoining room membership explicit. A thick doorway is a connection with its own threshold footprint, not floor area arbitrarily awarded to one side.
- Inference returns inspection evidence and is rebuilt from authoritative geometry. Never persist inferred partitions as authored blocks.

Architectural separation is distinct from air or weather connectivity. A courtyard can be bounded but open to the sky; a skylight changes roof coverage without joining the room to every outdoor space.

### Measurements and capabilities

Retain exact structural area, floor surfaces, roof coverage, clearance, placement membership, and connection geometry. Report finite interior volume only when it is actually bounded; otherwise report open or unknown.

Use unique placement IDs and use-slot IDs. Count adjacency between distinct objects, not occupied cells. Objects spanning spaces do not duplicate capacity.

Evaluate sleeping, dining, crafting, storage, and shelter independently. Display names summarize functions without suppressing them. Remove inherited small-house size limits from unrelated functions.

Outdoor activities are facility-centered service areas. Their range and conditions are mod data, exposed in inspection. Service areas follow reachable surfaces and do not pass through architectural partitions unless their definition explicitly permits portal traversal. Overlap never duplicates or double-books facilities.

Set-piece recognition remains a separate future consumer of geometry and placement relationships. It does not define ordinary room membership.

### Authority and mod ownership

The engine owns geometry, navigation, transactions, reservations, and validated execution. Mods own content, balance, NPC priorities, capability rules, and optional responses to events.

All player, NPC, and scripted mutations use the same authoritative services. Preserve the existing always-client networking architecture and shared spatial subscription service.

## 3. Ordered implementation milestones

### Milestone 1 — Construction and crafting conserve materials

Replace direct plan-record mutation with an authoritative construction service.

**Implementation**

- Give each project a stable ID, revision, intended footprint, requirements, delivered-material escrow, progress, and reservations.
- Provide designate, replace, deliver, cancel, and complete operations.
- Identical designation is idempotent. Replacement atomically cancels the old project and creates the new one.
- Reserve and validate the entire multi-cell footprint.
- Cancellation releases reservations and returns escrow through the normal item-placement service. If no valid destination exists, retain a pending refund record until delivery succeeds; never delete it.
- Completion validates project revision, current geometry, materials, and worker authority before committing placement and consuming escrow together.
- Independent placement into a project footprint explicitly resolves or cancels the project.
- Crafting atomically consumes requirements and stores the actual committed input bundle. Cancellation refunds that bundle.
- Persist projects, escrow, pending refunds, and crafting commitments. Replicate their committed results through the existing spatial framework.

**Completion gate**

Cancellation, repainting, replacement, blocked completion, duplicate requests, competing workers, disconnects, and save/load conserve materials and cannot complete work twice. Test both service calls and two-App UDP requests.

This milestone does not depend on new room detection.

### Milestone 2 — Finish the spatial contract against a finite fixture suite

Complete the reference implementation for the locked rules. Do not expand this milestone into capabilities, NPC policy, or mod-loader work.

**Required fixtures and outcomes**

| Fixture family | Required outcome |
|---|---|
| Rectangular, L-shaped, U-shaped, and narrow interiors | Correct structural membership; bounding-box holes are excluded. |
| Wall-aligned beds, dense furniture rows, rotated and stacked furnishings | Structural area and boundaries remain unchanged; objects count exactly once. |
| Flat and stepped roofs, skylights, tall vaults, roof added last | Correct coverage and volume; every relevant edit changes the result. |
| Low-wall courtyard and floating canopy | Courtyard boundary and shelter remain separate; canopy does not imply enclosed walls. |
| Thin/thick rectangular passages and stepped arches | Recognized connection with explicit threshold geometry and evidence. |
| Damaged frames, windows, corridors, intersecting frames | No invented doorway; authored partitions take precedence. |
| Stepped hall and atrium with gallery | One space, separate floor surfaces, no double-counted area or volume. |
| Separate storeys joined through a stair doorway | Separate spaces connected by a portal. |
| Parapet, balcony, terrace, and natural cave ledges | Traversability does not automatically merge architectural spaces. |
| Unknown chunks and large cave continuation | Unknown/pending results rather than false enclosure or forced “outdoors.” |

Exercise rotations, insertion-order permutations, and representative geometry edits for these families.

**Completion gate**

Every fixture has a fixed expected result, a passing test, and an inspectable rendering. Document the supported opening rules and the distinction between connections and space membership. Remaining unsupported shapes must be explicitly outside the agreed contract, not hidden behind “further experiments.”

### Milestone 3 — Finish replaceable content and authoritative mod execution

Finish this gate before integrating live spaces. Subsystem tests alone do not close it.

**Content pipeline and ownership**

Use one pipeline for server startup, client startup and headless validation:
discovery → dependency resolution → definitions → patches → final validation/freeze
→ state restoration/migration → runtime hooks. Preserve deterministic dependency
ordering and explicit replacements. Replace reverse-order lookup with an explicit
callback-owner registry; disabling an owner never silently revives its predecessor.
All replaceable callbacks and settings enforce ownership. Event subscriptions are
additive and replacement addresses a stable registration ID.

Preserve definition and field provenance through patches, defaults, derived values
and reference validation. Diagnostics identify mod, source script, definition,
field and conflicting/missing reference. Lists replace by default; explicit
append, remove-by-key and replace-by-key reject ambiguous keys, missing targets and
duplicates. Reject unknown fields and invalid numbers. Publish immutable validated
content whose runtime queries survive release of startup buffers.

**World presets and independent bootstrap**

Introduce a mod-defined `WorldPreset`: stable ID/name; terrain roles; generation
algorithm/version, seed and parameters; player spawn; starting population kind and
position entries; movement-profile and settlement-setting references. Keep the
current generator and move its balance and required references into the preset;
do not add procedural cave generation.

New-world UI and CLI select the preset. Select automatically only when exactly one
exists. Save resolved preset/generation configuration; joining clients validate it
before procedural reconstruction. Remove required vanilla IDs and the hardcoded
NPC cluster. Initialize population once at world creation and persist its marker;
connect/reconnect never creates population.

Ship an independent base with its own IDs, terrain roles, furnishings, workstation,
recipes, movement profile and NPC policy. It may use no vanilla definition, script
or asset path.

**Structural metadata**

Define structural occupancy, support surfaces, partition faces, portal thresholds,
shelter occlusion independent of collision/rendering, furnishing footprints and
multiple named use slots. Slots specify poses, approaches, exclusivity and supported
interactions. Define traversal affordances and actor movement profiles. Compile
rotations once in validation. Furniture defaults to no structural contribution.
Reject invalid faces, duplicate slot keys, impossible approaches and inconsistent
portals. Port vanilla now; the live detector consumes this in M4, which removes
obsolete flags after parity.

**Transactions, state and failures**

Introduce typed `CommandBatch`, `CommandResult` and reservation-owner identities
for players, NPCs and mod behaviors. Commands address stable IDs/revisions and cover
placement/removal, construction, inventory transfers, crafting, interaction intents,
population and namespaced state. Validate against a staged transaction view and
commit all immediate effects together or reject all. Timed actions accept a job
then report its outcome. Route every player/NPC/script entrypoint through these
services; Lua cannot edit chunks, inventories, needs or reservation tables directly.

Keep Lua memory/instruction limits; bound command count, payload, state and event
chains. Generated events run on subsequent ticks with bounded work and causal IDs.
Failed callbacks discard commands and state writes. Disabled behaviors release
reservations, preserve escrow/committed inputs and produce structured failures.

Persist state by stable mod ID, scope ID and schema version. Scopes are world,
NPC, placement and space; globals are ephemeral. Migrate staged copies with world
mutation unavailable, and publish only after migration and reference validation.

Allow validated balance/planner updates to saves. Schema changes require migrations.
Changes to existing placement geometry, slot identities, movement or generation
require explicit compatible migration, otherwise reject actionably. Missing mods
owning referenced content/state reject. Failed upgrades leave original saves intact.
Fingerprint canonical definitions, effective configuration, selected preset and
relevant scripts/resources; retain compatibility metadata distinguishing balance
from structural contracts.

**Deliverables and completion gate**

Ship `validate-mods` using production validators, preset selection, optional saved
compatibility checking, readable/JSON diagnostics and nonzero failure exit; it starts
neither rendering nor networking. Ship the independent base and a balance/planner
add-on. Verify independent bootstrap and actual add-on behavior, dependency failures,
conflicting patches, malformed metadata, failed migrations, runaway callbacks,
rejected batches and reservation cleanup through services and real two-App UDP.
Only then close M3.

### Milestone 4 — Integrate architectural spaces and their lifecycle

**Placements and geometry**

Persist `PlacementId`, definition, orientation, anchor, revision and authored state.
`UseSlotId` is placement identity plus slot key. Rebuild cell/sidecar indexes from
records. Move/rotate preserves identity; destroy/replace allocates a new one.
Materialize and persist procedural interactable identities on first indexing in
demanded regions; ordinary terrain needs none. Read structure separately from
furnishing occupancy. Thresholds belong to connections, outside adjoining floors.
Translate every M2 fixture into live chunks/placements with unchanged expectations.

**Demand-driven computation**

Implement chunk-local floor/air-section topology, boundary connectivity and resumable
region assembly. Fixed pipeline:

1. Refresh structural summaries and furnishing indexes.
2. Rebuild affected floor patches and opening candidates.
3. Reconcile cross-chunk connectivity and multilevel membership.
4. Compute measurements and placement relationships.
5. Validate dependencies and publish a coherent revision.

Instrument every geometry read, including failed openings, empty roof probes and
unavailable chunks. Reverse dependencies invalidate on edits and availability changes.
Demand roots are shared player subscriptions and active simulation requests; additional
chunks are requested only as their dependencies. Unrequested caves create no spatial
jobs or colony updates. Use deterministic budgets/fair queues, coalesce edits, retain
unaffected work and discard stale results. Limits mean pending, never a room-size
limit or outdoor classification. Roofs use authoritative column summaries with
unknown states, never a fixed-height probe's end as evidence of sky.

**Identity, persistence and replication**

Durable SpaceIds are independent of reference rebuild keys. Exact unchanged
membership and one-to-one expansion/contraction preserve ID; the latter emits Updated.
Splits/merges allocate replacements and explicit predecessor/successor lineage.
Removed spaces retire IDs. Persist canonical membership fingerprints and compact
reconciliation membership; rebuild caches on load. Retired region state stays intact
until its owner resolves lineage; do not duplicate arbitrary state or inventory.

Commit complete revisions before Created/Updated/Split/Merged/Removed events. Content
or measurement changes emit Updated even if function is unchanged. Extend shared
spatial datasets with bounded revision-tagged pages. Clients stage required pages,
reject stale bases and request replacement snapshots before exposing a revision.
Order dependent snapshots/deltas. Subscription loss only removes visibility.

**Settlements and completion gate**

Use deterministic connected components of participating facilities; mods mark seeds,
joiners and nonparticipants. Relationships come from shared membership, portals and
bounded mod-defined reachable outdoor links. Empty caves activate nothing. Closing
a door affects access, not settlement identity by itself. Remove old production
room detection/bounding-box ownership after parity; retain the independent oracle.

Pass all live reference fixtures and randomized-edit versus rebuild tests. Cover
roof/frame edits, unknown dependencies, 64-chunk continuation, split/merge/state
retirement, save/load, reconnect and subscriptions. Prove unrequested caves inactive.

### Milestone 5 — Additive capabilities and first-person explanations

**Rules and evidence**

Provide typed declarative counts of distinct placements/slots; area, coverage,
clearance and environment; adjacency/relationships; comparisons, Boolean logic,
ratios and bounded arithmetic (min/max/division/rounding). Validate types/references
at load. Division by zero and unknown geometry yield explicit failure/unknown, never
fabricated values. Results retain observed values, requirements, contributing IDs
and reasons. Generate rule references and readable explanations from those definitions.
Sleeping, dining, crafting, storage and shelter evaluate independently; names summarize
functions rather than selecting an exclusive class.

**Facilities and service areas**

Track installed, usable and currently available capacities separately: occupied slots
remain usable. Authored approaches and actor movement profiles drive usability using
the same clearance/traversal rules as navigation and interaction validation. Expose
valid space associations without multiplying capacity; aggregate unique slot IDs and
reserve globally. Adjacency counts distinct objects, not cells.

Outdoor service areas expand from facility anchors over reachable surfaces with
mod-defined ranges/conditions. Partitions block expansion unless portal traversal is
explicitly permitted. Cache accessibility/service-area dependencies. Unknown paths
stay pending. Scenic interest is synthetic authored environmental data, not fluids.
Port vanilla to additive, forgiving rules. The add-on defines comfort capacity as
`min(usable sleeping slots, floor(structural area / area per sleeper))`; the engine
and vanilla do not impose that area requirement.

**Inspection, preview and completion gate**

Extend first-person inspection/preview without a management interface. Show boundaries,
floor surfaces, thresholds, coverage, volume status, all three slot capacities,
evidence and pending states. Selecting failed requirements highlights contributing
facilities, blocked approaches or missing geometry. Preview evaluates a staged edit
with the same code, reports its source revision and remains pending without dependencies.

A mixed-use space supplies all four facility functions. Furniture never shrinks
structure; blocked approaches lower usable capacity with explanations; clearing
restores it. Expansion updates area effects without reclassification. Test overlaps,
cross-space objects, actor profiles, persistence and UDP inspection updates.

### Milestone 6 — Mod policy chooses; engine services execute

Replace native craft/haul scheduling before Lua with read-only candidate providers.
Snapshots are bounded/deterministic and contain IDs/revisions, requirements, estimated
costs, capability references and availability. Enumeration reserves nothing. NPC
job-class participation defaults to none; vanilla workers opt into construction,
hauling and crafting; nonworkers never enter those jobs before policy.

Policy selects a candidate, facility interaction, movement, rest or idle. Needs and
interruption priorities stay mod-controlled. Shared services validate/reserve; resolve
same-tick contention by stable actor ID, report rejection and reconsider later.
Job states are accepted, travelling, working, completed, interrupted, rejected and
failed, with reasons/stable identities returned to policy.

Use movement profiles consistently for routing/interaction. Remove navigation
exceptions permitting physically impossible furniture traversal. Release reservations
on interruption, bad paths, destruction, behavior disablement, actor removal and
session teardown while retaining carried inventory, escrow and committed craft inputs.
Persist progress/commitments and rebuild transient leases through validation on load.
Player disconnect does not cancel autonomous NPC work they designated.

Port forgiving vanilla policy and extend the add-on to show rest priority, one kind
without hauling and capability-based facility selection. Gate: policy wins over
available crafts; nonworkers get no jobs; workers cannot double-book; stale candidates
reject; interruption/destruction conserve materials; reload/reconnect never duplicates
work. Cover services and two-App UDP.

### Milestone 7 — Integrated acceptance, performance and delivery

One deterministic runner uses fixed content, seed, actors and action order and drives
production services/network requests. Fixtures may establish the initial world, but
cannot bypass services for actions under test. Run solo loopback and dedicated paths:

1. Create the selected-preset world.
2. Partially supply, repaint, replace and cancel construction.
3. Complete an underground stepped hall/gallery.
4. Install sleeping, dining, storage and crafting facilities.
5. Expand; verify geometry, capabilities and mod events.
6. Obstruct and restore access.
7. Open, close, damage and rebuild a connecting doorway.
8. Save/reload during construction and committed crafting.
9. Reconnect a second authenticated client.
10. Apply planner/balance changes through validated session restart.
11. Repeat bootstrap and representative interactions without vanilla.

Assert conservation, exactly-once completion, unique slots, coherent revisions,
durable state, lineage and understandable explanations throughout. Capture inspector
views for visual verification.

Benchmark 1/10/100 requested spaces with 10/50/100 NPCs, including one connected
64-chunk cave. Compare identical active neighborhoods with/without unrequested caves.
Record hardware, build/profile/config, tick p50/p95/p99, queue depth, convergence,
allocations and bytes/client. Separate initial streaming from steady traffic and
assess 40 kbps/player explicitly. Saturation remains responsive; finite work converges
when edits stop. Report measured limits rather than unlimited scale.

Complete only when both networking modes pass with both bases, inspection explains
outcomes, and validator, examples, generated rule docs, performance report and ledger
are delivered. Every owning milestone includes persistence, network, malformed-input
and cleanup tests; fmt, workspace/all-target tests and strict Clippy; and applicable
save/mod-API/protocol/dataset version bumps. Old saves stay untouched and unsupported
formats reject explicitly. Keep conservation/reference regressions while removing
obsolete production paths.

## 4. Verification, performance, and compatibility

- Every milestone runs formatting, workspace tests, and strict Clippy.
- Add integration tests at the owning milestone, rather than relying on a final integration pass.
- Preserve the finite reference detector as an independent correctness oracle for optimized geometry.
- Test material conservation, exactly-once completion, coherent revisions, and lifecycle cleanup as invariants.
- Benchmark 1, 10, and 100 requested spaces, including one connected cave crossing 64 chunks, with 10, 50, and 100 NPCs.
- Record hardware, tick-time percentiles, spatial queue size, convergence latency, allocations, and bytes per client.
- Compare a fixed active neighborhood with and without additional unrequested generated caves. Unrequested caves must not enter spatial work queues or produce capability, settlement, or network updates.
- Use deterministic operation budgets for spatial work and configurable limits for script execution. A saturated queue must remain responsive and eventually converge after edits stop.
- Assess the existing bandwidth target separately from initial streaming. Report measured limits rather than claiming unlimited world or colony scale.
- Bump protocol, mod API, and save schemas with the milestones that change them.
- Preserve existing saves untouched. The unreleased rework may reject old formats explicitly; do not require a legacy migration subsystem to maintain accidental behavior. New-format saves must round-trip at every milestone.

## 5. Scope and final deliverables

Deliver the integrated systems, their acceptance tests, a headless mod validator, example base/add-on mods, generated rule documentation, first-person inspection, and a reproducible performance report.

Do not add combat, fluids, politics, a large set-piece catalog, procedural whole-building generation, general hot reload, or a 2D management interface.

Maintain the short [milestone ledger](colony-foundation-ledger.md). Chronological prototype notes live in [implementation history](colony-foundation-history.md), outside this normative plan. A milestone is complete only when its stated gate passes; additional discoveries are recorded as specific failures or separately scoped follow-ups, not used to keep expanding an unfinished phase.
