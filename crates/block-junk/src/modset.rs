//! Connect-time mod-set agreement.
//!
//! The wire references registry content by slot (blocks, items,
//! recipes) or by id string (NPC kinds). A client whose registries
//! disagree with the server's desyncs *silently* — wrong meshes, wrong
//! drops, wrong recipe lists — long after the connect handshake, which
//! is strictly worse than failing loudly at the door. So the server
//! sends a [`ModSetManifest`] once on connect and the client refuses
//! the session on any disagreement (see `receive_modset_manifest` in
//! client.rs).
//!
//! Two levels of check:
//! - **Id tables** per registry, in slot order. Catch missing mods,
//!   extra mods, and load-order divergence, with precise diagnostics.
//! - **A content hash** over the serialized defs (stable FNV-1a over
//!   serde_json bytes, slot order). Catches same-ids-but-different-
//!   definitions — e.g. two versions of a mod that changed a drop
//!   count. JSON field order follows struct declaration order, so the
//!   bytes are deterministic for map-free defs. `NpcKindDef` carries a
//!   `HashMap` (nondeterministic serialization order), so NPC kinds
//!   participate only in the sorted id-list check.

use serde::Serialize;

use crate::blocks::BlockRegistry;
use crate::items::ItemRegistry;
use crate::npc_registry::NpcKindRegistry;
use crate::protocol::ModSetManifest;
use crate::recipes::RecipeRegistry;
use crate::rooms::RoomPatternRegistry;

/// Snapshot the local registries into the wire manifest. Called by the
/// server once per connect, and by the client to diff against what the
/// server sent.
pub fn local_manifest(
    blocks: &BlockRegistry,
    items: &ItemRegistry,
    recipes: &RecipeRegistry,
    npc_kinds: &NpcKindRegistry,
    rooms: &RoomPatternRegistry,
) -> ModSetManifest {
    let mut hash = FNV_OFFSET;
    for (_, def) in blocks.iter() {
        fnv1a_json(def, &mut hash);
    }
    for (_, def) in items.iter() {
        fnv1a_json(def, &mut hash);
    }
    for (_, def) in recipes.iter() {
        fnv1a_json(def, &mut hash);
    }
    // Room patterns joined the wire surface with room replication
    // (pattern ids in RoomSummary; the client resolves them locally).
    // RoomPattern is HashMap-free, so it hashes deterministically.
    for def in rooms.iter() {
        fnv1a_json(def, &mut hash);
    }
    ModSetManifest {
        blocks: blocks.iter().map(|(_, def)| def.id.clone()).collect(),
        items: items.iter().map(|(_, def)| def.id.clone()).collect(),
        recipes: recipes.iter().map(|(_, def)| def.id.0.clone()).collect(),
        npc_kinds: npc_kinds.ids_sorted(),
        room_patterns: rooms.iter().map(|p| p.id.as_str().to_owned()).collect(),
        defs_hash: hash,
    }
}

/// Compare the server's manifest against ours. Empty result = agreed.
/// Non-empty = human-readable mismatch lines, most specific first,
/// truncated so a wholly different mod set doesn't produce a wall.
pub fn diff(server: &ModSetManifest, local: &ModSetManifest) -> Vec<String> {
    const MAX_LINES: usize = 10;
    let mut lines = Vec::new();
    diff_table(
        "block",
        &id_strings(&server.blocks),
        &id_strings(&local.blocks),
        &mut lines,
    );
    diff_table(
        "item",
        &id_strings(&server.items),
        &id_strings(&local.items),
        &mut lines,
    );
    diff_table("recipe", &server.recipes, &local.recipes, &mut lines);
    diff_table("npc kind", &server.npc_kinds, &local.npc_kinds, &mut lines);
    diff_table(
        "room pattern",
        &server.room_patterns,
        &local.room_patterns,
        &mut lines,
    );
    if lines.is_empty() && server.defs_hash != local.defs_hash {
        lines.push(
            "registries list the same ids but their definitions differ — \
             the mod versions are out of sync"
                .to_string(),
        );
    }
    if lines.len() > MAX_LINES {
        let dropped = lines.len() - MAX_LINES;
        lines.truncate(MAX_LINES);
        lines.push(format!("... and {dropped} more mismatch(es)"));
    }
    lines
}

