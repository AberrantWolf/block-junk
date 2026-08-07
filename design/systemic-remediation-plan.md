# Systemic remediation plan

Date: 2026-08-06

This plan comes from a repository-wide review focused on replacing local guards
with boundaries and invariants that protect every caller. It covers the original
findings plus bugs found while tracing their adjacent data flows.

## Baseline

- `cargo test --workspace --all-targets` passes 149 tests.
- `cargo clippy --workspace --all-targets -- -D warnings` passes as of
  2026-08-07. The lint cleanup used named Bevy `SystemParam` contexts and query
  aliases rather than broad suppressions.
- Bevy and Lightyear skills have been replaced with project-accurate 0.19 and
  0.28 guides. The networking design guide now reflects the implemented chunk
  subscription and the remaining global-visibility gaps.

## Required invariants

1. No numeric ID, collection length, string, coordinate, float, or claimed
   identity received from the network or disk is trusted before validation.
2. Clients request actions; only the server decides when work is valid and
   complete and emits facts about the resulting world state.
3. `Connected` means the transport exists. Only an authenticated,
   content-compatible `GameReady` connection may receive or mutate game state.
4. Every spatial message and replicated entity uses one shared chunk-subscription
   service. `NetworkTarget::All` is reserved for genuinely global facts.
5. A save is decoded, migrated, and fully validated without mutating the ECS.
   Publication makes its blob and metadata visible as one durable generation.
6. Plugins own the lifecycle of their session-local state. Starting a second
   session is equivalent to starting a fresh process.

## Issue register

### Critical

**BND-01: unchecked registry slots can panic the server.** `BlockRegistry::def`
indexes directly, and untrusted `BlockSlot(u16)` values reach it through block and
plan requests. `ItemRegistry::def` and `RecipeRegistry::def` have the same unsafe
shape even where current callers happen to validate first. One hostile packet can
therefore become a denial of service.

**AUTH-01: block work is client authoritative.** The client owns tool checks and
timing, then sends a completed `BlockEdit`. The server validates reach and world
geometry but not elapsed work, required tool, material consumption, or whether the
requested block is placeable. A modified client can mine instantly and place
arbitrary registered blocks.

**NET-01: compatibility validation races game bootstrap.** Independent
`On<Add, Connected>` observers create the avatar, enable replication/AoI, and send
full state while `ModSetManifest` travels on another channel. Numeric registry
slots can arrive before the client accepts the manifest, so mismatched content can
be interpreted with the wrong definitions or crash the client.

**SAVE-01: a crash can publish an incoherent save generation.** The blob and
metadata are renamed separately. A crash after the new blob rename leaves old
metadata selecting an old positional decoder. Decode ignores the consumed byte
count, so a new blob can decode as an old prefix, silently lose trailing state,
and later be overwritten.

### High

**SEC-01: player identity is a spoofable `client_id.txt` number.** The server uses
the client claim to select persisted inventory and pose. The default netcode key
and an externally bound server do not bind that identity to a credential. Debug
C2S messages are always registered and handled without an authorization check.

**BND-02: inbound payloads and work per tick are not bounded at the wire
boundary.** Plan/storage batch vectors and request strings allocate before
handler-side truncation. Receivers are drained without a per-peer budget.
`RequestNpcDetails` performs a linear NPC scan per request. This permits memory,
CPU, and reliable-channel denial of service.

**WORLD-01: same-frame snapshots cannot reconcile each other.** Snapshot handling
adds chunk entities with deferred `Commands`, then queries neighbors. Chunks from
the same batch are not queryable yet, so padding between procedural and edited
neighbors can remain stale.

**WORLD-02: destructive block deltas depend on client pre-state.** A break delta
carries `EMPTY`; the client derives the old multi-cell footprint from whichever
anchor chunk it currently has. If only neighbor padding is loaded, ghosts outside
the anchor can survive. The server already knows the old slot and footprint and
must send the authoritative result.

**AOI-01: chunk subscriptions do not govern the rest of spatial replication.**
Applied block edits, NPCs, world items, rooms, plans, storage, stations, and
containers have global broadcasts or full syncs. This leaks remote state and
makes per-client bandwidth grow with the whole world. Initial edited snapshots
are also large and have no per-client byte budget.

**CRAFT-01: resumed and continuing station work is under-validated.** Resuming
`active_work` trusts the original tool check, and ticking only checks that a worker
booking exists. A player can resume with the wrong tool, walk away, unequip, or
lose a stop packet while work continues. Worker takeover semantics are implicit.

