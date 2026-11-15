# Colony foundation milestone ledger

The [implementation plan](colony-foundation-rework.md) defines completion. Detailed
implementation notes are in [history](colony-foundation-history.md). The full
foundation rework is complete against its accepted gates.

| Milestone | Status | Delivered behavior | Remaining gate failures |
|---|---|---|---|
| 1 — Material transactions | Complete baseline | Project identity/escrow, footprint validation, refunds, exactly-once placement and committed crafting inputs; service/save/UDP checks pass. | None against its accepted gate; preserve regressions. |
| 2 — Reference spatial contract | Complete baseline | Thirty rendered fixtures, rotations/order permutations and edits; exact geometry, openings, multilevel surfaces and unknown boundaries. | None against its accepted finite reference gate. |
| 3 — Replaceable content | Complete | Shared composition/validation, presets and independent bootstrap, explicit callback ownership, frozen queries/provenance, resource sandbox, namespaced state migration, authoritative command batches, placement/NPC identities, crafting/construction jobs and durable causal execution. | Gate passed: independent bootstrap, add-on ownership, staged upgrades/failures, bounded callbacks and authoritative service/UDP acceptance. |
| 4 — Live spatial lifecycle | Complete | Cooperative exact live geometry, demand/dependency caches, durable identity/lineage/events, authoritative portal state, exact-footprint replication, facility settlements and reconnect/save rebuilding. Old production detector and bbox ownership removed. | Integrated gate passes; measured cave convergence latency remains an explicit M7 performance concern. |
| 5 — Capabilities and inspection | Complete | Typed additive rules/evidence, exact installed/usable/available slots, outdoor areas, UDP publication, inspector highlighting, revision-tagged pending placement preview, examples and generated rules. | None against the accepted gate; preserve mixed-room, profile, scenic and reconnect regressions. |
| 6 — NPC policy | Complete | Bounded Lua-first candidates, explicit job participation, stable revision validation, deterministic reservation, durable outcomes and cleanup. Native craft/haul selection before policy is removed. | None for the shipped humanoid profiles; new body shapes require their physical controller contract before content validation accepts them for NPC kinds. |
| 7 — Colony scenario | Complete | The deterministic production-service scenario and real two-App UDP suite cover the fixed sequence in loopback/dedicated forms; both base contents, the planner add-on, validator, generated rules and measured performance report ship together. | No gate failure. The report records the 64-chunk cave latency as the current measured scale limit. |

## Current M3 behavior and evidence

- Independent preset bootstraps once, including without connected players. Real
  signed UDP negotiation rejects content/configuration mismatch before GameReady.
- Dependencies/replacements, keyed list patches and field provenance feed frozen
  definitions. Runtime queries survive startup buffer draining. Decoded numeric
  validation catches Lua-to-f32 overflow before definitions or patches publish.
- Stable mod asset roots validate referenced resource graphs. Execution, memory,
  commands, result payloads, state and event chains are bounded. Failed callbacks
  discard writes and preserve unread command receipts.
- Construction/crafting escrow, facility inventories and pending item deliveries
  commit atomically with terrain and scoped state. NPC retirement preserves
  materials and owned state. Player/NPC/behavior reservations share owner types.
- Crafting and construction timed commands preserve reservations, progress/input
  commitments and structured outcomes. Replacement keeps its new worker claim;
  stale completion cannot release current work. Outcomes are explicitly acknowledged
  by their controlling mod. Binary saves round-trip populated job ledgers.
- M3 shipped save **33**; M4 now uses save **38** for identities, journals and facility homes, protocol **16**, mod API **0.5**. Pending event callbacks and
  causal budgets survive reload; delayed construction publishes its original cause.
  Older saves remain untouched and unsupported schemas reject explicitly.
- Named facility actions now declare timed use, crafting or storage. Slots reference
  distinct defined actions; malformed durations/consumption/need references fail.
  Vanilla metadata includes station/container/forage approaches as well as beds.
  Native and scripted timed uses now share stable slot reservations, service-owned
  timers and atomic stock/depletion/need completion. Replacement invalidates the
  original placement revision. Saved progress resumes once after revalidation;
  original exit surfaces survive behavior disablement. The real two-App UDP test
  observes the committed need change through NPC inspection.
- Focused tests cover distinct seats, shared-slot stock contention, stale completion,
  blocked depletion, binary progress reload, batch rollback and complete timed
  execution. Full workspace and strict Clippy passed for this interaction slice.