fn id_strings<T: ToString>(ids: &[T]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

/// Elementwise table compare with per-slot diagnostics.
fn diff_table(what: &str, server: &[String], local: &[String], lines: &mut Vec<String>) {
    for (i, (s, l)) in server.iter().zip(local.iter()).enumerate() {
        if s != l {
            lines.push(format!("{what} slot {i}: server has {s:?}, you have {l:?}"));
        }
    }
    if server.len() > local.len() {
        lines.push(format!(
            "server has {} {what}(s) you don't, starting at {:?}",
            server.len() - local.len(),
            server[local.len()],
        ));
    } else if local.len() > server.len() {
        lines.push(format!(
            "you have {} {what}(s) the server doesn't, starting at {:?}",
            local.len() - server.len(),
            local[server.len()],
        ));
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a folded over a def's JSON bytes. Hand-rolled because the hash
/// crosses the wire: `DefaultHasher` doesn't promise stability across
/// Rust versions, and an algorithm change would read as a false mod
/// mismatch (fails closed, but confusing).
fn fnv1a_json<T: Serialize>(value: &T, hash: &mut u64) {
    let bytes = serde_json::to_vec(value).expect("registry defs serialize infallibly");
    for b in bytes {
        *hash ^= b as u64;
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use block_junk_mod_api::items::ItemId;

    fn manifest(items: Vec<&str>, hash: u64) -> ModSetManifest {
        ModSetManifest {
            blocks: Vec::new(),
            items: items.into_iter().map(ItemId::new).collect(),
            recipes: Vec::new(),
            npc_kinds: Vec::new(),
            room_patterns: Vec::new(),
            defs_hash: hash,
        }
    }

    #[test]
    fn room_pattern_divergence_is_reported() {
        let mut server = manifest(vec!["vanilla:log"], 7);
        server.room_patterns = vec!["vanilla:bedroom".to_string()];
        let local = manifest(vec!["vanilla:log"], 7);
        let lines = diff(&server, &local);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("room pattern"), "{lines:?}");
    }

    #[test]
    fn agreement_is_empty() {
        let a = manifest(vec!["vanilla:log"], 7);
        assert!(diff(&a, &a.clone()).is_empty());
    }

    #[test]
    fn slot_divergence_names_both_sides() {
        let server = manifest(vec!["vanilla:log", "vanilla:rock"], 7);
        let local = manifest(vec!["vanilla:log", "other:rock"], 7);
        let lines = diff(&server, &local);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("vanilla:rock") && lines[0].contains("other:rock"));
    }

    #[test]
    fn missing_tail_is_counted() {
        let server = manifest(vec!["vanilla:log", "extra:one", "extra:two"], 7);
        let local = manifest(vec!["vanilla:log"], 7);
        let lines = diff(&server, &local);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("2 item(s)"), "{lines:?}");
    }

    #[test]
    fn hash_only_divergence_reports_version_skew() {
        let server = manifest(vec!["vanilla:log"], 7);
        let local = manifest(vec!["vanilla:log"], 8);
        let lines = diff(&server, &local);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("out of sync"));
    }

    #[test]
    fn fnv_is_order_sensitive_and_stable() {
        let mut a = FNV_OFFSET;
        fnv1a_json(&("x", 1), &mut a);
        let mut b = FNV_OFFSET;
        fnv1a_json(&("x", 1), &mut b);
        assert_eq!(a, b);
        let mut c = FNV_OFFSET;
        fnv1a_json(&(1, "x"), &mut c);
        assert_ne!(a, c);
    }
}
