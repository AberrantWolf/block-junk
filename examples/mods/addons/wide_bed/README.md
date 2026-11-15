# Compatible furnishing upgrade

Load this add-on alongside `examples/mods/independent/foundation`, without vanilla.
It expands the bed to two cells and renames its use slot from `primary` to
`sleeping`. The declaration in `data.lua` authorizes only the exact before/after
structural contracts of this example.

Use a mods directory containing those two mods, then check an existing independent
world with:

```sh
cargo run -p block-junk -- validate-mods --mods-dir <mods-directory> --preset foundation:plain --save <save-name> --json
```

Validation does not write the save. Starting the session with the same content
stages the same upgrade before restoring state or running hooks. A blocked bed or
construction footprint rejects the upgrade; clear its future cell in the original
world first. Placement IDs, delivered materials and saved interaction progress are
preserved. A later normal save writes the upgraded generation.

The JSON validator reports structural contract digests for authoring upgrades.
Keep the source digest from the old release; the destination must match the final
patched definition. When an unrelated content edit changes that destination, the
validator rejects this declaration instead of silently broadening its authority.
