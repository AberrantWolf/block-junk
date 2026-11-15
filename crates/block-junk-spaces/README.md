# Architectural-space reference model

This crate implements the finite spatial contract in Milestone 2 of
[the colony foundation plan](../../design/colony-foundation-rework.md). It has no
Bevy, networking, persistence, asset, or Lua dependencies. **The live game still
uses the old detector until Milestone 4 integrates this model.**

The small horizontal `detect` function remains a separate reference primitive.
Use `detect_architecture` for complete spaces, floor surfaces, threshold ownership,
connections, measurements and furnishing membership.

## Verification and inspection

From the workspace root:

```sh
cargo test -p block-junk-spaces
cargo run -p block-junk-spaces --example inspect_contract -- target/contract-panels > target/space-contract.svg
cargo run -p block-junk-spaces --example inspect_openings > target/opening-fixtures.svg
```

`fixtures::contract_cases` defines 30 cases with fixed expected areas and volumes.
The test suite checks each in four rotations and both insertion orders, then
removes and restores structural support. Additional tests exercise roof removal,
skylights, frame damage, doorway state, unknown observations, furnishing changes,
invalid profiles and crossing frames. The renderer consumes those same fixtures
and asserts their expected results. Individual SVG panels are written to the
optional directory. The original `inspect` example still renders the simple
patch oracle.

## How spaces are recognized

1. Structural occupancy and furnishing occupancy are separate. Only structural
   support produces floor surfaces; beds and decorations cannot subtract floor
   area or provide a wall, roof, jamb or lintel.
2. Recognized passage thresholds belong to their connections. Their floor and
   air cells are excluded from both adjoining spaces before measurement. A
   derived mask is used inside the finite oracle; authored geometry is unchanged.
3. Horizontal interior sections are bounded by structure or architectural
   partitions. Unknown continuation cannot establish a bounded section.
4. Sections connect vertically into architectural interiors. A visible drop
   between floor surfaces in the same interior joins their surface patches.
   This handles stepped floors and upper galleries without using walkability as
   room identity. Air above walls does not connect every courtyard to outdoors;
   removing the roof or adding a skylight preserves the hall's floor membership.
5. Outdoor surfaces and unknown cave continuations are not merged merely because
   a stair links them. Authored stair links provide explicit connections between
   spaces; doors preserve separation independently of their open/closed state.

Each space retains exact floor cells grouped by elevation. Its area is the sum
of those horizontal surfaces, including an upper gallery once. Volume counts
unique structural air cells in the interior, including furnishing occupancy but
excluding solid slabs and connection-owned passage volume. A bounded interior
has a finite volume. Open and unknown volumes remain distinct.

A low courtyard wall can bound the floor while leaving its volume open. A canopy
changes shelter coverage without supplying enclosing walls. Clearance and roof
coverage are measured per floor column. Unknown roof observations are not counted
as uncovered. `pending` reports incomplete architectural context; a known scenic
outdoor surface may still have unknown surrounding context.

Placement membership uses exact occupied cells and actual support beneath them.
It never uses bounding boxes. A placement spanning spaces can appear in multiple
membership sets; those sets are **not independent capacity counts**. Facilities
and use-slot reservations in later milestones must retain unique identities.
Connections retain threshold cells, passage volume, authored faces, adjoining
space sets, traversal state, contributing placements and structural evidence.

## Supported automatic openings

Rectangular openings and stepped arches may have any finite width and wall
thickness within the observation domain. The rule requires:

- Supported passage floor through the complete wall thickness and supported
  approach surfaces at both ends, with at least two cells of structural clearance.
- Continuous structural jambs at both sides of every thickness slice. Both jambs
  must end at exposed approach geometry; a long corridor is not split into doors
  at every cross-section.
- A connected overhead profile in each slice. Heights stay level or rise towards
  a central maximum, then stay level or fall towards the opposite jamb. A level
  rectangle is included. Symmetry is not required. Larger height steps need solid
  risers connecting the header rather than diagonal contact or floating blocks.
- Known geometry throughout the evidence. Furniture neither supplies a frame nor
  prevents recognition; it can obstruct navigation later.
- No intersecting authored partition. Authored walls, windows and portals take
  precedence. Ambiguous crossing inferred frames are both rejected.

A damaged/unframed breach reconnects the surrounding geometry. Inferred faces are
rebuilt from source geometry after every edit; never persist the prepared scene as
if its partitions were player-authored blocks. `Opening` exposes the complete
threshold, per-slice profiles, clearance, both approaches and positive evidence.
The simple patch oracle's entry-face convention is not threshold ownership in
`detect_architecture`.

Authored zero-thickness partitions have face geometry and zero threshold area.
Adjacent faces of the same partition and adjoining spaces form one connection;
separate doors between the same two spaces remain distinct. A closed stair door
keeps its connection while disabling traversal. Walls/windows cannot become
traversable stair links.

## Reference boundary and later integration

- Geometry is voxel-based: horizontal supports, full structural voxels and
  vertical authored faces. Arbitrary mesh interiors, curved/subcell ramps and
  general polygonal opening inference are outside this contract. Content can
  provide authored partitions for unusual architectural arrangements.
- `Scene` is an explicitly finite observation domain. Outside is unknown;
  open sky must be attested per column. This is not a global cave census.
- `PatchId` and `SpaceId` are deterministic rebuild keys, **not durable IDs**.
  Milestone 4 owns stable identities, split/merge lineage and persistence.
- The reference is a synchronous full rebuild for controlled fixtures. Production
  must use chunk-local caches, bounded resumable work and shared subscriptions.
- Positive opening witnesses and space dependencies are inspection evidence,
  not a complete incremental dependency index. Milestone 4 must capture every
  successful and unsuccessful geometry read, including rejected frame candidates.
- Navigation, usable capacity, service areas, capabilities, settlement activation,
  NPC policy and Lua metadata are implemented by their owning later milestones.
  Recognizing a natural cave here grants no colony functions.
