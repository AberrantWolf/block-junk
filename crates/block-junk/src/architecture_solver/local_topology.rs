//! Reusable chunk-local floor samples and horizontal air sections. Cross-chunk
//! joins are reconciled against the current immutable view, including unknown
//! neighbors; cache limits cannot turn a continuation into an outdoor boundary.
use super::*;
use std::collections::VecDeque;
const SIZE: i32 = crate::protocol::CHUNK_SIZE as i32;
type Coord = [i32; 3];
fn chunk(cell: Cell) -> Coord {
    [
        cell.0.div_euclid(SIZE),
        cell.1.div_euclid(SIZE),
        cell.2.div_euclid(SIZE),
    ]
}
#[derive(Default)]
pub struct Cache {
    pub openings: BTreeMap<Coord, Arc<openings::CachedInference>>,
    entries: BTreeMap<Coord, ([u8; 32], Arc<Local>)>,
    pub hits: u64,
    pub rebuilds: u64,
}
impl Cache {
    pub fn retain(&mut self, domain: &BTreeSet<Coord>) {
        self.entries.retain(|coord, _| domain.contains(coord));
        self.openings.retain(|coord, _| domain.contains(coord));
    }
}
#[derive(Default)]
struct Local {
    floors: BTreeSet<Cell>,
    floor_patches: BTreeMap<Cell, Cell>,
    floor_edges: Vec<(Cell, Cell)>,
    air: BTreeMap<Cell, Cell>,
    sections: BTreeMap<Cell, bool>,
    edges: Vec<(Cell, Cell)>,
    reads: BTreeSet<Cell>,
}
async fn masked(
    view: &View,
    mask: &BTreeSet<Cell>,
    cell: Cell,
    reads: &mut BTreeSet<Cell>,
) -> Option<Material> {
    reads.insert(cell);
    let mut material = view.read(cell).await?;
    if mask.contains(&cell) {
        material.occupied = true;
        material.supports = false;
    }
    Some(material)
}
async fn build(view: &View, mask: &BTreeSet<Cell>, coord: Coord) -> Local {
    let mut result = Local::default();
    for x in 0..SIZE {
        for y in 0..SIZE {
            for z in 0..SIZE {
                let cell = Cell(
                    coord[0] * SIZE + x,
                    coord[1] * SIZE + y,
                    coord[2] * SIZE + z,
                );
                if masked(view, mask, cell, &mut result.reads)
                    .await
                    .is_some_and(|m| !m.occupied)
                    && masked(view, mask, cell.offset(0, -1, 0), &mut result.reads)
                        .await
                        .is_some_and(|m| m.supports)
                {
                    result.floors.insert(cell);
                }
                if result.air.contains_key(&cell)
                    || masked(view, mask, cell, &mut result.reads)
                        .await
                        .is_none_or(|m| m.occupied)
                {
                    continue;
                }
                result.air.insert(cell, cell);
                let mut bounded = true;
                let mut queue = VecDeque::from([cell]);
                while let Some(current) = queue.pop_front() {
                    for next in current.horizontal() {
                        result.reads.extend([current, next]);
                        if view.partition(current, next).await.is_some() {
                            continue;
                        }
                        if chunk(next) != coord {
                            result.edges.push((current, next));
                            continue;
                        }
                        match masked(view, mask, next, &mut result.reads).await {
                            None => bounded = false,
                            Some(m) if m.occupied => {}
                            Some(_) => {
                                if let std::collections::btree_map::Entry::Vacant(entry) =
                                    result.air.entry(next)
                                {
                                    entry.insert(cell);
                                    queue.push_back(next);
                                }
                            }
                        }
                    }
                }
                result.sections.insert(cell, bounded);
            }
        }
    }
    for &seed in &result.floors {
        if result.floor_patches.contains_key(&seed) {
            continue;
        }
        result.floor_patches.insert(seed, seed);
        let mut queue = VecDeque::from([seed]);
        while let Some(cell) = queue.pop_front() {
            for next in cell.horizontal() {
                result.reads.extend([cell, next]);
                if view.partition(cell, next).await.is_some() {
                    continue;
                }
                if chunk(next) != coord {
                    result.floor_edges.push((cell, next));
                    continue;
                }
                if result.floors.contains(&next) && !result.floor_patches.contains_key(&next) {
                    result.floor_patches.insert(next, seed);
                    queue.push_back(next);
                }
            }
        }
    }
    result
}
fn root(parent: &BTreeMap<Cell, Cell>, mut cell: Cell) -> Cell {
    while parent[&cell] != cell {
        cell = parent[&cell];
    }
    cell
}
fn join(parent: &mut BTreeMap<Cell, Cell>, a: Cell, b: Cell) {
    let a = root(parent, a);
    let b = root(parent, b);
    parent.insert(a.max(b), a.min(b));
}
/// None selects the independent uncached implementation for finite sources.
pub async fn assemble(
    view: &View,
    mask: &BTreeSet<Cell>,
) -> Option<(BTreeMap<Cell, Cell>, BTreeMap<Cell, Cell>)> {
    let cache = view.source.topology_cache()?;
    let fingerprints = view.source.chunk_fingerprints()?;
    let mut locals = BTreeMap::new();
    for (&coord, stamp) in &fingerprints {
        let mut hash = blake3::Hasher::new();
        hash.update(stamp);
        let below = [coord[0], coord[1] - 1, coord[2]];
        hash.update(&fingerprints.get(&below).copied().unwrap_or([0; 32]));
        for cell in mask {
            view.budget.step().await;
            if chunk(*cell) == coord || chunk(*cell) == below {
                bincode::serde::encode_into_std_write(cell, &mut hash, bincode::config::standard())
                    .unwrap();
            }
        }
        for (face, partition) in view.source.partitions() {
            view.budget.step().await;
            if chunk(face.0) == coord || chunk(face.1) == coord {
                bincode::serde::encode_into_std_write(face, &mut hash, bincode::config::standard())
                    .unwrap();
                hash.update(&[match partition {
                    Partition::Wall => 0,
                    Partition::Window => 1,
                    Partition::Portal { open: false } => 2,
                    Partition::Portal { open: true } => 3,
                }]);
            }
        }
        let key = *hash.finalize().as_bytes();
        let found = {
            let mut cache = cache.lock().unwrap();
            let value = cache
                .entries
                .get(&coord)
                .filter(|(stamp, _)| *stamp == key)
                .map(|(_, local)| local.clone());
            if value.is_some() {
                cache.hits += 1;
            }
            value
        };
        let local = if let Some(local) = found {
            for &cell in &local.reads {
                view.budget.step().await;
                view.dependencies.lock().unwrap().insert(cell);
            }
            local
        } else {
            let local = Arc::new(build(view, mask, coord).await);
            {
                let mut cache = cache.lock().unwrap();
                cache.rebuilds += 1;
                cache.entries.insert(coord, (key, local.clone()));
            }
            local
        };
        locals.insert(coord, local);
    }
    let mut parent = BTreeMap::new();
    let mut unbounded = BTreeSet::new();

    for local in locals.values() {
        for (&section, &bounded) in &local.sections {
            view.budget.step().await;
            parent.insert(section, section);
            if !bounded {
                unbounded.insert(section);
            }
        }
    }
    for local in locals.values() {
        for &(a, b) in &local.edges {
            view.budget.step().await;
            let section = local.air[&a];
            if let Some(other) = locals.get(&chunk(b)).and_then(|local| local.air.get(&b)) {
                join(&mut parent, section, *other);
            } else if view.read(b).await.is_none() {
                unbounded.insert(section);
            }
        }
    }
    let unbounded: BTreeSet<_> = unbounded
        .into_iter()
        .map(|section| root(&parent, section))
        .collect();
    let mut enclosed = BTreeSet::new();
    for local in locals.values() {
        for (&cell, &section) in &local.air {
            view.budget.step().await;
            if !unbounded.contains(&root(&parent, section)) {
                enclosed.insert(cell);
            }
        }
    }
    let mut index = BTreeMap::new();
    for &seed in &enclosed {
        if index.contains_key(&seed) {
            continue;
        }
        index.insert(seed, seed);
        let mut queue = VecDeque::from([seed]);
        while let Some(cell) = queue.pop_front() {
            for next in cell
                .horizontal()
                .into_iter()
                .chain([cell.offset(0, -1, 0), cell.offset(0, 1, 0)])
            {
                view.budget.step().await;
                if !enclosed.contains(&next) || index.contains_key(&next) {
                    continue;
                }
                if cell.1 == next.1 && view.partition(cell, next).await.is_some() {
                    continue;
                }
                index.insert(next, seed);
                queue.push_back(next);
            }
        }
    }
    let mut floor_parent = BTreeMap::new();
    for local in locals.values() {
        for &patch in local.floor_patches.values() {
            view.budget.step().await;
            floor_parent.insert(patch, patch);
        }
    }
    for local in locals.values() {
        for &(a, b) in &local.floor_edges {
            view.budget.step().await;
            if let Some(other) = locals
                .get(&chunk(b))
                .and_then(|local| local.floor_patches.get(&b))
            {
                join(&mut floor_parent, local.floor_patches[&a], *other);
            }
        }
    }
    let mut patches = BTreeMap::new();
    for local in locals.values() {
        for (&cell, &patch) in &local.floor_patches {
            view.budget.step().await;
            patches.insert(cell, root(&floor_parent, patch));
        }
    }
    Some((patches, index))
}
