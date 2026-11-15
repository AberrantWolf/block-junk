# Generated capability rule reference

This file is generated from the final validated content set.

## Spacious sleeping (`area_comfort:sleeping`)

Scope: architectural space.

Requirements: none.

Capacity: `Minimum(usable sleep slots, Floor(Divide(structural floor area, 32)))`

## crafting (`foundation:crafting`)

Scope: architectural space.

Requirements:

- `facility`: At least one accessible crafting place — `usable crafting slots AtLeast 1`

Capacity: `usable crafting slots`

Attributes:

- `available`: `available crafting slots`
- `installed`: `installed crafting slots`
- `usable`: `usable crafting slots`

## dining (`foundation:dining`)

Scope: architectural space.

Requirements:

- `facility`: At least one accessible dining place — `usable dining slots beside foundation:table AtLeast 1`

Capacity: `usable dining slots beside foundation:table`

Attributes:

- `available`: `available dining slots beside foundation:table`
- `installed`: `installed dining slots beside foundation:table`
- `usable`: `usable dining slots beside foundation:table`

## sleeping (`foundation:sleeping`)

Scope: architectural space.

Requirements:

- `facility`: At least one accessible sleeping place — `usable sleep slots AtLeast 1`

Capacity: `usable sleep slots`

Attributes:

- `available`: `available sleep slots`
- `installed`: `installed sleep slots`
- `usable`: `usable sleep slots`

## storage (`foundation:storage`)

Scope: architectural space.

Requirements:

- `facility`: At least one accessible storage place — `usable storage slots AtLeast 1`

Capacity: `usable storage slots`

Attributes:

- `available`: `available storage slots`
- `installed`: `installed storage slots`
- `usable`: `usable storage slots`

## Scenic rest (`scenic_site:rest`)

Scope: outdoor service area anchored at `scenic_rest`, range 4, portal traversal false.

Requirements:

- `view`: Reach a scenic interest marker within four surface steps — `environmental attribute scenic_interest AtLeast 1`

Capacity: `usable scenic_rest slots`

Attributes:

- `available`: `available scenic_rest slots`
- `installed`: `installed scenic_rest slots`