**SAVE-02: unknown item IDs silently destroy persisted assets.** Plans, station
and container inventories, world items, player inventories/tools, and NPC state
warn and drop missing item IDs. Block IDs correctly fail the load. Unknown recipe
and need identifiers are also accepted into partially usable runtime state.

**SAVE-03: save structure lacks a preflight validator.** Corrupt short chunk
vectors can later panic indexed access. Duplicate coordinates/IDs, zero or
overflowing identifiers, non-finite transforms/times/needs, invalid counts,
arithmetic overflow, inconsistent multi-cell sidecars, and station/container
state on the wrong block are not rejected before resources begin mutating.

### Medium

**SESSION-01: session cleanup is a central manual list.** The comment explicitly
asks new features to add resources to `client::cleanup_session`. Session-scoped
`Local` values are outside that list; notably `normal_mode_action_input` can carry
a stale work cell into the next session and send `WorkStop` for it. Pause/debug UI
locals can similarly leak presentation state.

**SAVE-04: serialized save bytes are nondeterministic.** Several station, plan,
chunk, NPC, world-item, and inventory collections originate in `HashMap` or ECS
query order and are not sorted. Equivalent worlds can produce different blobs,
weakening reproducibility, checksums, and backup comparison.

**MAINT-01: stale framework assumptions must not become new guidance.** The strict
lint baseline was restored on 2026-08-07, including moving the `EntityAabb` impl
before the test module and replacing repeated oversized signatures with named
contexts. Keep `-D warnings` in CI and update stale Bevy 0.18 source comments so
they do not preserve obsolete API assumptions.

## Systemic components

### 1. Validated boundary types

- Add fallible `get`/`try_def` APIs to every slot registry and reserve panicking
  indexing for construction-time code with explicit invariants.
- Introduce bounded wire newtypes for strings and collections, plus checked
  coordinate/count/duration types. Reject oversize values during deserialization
  or immediately before allocation, not after collecting a `Vec`.
- Route every C2S handler through a shared `ValidatedRequestContext` that resolves
  the authenticated player and checks readiness, capability, rate budget, finite
  values, registry references, reach, and current-world preconditions.
- Define per-message and per-peer token buckets, a maximum decoded message size,
  malformed-request strikes, disconnect policy, and O(1) `NpcId -> Entity` lookup.

### 2. Connection and authority state machines

- Add `Connected -> ContentValidated -> GameReady`. On transport connect, send a
  nonce plus protocol/mod manifest only. Require an acknowledgment bound to that
  nonce and expected hash; time out or disconnect mismatches.
- Insert `ReplicationSender`, spawn avatars, subscribe AoI, and send all full
  syncs only on `GameReady`. Move every feature observer to this single gate.
- Replace client-completed `BlockEdit` requests with typed start/continue/stop or
  place requests. Store authoritative progress server-side, revalidate on every
  tick, consume inventory once, and emit `AppliedBlockEdit` only on completion.
- Give debug/admin operations an explicit server-issued capability. Do not even
  install production handlers unless debug administration is enabled.
- Replace claimed numeric identity with a persistent signing key, nonce proof,
  and a server mapping from public-key fingerprint to `PlayerId`. Provide a
  one-time migration/claim path for old IDs. Until then, refuse public/dedicated
  startup in insecure mode unless an explicit trusted-LAN flag is supplied.

### 3. Spatial consistency and visibility

- Stage a drained snapshot batch in a `HashMap<ChunkCoord, StagedChunk>`, validate
  exact padded lengths and slots, reconcile padding among staged and loaded chunks
  in memory, then apply ECS mutations after reconciliation.
- Define `AppliedBlockEdit { anchor, old_slot, new_slot, orientation, ... }` or an
  equivalent authoritative affected-cell payload. Client application must not
  infer destructive footprints from local state.
- Make one `SpatialSubscriptions` service own client/chunk membership and target
  selection. Use it for snapshots, deltas, state partitions, and Lightyear entity
  visibility. Changes spanning chunks target the union of subscribers.
- Partition plans/storage/rooms/stations/containers by chunk. Send enter snapshots,
  exit removals, and targeted deltas. Apply the same visibility to NPCs and items.
- Add bounded RLE or equivalent chunk encoding, exact decompressed-size checks,
  near-first prioritization, and per-client per-tick snapshot byte budgets.

### 4. Transactional persistence

- Decode with an explicit size limit and require `consumed == bytes.len()`. Verify
  both metadata and embedded version before migration.
- Convert the entire decoded save into a `ValidatedLoadedWorld`. Accumulate all
  errors with field paths and do not mutate any resource/entity until validation
  succeeds. Missing content IDs fail with an actionable list rather than dropping
  possessions.