- Compatible structural upgrades now run on staged save copies through the shared
  startup/headless path. Exact source/destination contracts select explicit
  footprint, slot, movement and generation transformations. Tests preserve IDs,
  escrow and progress, reject collisions without touching the source, and retain
  old terrain around actors during a generation upgrade. The shipped `wide_bed` add-on passes disk-save/headless acceptance, including
  rejection after a later state-migration failure without changing the source save.
- Whole-save persistence now supports 64 MiB encoded data with a bounded decoder;
  a 64-chunk fixture over the former 1 MiB limit round-trips through disk.

## Verification

Milestone 3 gate passed: `cargo fmt --all -- --check`,
`cargo test --workspace --all-targets`, and
`cargo clippy --workspace --all-targets -- -D warnings`.
Logs: `/tmp/m3-gate-workspace.log`, `/tmp/m3-gate-clippy.log`.
The independent and wide-bed production headless validators pass. The shipped
planner replacement passes through the production composition pipeline. Disk-save
acceptance verifies collision and state-migration failure without altering the
original generation. Real two-App UDP coverage verifies content/configuration
negotiation, rejected callbacks, timed facility outcomes, material conservation,
duplicate requests and disconnect cleanup.

The remaining native scheduler and legacy room detector belong to M6 and M4
respectively. No live-space or capability completion is claimed by this gate.

## Current M4 behavior and evidence

- Production geometry uses its own cooperative solver; the reference crate is a
  test-only oracle. Exact membership, volumes, furnishing membership and connection
  counts match all thirty fixtures in four rotations. The full live-chunk suite now
  checks each fixed contract space in four rotations and both insertion orders,
  including roofs and exact connection thresholds/volume/evidence. Live publication
  additionally covers skylight edits, stable-ID updates, reload and demand removal.
- Structural summaries separate furnishing occupancy from architecture. Geometry
  reads track unavailable anchors and failed queries. Chunk-local floor patches
  and horizontal air sections are cached and reconciled across chunk boundaries.
  Deterministic edit tests match complete rebuilds and verify reuse of unchanged
  neighbors. Reverse dependency indexes cover demanded structural summaries;
  supported failed opening candidates retain missing-chunk dependencies. The
  64-chunk cave converges in 36,758 ticks at the default 8,192-operation budget, with exact area/volume and no work for sixteen extra unrequested cave chunks. This is about 613 seconds at 60 Hz; final performance verification and latency improvement remain open.
- Space identities persist in save 38; unchanged membership and one-to-one edits
  preserve IDs, while splits/merges retain explicit lineage and retired state.
  Placement relocation preserves identity, authored state and inventory atomically.
  Sidecars rebuild from durable placement records; authored partitions, stairs and
  multi-cell blocks retain identities even without meshes. Save 35 rejects prior
  formats whose placement coverage could omit those architectural elements.
- Dataset 6 uses ordered, bounded revision frames over the shared spatial lane and
  token bucket. Clients validate complete changes before accepting them and wait
  for the matching terrain basis before exposure. Subscription loss hides fragments
  without retiring server identities. Authenticated two-App UDP coverage exercises
  replacement snapshots and subscription removal. Protocol is now 16.
- Focused logs: `/tmp/m4-topology-parity.log`,
  `/tmp/m4-live-publication-tests.log`, `/tmp/m4-space-network-tests.log`,
  `/tmp/m4-space-udp-tests.log`. Strict Clippy passed the initial transport slice
  (`/tmp/m4-network-clippy.log`). The expanded cache/fixture run passed in
  `/tmp/m4-cached-topology-tests.log`; workspace verification for the latest demand
  and sidecar changes passed. The causal event/lineage-state workspace run also passed (`/tmp/m4-events-workspace.log`). Strict Clippy is being rerun after the latest save validation checks.
