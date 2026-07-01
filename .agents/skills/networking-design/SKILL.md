---
name: networking-design
description: Design rules for block-junk's networking — what gets replicated and how, channel choices, bandwidth budget, and area-of-interest strategy. Use when adding any feature that crosses the client/server boundary, or when reviewing a change that adds replicated state. The transport mechanics (which lightyear API to call) live in the lightyear-026 skill; this is about *what to send and why*.
user-invocable: false
---

# Networking design rules for block-junk

This is a project-locked architecture, not a list of options. Every cross-boundary feature must obey these rules unless we explicitly revisit them. The "why" matters because it's the only way to judge edge cases.

## Always-client architecture (Quake/CS pattern)

There is no "host mode" in this project. **Every player runs only as a client.** Solo play just means *we also spin up a server thread* (a headless Bevy App listening on UDP-localhost) and the client connects to it. The wire format is identical to friends-mode.

**Why:** the alternative (lightyear's `HostClient` shared-world pattern) requires special-case dedup logic, observer-timing races between server-spawn and client-snapshot, two `ChunkMap` resources sharing one `World`, and an "are we host or split" branch in every cross-side feature. The cost of always-going-through-UDP-loopback is ~1 ms latency on a localhost packet — imperceptible. The benefit is **one code path** for solo, friends-host, and split modes. Networking bugs surface in the dev's own play session instead of "works in solo, breaks on someone else's machine."

The three modes:

| Command | What runs |
|---|---|
| `cargo run` (default = solo) | Server thread + client App on main thread, connected via UDP-localhost. |
| `cargo run -- server` | Just the server, headless. Listens on `SERVER_ADDR` (currently `127.0.0.1:5050`; configurable later). |
| `cargo run -- client` | Just the client. Connects to `SERVER_ADDR`. (Eventually take an `--addr` arg for friends-join.) |

Friends-host = run the server somewhere reachable (your machine with port-forwarded `5050`, or a small dedicated VM). Friends connect via `cargo run -- client --addr <host>`. Identical code path to your own local client.

This rule is load-bearing — adding a "shortcut for local play that bypasses serialisation" because it seems faster is exactly the kind of choice that sneaks divergence in. Don't.

## Bandwidth budget

**Target ≤ 40 kbps per player after compression.** That's roughly Minecraft-vanilla territory (sources: vanilla servers report 30–50 kB/s **per server** with 4–8 players, so a few kB/s per player; we target a comfortable overhead for the simulation depth we want).

Anything systemic that pushes past this needs a justification, not a workaround. If a feature *requires* more bandwidth, that feature gets cut to AoI sooner, throttled, or quantized harder.

## Replicate **events**, not state, for the world grid

Voxel chunks are huge — `32³` = 32,768 blocks raw, ~32 KB even at one byte each. If we let lightyear's `Replicate<Chunk>` push the full component on every change, **a single block edit replicates the entire chunk**. That's catastrophic at scale and silently encouraged by the framework.

**Don't replicate `Chunk` directly.** Server broadcasts edits as events:

1. Client sends `BlockEdit { coord, pos, block }` to server (request).
2. Server validates against its world, applies the change.
3. Server broadcasts the applied `BlockEdit` to all clients in the AoI of the affected chunk.
4. Each client's authoritative state = (initial snapshot received on join) + (accumulated `BlockEdit`s since).

Initial chunk state is sent **once per chunk per joining client** as a `ChunkSnapshot { coord, blocks }` message. The client spawns a local chunk entity and applies subsequent edits to it.

This is event-sourcing for world state. Cost per edit on the wire: ~10–20 bytes vs ~32 KB.

## Replicate **state**, not events, for entities

Players, NPCs, projectiles — anything with a continuous Transform that all clients need to render. Per-tick snapshots are small (~30 bytes after quantization) and lossy/unreliable is fine: the next tick's snapshot supersedes a dropped one. Use lightyear's `Replicate` machinery for these, with `SequencedUnreliable` channel underneath.

The split: **discrete world changes are events; continuous entity state is replicated state**. Apply this rule to every new replicated thing you add.

## Identify chunks by coordinate, not by `Entity`

Server and client live in different worlds, so server's `Entity` IDs don't translate. We could solve this with `MapEntities` on every cross-side message, or we can sidestep it: **chunks have a stable global identifier** — their `IVec3` chunk-grid coordinate. `BlockEdit` and `ChunkSnapshot` reference that coord.

Both sides keep a `ChunkMap: HashMap<IVec3, Entity>` resource so they can find the local entity for a chunk coord cheaply.

This rule extends to other replicated identities later: prefer stable IDs (PlayerId, NpcId) over Bevy entity IDs in any message that crosses the wire.

## Channel choices

| Data | Channel mode | Reliability | Frequency |
|---|---|---|---|
| `BlockEdit` (place/break) | `OrderedReliable` | reliable, in order | sparse (per click) |
| `ChunkSnapshot` (initial load) | `UnorderedReliable` | reliable | rare (on join, on chunk-stream-in) |
| Player/NPC transforms | `SequencedUnreliable` | unreliable, latest wins | every tick |
| Building-detection events | `OrderedReliable` | reliable | rare |
| Water-sim cell deltas (later) | `UnorderedUnreliable` (delta-batched) | unreliable | frequent — see "water" below |

**Decision rule**: if dropping the message *causes incorrect state when later messages arrive*, it's reliable. If a newer message *supersedes* the lost one, it's unreliable.

`BlockEdit` is reliable because dropping one means the client never agrees the block changed — all subsequent edits assume a wrong base. Player position is unreliable because dropping tick 100's snapshot is fine when tick 101's lands a frame later.

## Don't replicate procedurally-regenerable state

If a chunk has never been edited and is fully reconstructible from the world seed + chunk coords, the server doesn't send the bytes — it sends "chunk K is procedural-default" and the client regenerates. Voxel-Tools recommends this exact pattern (`info.are_voxels_edited()`).

Implement this once we have multi-chunk + procedural worldgen. For now (single hand-built sphere), every chunk is "edited" and gets a snapshot.

## Area of interest (AoI)

**Not yet — single chunk.** When multi-chunk lands, each player has a chunk-radius AoI (start at 8 chunks). Server only sends snapshots and edits for chunks within that radius.

Per Lysenko's classification, three viable styles:

1. **Rule-based** — "the player can't see this, don't send" (dimensions, fog of war).
2. **Static partitioning** — fixed grid; player subscribes to N nearest cells. *This is what we'll use.*
3. **Geometric** — prioritize by projected area / distance / visual salience.

For a Minecraft-shaped game, static partitioning at the chunk grid is the natural fit: the partition exists for free (chunks already are the grid).

## When to compress

- **Per-message compression** (zstd/lz4): only worth it for messages > ~256 B. `BlockEdit` at ~15 B doesn't need it.
- **Channel-level compression** (zlib partial-flush, à la Minecraft): consider for the snapshot channel later. Lightyear may already include this — verify before adding.
- **RLE for chunk snapshots**: trivial win since most blocks are `Empty` in early chunks. Implement *when* `ChunkSnapshot` is the bandwidth bottleneck, not preemptively.

## Time dilation as graceful degradation

If bandwidth or sim-cost saturates, **slow the simulation tick** rather than dropping packets randomly or kicking players. EVE Online's pattern, mentioned by Lysenko: dilation should vary as the inverse square of the load so the game stays playable instead of disconnecting under stress.

Lightyear has tick-rate control. Defer until we see actual saturation.

## Water sim (when it lands)

Naïve cell replication will blow the bandwidth budget instantly — water cells change every tick across many positions. Strategies, in order of preference:

1. **Don't replicate** — run the same deterministic sim on every client from the same seed. Edits desync it; periodically sync. (Risky: drift accumulates.)
2. **Replicate the source/sink terms only** — water-level changes that introduce new water (rain, broken dams) are the events; the propagation is computed locally. Much smaller wire payload.
3. **Spatial AoI + delta batching** — only sync cells near players, batch all changes per tick into one unreliable message.

Likely we'll do (2) primarily, with (3) as a safety net. *Decision deferred until we start the water sim.*

## Sources

- Lysenko, *Replication in network games: Bandwidth* — <https://0fps.net/2014/03/09/replication-in-network-games-bandwidth-part-4/>
- Fiedler, *Sending Large Blocks of Data* — <https://gafferongames.com/post/sending_large_blocks_of_data/>
- Zylann, *Voxel Tools — Multiplayer* — <https://voxel-tools.readthedocs.io/en/latest/multiplayer/>