- Validate exact chunk length, uniqueness and ownership, block sidecars, all IDs,
  known recipes/needs, finite/ranged values, count relations, checked sums,
  allocator headroom, and block/state correspondence.
- Write each save as a new generation containing blob, metadata, format version,
  length, and checksum. `sync_all` files, atomically replace a small `CURRENT`
  pointer, and fsync the directory. Retain the previous generation and fall back
  to it after checksum, decode, migration, or preflight failure.
- Sort every stable collection before encoding. Add a v18 migration only after
  the validator and generation reader can protect existing v12-v17 fixtures.

### 5. Owned session lifecycle

- Move plugin-private resource cleanup into each plugin's `OnExit(InGame)` path;
  keep central cleanup limited to cross-plugin connection and replicated entities.
- Replace session-scoped `Local<T>` with resettable resources/components. Process-
  lifetime input-edge locals may remain only when documented and proven harmless.
- Add a two-session test that connects, mutates every session resource, exits, and
  reconnects with no stale work, selections, UI feedback, indexes, or entities.

## Implementation order and gates

### Phase 0: restore the safety baseline

The strict-clippy portion of `MAINT-01` is complete. Update the stale framework
comments, add strict clippy to CI, add hostile-input unit-test helpers and a two-App
UDP loopback harness, and record per-channel/per-peer bytes and rejected-request
metrics. Gate: tests and strict clippy both pass before behavioral work.

### Phase 1: close boundary panics and resource exhaustion

Implement `BND-01` and `BND-02`, migrate all handlers to the validated context,
and fuzz/proptest every C2S decoder and slot lookup. Gate: arbitrary slot values,
length prefixes, strings, floats, and request floods never panic or allocate/work
beyond configured limits.

### Phase 2: make readiness, identity, and permissions explicit

Implement `NET-01` and `SEC-01`, move all bootstrap observers to `GameReady`, and
bump `NETCODE_PROTOCOL_ID`. Gate: no game component/message precedes successful
manifest acknowledgment; timeout, mismatch, replayed nonce, spoofed identity, and
unauthorized debug requests are rejected in loopback tests.

### Phase 3: restore gameplay authority

Implement `AUTH-01` and `CRAFT-01` with shared predicates for tool, reach, target,
booking, recipe, and worker validity. Define deterministic takeover/disconnect
behavior. Gate: premature completion, wrong tool/material, movement out of range,
equipment change, target replacement, duplicate start/stop, disconnect, and lost
stop cannot complete work or duplicate/lose inventory.

### Phase 4: make chunks and AoI coherent

Implement `WORLD-01`, `WORLD-02`, and `AOI-01`; bump the protocol for the applied
delta/encoding changes. Gate: same-frame neighboring snapshots have correct seams,
multi-cell border breaks clear every loaded view, two distant clients see only
their subscriptions, enter snapshots include missed changes, leave removes state,
and a teleport cannot starve click-critical messages.

### Phase 5: make saves all-or-nothing

Implement `SAVE-01` through `SAVE-04`, retain golden migration fixtures for every
supported version, then publish v18. Gate: fault injection after every write,
fsync, and rename yields the previous or next complete generation; trailing bytes,
missing IDs, corrupt lengths, duplicates, NaN/inf, overflow, and invalid relations
fail before world mutation; equal worlds encode identically.

### Phase 6: make lifecycle ownership local

Implement `SESSION-01`, audit every `Local`, resource, index, and session entity,
and run the two-session test under normal and failed-connect exits. Gate: the test
passes without adding feature-specific entries to `client::cleanup_session`.

### Phase 7: final verification and rollout

- Run unit, migration, property/fuzz corpus, loopback authority/AoI, two-session,
  and crash-injection suites, then strict clippy and formatting.
- Exercise solo, hosted, client-only, mismatched-mod, reconnect, and save-upgrade
  smoke tests. Preserve the always-client UDP path in every mode.
- Measure a representative two-player scenario against the 40 kbps/player target;
  fail the performance test on unbounded growth rather than a single noisy sample.
- Land phases as independently reviewable commits with their tests. Do not combine
  save-format, protocol, and identity migrations in one irreversible rollout.

## Completion criteria

The work is complete when every issue above has a regression test at its owning
boundary, all C2S traffic passes the common validation/readiness/capability path,
all spatial traffic derives from one subscription set, loading is non-mutating
until full validation, crash tests prove generational recovery, a second session
is clean, strict CI is green, and measured traffic stays bounded as world size
grows outside a player's AoI.