- Spatial revisions queue durable bounded mod events. Coalesced edits retain and charge all causal ancestors; delivery backpressure preserves revisions. Retired region state is never copied automatically. Its owner can query paged lineage and atomically transfer selected state and resolve the retired record; stale recreation rejects. Save 36 persists journals, causal ancestry and resolution markers. Focused restore/backpressure and atomic lineage-state tests pass.
- Authored portal changes share revision-checked player/script transactions, reject closing through actors, preserve placement identity and replicate collision state over UDP. Save 38 preserves portal state and NPC home-facility identities. The new exact-footprint placement dataset is ID 7; the old room-summary dataset ID 5 is retired.
- Facility settlements now use deterministic components of participating facilities, architectural membership and portals, plus budgeted reachable outdoor links. Open/closed doors do not alter settlement identity. Focused tests cover blocked/unknown paths, committed wall edits, and no work for unrequested chunks. NPC homes follow stable facilities across component changes.
- The old production room detector, dirty queue, first-hit civilization clustering and bounding-box room ownership are removed. Prototype geometry is test-only. NPC visit snapshots and first-person inspection consume exact architectural membership; placement identities are available to the inspector and portal controls.
- Gate verification: `cargo fmt --all -- --check`, `cargo test --workspace --all-targets` (`/tmp/m4-final-workspace.log`), and `cargo clippy --workspace --all-targets -- -D warnings` (`/tmp/m4-final-clippy.log`) pass. Both production validators pass (`/tmp/m4-independent-validator.log`, `/tmp/m4-vanilla-validator.log`). Expanded live tests verify journal saturation/resumption, roof changes, split/merge lineage, furnishing identity and reload. Fresh-App authenticated UDP reconnect preserves space IDs. Settlement reconstruction discards derived caches and recovers facility membership. NPC home IDs survive compatible upgrades and serialization.
- Save-upgrade validation reconstructs geometry separately from omitted gameplay slots and rendering metadata. Earlier failures are resolved. API 0.5 exposes exact-space policy candidates and explicit voxel portal visuals; visual cells are validated and compiled per rotation. M4 is complete. The 36,758-tick cave measurement remains unchanged and must be assessed in M7; no scale or performance-target claim is made.

## M5–M7 completion evidence

- Typed, bounded capability definitions now use production composition, immutable
  queries, reference validation and evidence. Vanilla and the independent base use
  additive facility functions. The `area_comfort` add-on owns area-per-sleeper balance.
- Budgeted authored-approach observations distinguish installed, usable and available
  unique slots. The production mixed-room service test passes: furniture preserves
  structural area, obstructions reduce usability, reservations reduce availability,
  and a one-to-one expansion updates mod-derived capacity on the existing space ID.
  Verification: `cargo test -p block-junk --bin block-junk mixed_room_capabilities`
  (`/tmp/m5-mixed-room-tests.log`).
- Outdoor reachable service areas retain path dependencies, respect portals and
  deduplicate overlapping stable slots. Synthetic scenic evidence, actor profiles,
  unknown paths and exact interaction clearance have passing acceptance tests.
- Dataset 8 publishes capability evidence coherently over UDP. The inspector shows
  structural measurements, thresholds and failed evidence; placement preview reports
  its source revision and pending state instead of guessing hypothetical access.
- Lua policy receives bounded candidates only after explicit job-class participation.
  The engine validates stable revisions, reserves selected work and returns durable
  structured outcomes. The replacement add-on demonstrates rest priority, disabled
  hauling and capability-selected facilities.
- The deterministic production-service scenario and authenticated two-App tests cover
  the fixed colony sequence, including mid-craft persistence and reconnect. The
  independent, vanilla and replacement compositions pass headless validation.
- Final versions are save **39**, protocol **18**, and mod API **0.6.0**. Formatting,
  workspace/all-target tests and strict Clippy pass. Performance and scale evidence is
  recorded in [the reproducible report](colony-foundation-performance.md).

## Post-review corrections

- Native policy outcomes now follow committed construction/crafting results.
  Construction waits for its authoritative reply, including across multiple fixed
  ticks. Rejected or duplicate completions cannot grant a need benefit. Stale work
  preserves a replacement's claim. A committed policy-selected craft returns to
  the planner before starting another unit; interrupted work is not success.
- Planner errors and absent callback owners retain unread policy feedback. Behavior
  disablement records interruption and releases transient ownership.
- Cursor previews now run the shared structural decoder, cooperative topology,
  authored-approach checks and capability evaluator on an isolated staged edit.
  Their key includes terrain, placement and spatial revisions. Unavailable geometry
  or identities remain pending; local previews explicitly leave reservation-dependent
  observations unknown. No hypothetical placement or identity reaches live state.
- Inspection displays installed, usable and available counts separately from
  support status. Text, highlights and preview scope share clearance-bounded
  membership, and stale terrain prevents capability inspection from claiming readiness.
- Regression coverage includes staged-versus-committed access, immutable preview
  sources, missing/occupied footprints, stacked storeys, fully reserved facilities,
  callback failure retention, and authoritative/duplicate outcomes over real UDP.
- Verification after these corrections: 374 workspace/all-target tests pass (the
  existing performance matrix remains opt-in), strict Clippy and formatting pass,
  and vanilla plus independent/add-on production validation succeeds. The generated
  capability reference was regenerated through `validate-mods --rules-out`.
