//! Exact floor patches, bounded horizontal air sections and multilevel assembly.
use super::*;
use std::collections::VecDeque;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Boundary {
    Closed,
    Open,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ceiling {
    At(Cell),
    Sky,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Volume {
    Bounded(u64),
    Open,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Space {
    pub floor: BTreeSet<Cell>,
    pub roofs: BTreeMap<Cell, Ceiling>,
    pub clearance: BTreeMap<Cell, Ceiling>,
    pub volume: Volume,
    pub boundary: Boundary,
    pub pending: bool,
    pub placements: BTreeSet<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Connection {
    pub ends: [BTreeSet<Cell>; 2],
    pub faces: BTreeSet<Face>,
    pub threshold: BTreeSet<Cell>,
    pub volume: BTreeSet<Cell>,
    pub evidence: BTreeSet<Cell>,
    pub traversable: bool,
    pub placements: BTreeSet<u64>,
}
#[derive(Clone, Debug, Default)]
pub struct Topology {
    /// Keys identify this rebuild only; the identity service allocates durable IDs.
    pub spaces: BTreeMap<Cell, Space>,
    pub connections: Vec<Connection>,
    pub openings: Vec<openings::Opening>,
    pub pending_openings: BTreeMap<Cell, BTreeSet<Cell>>,
    pub floor_index: BTreeMap<Cell, Cell>,
    pub dependencies: BTreeSet<Cell>,
    pub continuations: BTreeSet<[i32; 3]>,
}
async fn read(view: &View, mask: &BTreeSet<Cell>, cell: Cell) -> Option<Material> {
    let mut material = view.read(cell).await?;
    if mask.contains(&cell) {
        material.occupied = true;
        material.supports = false;
    }
    Some(material)
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
async fn floor_below(
    view: &View,
    mask: &BTreeSet<Cell>,
    floors: &BTreeMap<Cell, Cell>,
    mut cell: Cell,
) -> Option<(Cell, Cell)> {
    loop {
        if read(view, mask, cell).await?.occupied {
            return None;
        }
        if let Some(&patch) = floors.get(&cell) {
            return Some((cell, patch));
        }
        cell = cell.offset(0, -1, 0);
    }
}
async fn column(view: &View, floor: Cell) -> (Ceiling, Ceiling) {
    let (mut roof, mut clearance) = (Ceiling::Unknown, Ceiling::Unknown);
    let mut cell = floor.offset(0, 1, 0);
    loop {
        view.budget.step().await;
        if view.source.sky_above(cell) {
            if roof == Ceiling::Unknown {
                roof = Ceiling::Sky;
            }
            if clearance == Ceiling::Unknown {
                clearance = Ceiling::Sky;
            }
            break;
        }
        let Some(material) = view.read(cell).await else {
            break;
        };
        if material.shelters && roof == Ceiling::Unknown {
            roof = Ceiling::At(cell);
        }
        if material.occupied && clearance == Ceiling::Unknown {
            clearance = Ceiling::At(cell);
        }
        if roof != Ceiling::Unknown && clearance != Ceiling::Unknown {
            break;
        }
        cell = cell.offset(0, 1, 0);
    }
    (roof, clearance)
}
async fn air_sections(view: &View, mask: &BTreeSet<Cell>) -> BTreeMap<Cell, Cell> {
    let mut visited = BTreeSet::new();
    let mut enclosed = BTreeSet::new();
    for seed in view.source.cells() {
        if visited.contains(&seed) || read(view, mask, seed).await.is_none_or(|m| m.occupied) {
            continue;
        }
        visited.insert(seed);
        let mut section = BTreeSet::new();
        let mut queue = VecDeque::from([seed]);
        let mut bounded = true;
        while let Some(cell) = queue.pop_front() {
            section.insert(cell);
            for next in cell.horizontal() {
                if view.partition(cell, next).await.is_some() {
                    continue;
                }
                match read(view, mask, next).await {
                    None => bounded = false,
                    Some(m) if m.occupied => {}
                    Some(_) => {
                        if visited.insert(next) {
                            queue.push_back(next);
                        }
                    }
                }
            }
        }
        if bounded {
            for cell in section {
                view.budget.step().await;
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
    index
}

pub async fn detect(view: &View) -> Topology {
    let inference = openings::infer_tracked(view).await;
    let openings = inference.openings;
    let mut mask = BTreeSet::new();
    for opening in &openings {
        for &cell in &opening.volume {
            view.budget.step().await;
            mask.insert(cell);
        }
    }
    for portal in view.source.portals() {
        for &cell in &portal.volume {
            view.budget.step().await;
            mask.insert(cell);
        }
    }
    let local = super::local_topology::assemble(view, &mask).await;
    let mut floors = local
        .as_ref()
        .map(|(floors, _)| floors.keys().copied().collect::<BTreeSet<_>>())
        .unwrap_or_default();
    if local.is_none() {
        for cell in view.source.cells() {
            if read(view, &mask, cell).await.is_some_and(|m| !m.occupied)
                && read(view, &mask, cell.offset(0, -1, 0))
                    .await
                    .is_some_and(|m| m.supports)
            {
                floors.insert(cell);
            }
        }
    }
    let mut patches = local
        .as_ref()
        .map(|(patches, _)| patches.clone())
        .unwrap_or_default();
    let mut parent: BTreeMap<_, _> = patches
        .values()
        .copied()
        .map(|patch| (patch, patch))
        .collect();
    for &seed in &floors {
        if patches.contains_key(&seed) {
            continue;
        }
        patches.insert(seed, seed);
        parent.insert(seed, seed);
        let mut queue = VecDeque::from([seed]);
        while let Some(cell) = queue.pop_front() {
            for next in cell.horizontal() {
                view.budget.step().await;
                if floors.contains(&next)
                    && !patches.contains_key(&next)
                    && view.partition(cell, next).await.is_none()
                {
                    patches.insert(next, seed);
                    queue.push_back(next);
                }
            }
        }
    }
    let air = match local {
        Some((_, air)) => air,
        None => air_sections(view, &mask).await,
    };
    for (&upper, &patch) in &patches {
        let Some(region) = air.get(&upper) else {
            continue;
        };
        for next in upper.horizontal() {
            if view.partition(upper, next).await.is_some() {
                continue;
            }
            if let Some((lower, other)) = floor_below(view, &mask, &patches, next).await
                && lower.1 < upper.1
                && air.get(&lower) == Some(region)
            {
                join(&mut parent, patch, other);
            }
        }
    }
    let mut result = Topology {
        openings,
        pending_openings: inference.pending,
        ..Topology::default()
    };
    for (&cell, &patch) in &patches {
        view.budget.step().await;
        let key = root(&parent, patch);
        let (roof, clearance) = column(view, cell).await;
        let space = result.spaces.entry(key).or_insert_with(|| Space {
            floor: BTreeSet::new(),
            roofs: BTreeMap::new(),
            clearance: BTreeMap::new(),
            volume: Volume::Unknown,
            boundary: Boundary::Closed,
            pending: false,
            placements: BTreeSet::new(),
        });
        space.floor.insert(cell);
        space.roofs.insert(cell, roof);
        space.clearance.insert(cell, clearance);
        result.floor_index.insert(cell, key);
    }
    for space in result.spaces.values_mut() {
        let mut interior = BTreeSet::new();
        let (mut unknown, mut open) = (false, false);
        for (&floor, &ceiling) in &space.clearance {
            match ceiling {
                Ceiling::Unknown => unknown = true,
                Ceiling::Sky => open = true,
                Ceiling::At(top) => {
                    for y in floor.1..top.1 {
                        view.budget.step().await;
                        interior.insert(Cell(floor.0, y, floor.2));
                    }
                }
            }
        }
        for &cell in &space.floor {
            for next in cell.horizontal() {
                if view.partition(cell, next).await.is_some()
                    || read(view, &mask, next).await.is_some_and(|m| m.occupied)
                {
                    continue;
                }
                if floor_below(view, &mask, &patches, next)
                    .await
                    .is_some_and(|(floor, _)| space.floor.contains(&floor))
                {
                    continue;
                }
                space.boundary = match read(view, &mask, next).await {
                    None => Boundary::Unknown,
                    _ if space.boundary == Boundary::Unknown => Boundary::Unknown,
                    _ => Boundary::Open,
                };
            }
        }
        for &cell in &interior {
            for next in cell.horizontal() {
                if interior.contains(&next) || view.partition(cell, next).await.is_some() {
                    continue;
                }
                match read(view, &mask, next).await {
                    None => unknown = true,
                    Some(m) if !m.occupied => open = true,
                    _ => {}
                }
            }
        }
        let context_unknown = space.floor.iter().any(|cell| !air.contains_key(cell));
        space.pending = space.boundary == Boundary::Unknown
            || space.roofs.values().any(|roof| *roof == Ceiling::Unknown)
            || context_unknown;
        if context_unknown && !space.roofs.values().any(|roof| *roof == Ceiling::Sky) {
            unknown = true;
        }
        space.volume = if unknown {
            Volume::Unknown
        } else if open {
            Volume::Open
        } else {
            Volume::Bounded(interior.len() as u64)
        };
    }
    for placement in view.source.placements() {
        for &cell in &placement.cells {
            if let Some((floor, _)) = floor_below(view, &mask, &patches, cell).await
                && let Some(key) = result.floor_index.get(&floor)
            {
                result
                    .spaces
                    .get_mut(key)
                    .unwrap()
                    .placements
                    .insert(placement.id);
            }
        }
    }
    for opening in &result.openings {
        let mut ends = [BTreeSet::new(), BTreeSet::new()];
        for (side, end) in ends.iter_mut().enumerate() {
            for cell in &opening.approaches[side] {
                view.budget.step().await;
                if let Some(key) = result.floor_index.get(cell) {
                    end.insert(*key);
                }
            }
        }
        let mut placements = BTreeSet::new();
        for placement in view.source.placements() {
            for cell in &placement.cells {
                view.budget.step().await;
                if opening.volume.contains(cell) {
                    placements.insert(placement.id);
                }
            }
        }
        result.connections.push(Connection {
            ends,
            faces: opening.faces.clone(),
            threshold: opening.floor.clone(),
            volume: opening.volume.clone(),
            evidence: opening.evidence.clone(),
            traversable: true,
            placements,
        });
    }
    authored_connections(view, &mask, &patches, &mut result).await;
    result.dependencies = std::mem::take(&mut *view.dependencies.lock().unwrap());
    refresh_continuations(view, &mut result).await;
    result
}

pub async fn refresh_continuations(view: &View, result: &mut Topology) {
    result.continuations.clear();
    let size = crate::protocol::CHUNK_SIZE as i32;
    for (seed, unknown) in &result.pending_openings {
        if !result.floor_index.contains_key(seed) {
            continue;
        }
        for cell in unknown {
            view.budget.step().await;
            result.continuations.insert([
                cell.0.div_euclid(size),
                cell.1.div_euclid(size),
                cell.2.div_euclid(size),
            ]);
        }
    }
    // Continue only architectural queries associated with incomplete spaces.
    // Known sky-facing outdoor surfaces do not demand an unbounded world census.
    let mut columns = BTreeMap::<(i32, i32), i32>::new();
    for space in result.spaces.values() {
        if !space.pending || space.roofs.values().all(|roof| *roof == Ceiling::Sky) {
            continue;
        }
        let min_y = space.floor.iter().map(|cell| cell.1).min().unwrap() - 1;
        for cell in &space.floor {
            for at in std::iter::once(*cell).chain(cell.horizontal()) {
                view.budget.step().await;
                columns
                    .entry((at.0, at.2))
                    .and_modify(|y| *y = (*y).min(min_y))
                    .or_insert(min_y);
            }
        }
    }
    for &cell in &result.dependencies {
        view.budget.step().await;
        if columns
            .get(&(cell.0, cell.2))
            .is_some_and(|min_y| cell.1 >= *min_y)
            && view.source.material(cell).is_none()
            && !view.source.sky_above(cell)
        {
            let size = crate::protocol::CHUNK_SIZE as i32;
            result.continuations.insert([
                cell.0.div_euclid(size),
                cell.1.div_euclid(size),
                cell.2.div_euclid(size),
            ]);
        }
    }
}

async fn authored_connections(
    view: &View,
    mask: &BTreeSet<Cell>,
    patches: &BTreeMap<Cell, Cell>,
    result: &mut Topology,
) {
    // Zero-thickness faces group only when coplanar, adjacent and sharing endpoints.
    let mut remaining = BTreeMap::new();
    for (&face, &partition) in view.source.partitions() {
        let (Some((a, _)), Some((b, _))) = (
            floor_below(view, mask, patches, face.0).await,
            floor_below(view, mask, patches, face.1).await,
        ) else {
            continue;
        };
        let (a, b) = (result.floor_index[&a], result.floor_index[&b]);
        if a != b {
            remaining.insert(face, (partition, a, b));
        }
    }
    while let Some((&seed, &(partition, a, b))) = remaining.first_key_value() {
        remaining.remove(&seed);
        let mut faces = BTreeSet::from([seed]);
        let mut queue = VecDeque::from([seed]);
        while let Some(face) = queue.pop_front() {
            let normal_x = face.0.0 != face.1.0;
            let offsets = if normal_x {
                [(0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)]
            } else {
                [(0, 1, 0), (0, -1, 0), (1, 0, 0), (-1, 0, 0)]
            };
            for (x, y, z) in offsets {
                view.budget.step().await;
                let next = Face::new(face.0.offset(x, y, z), face.1.offset(x, y, z));
                if remaining.get(&next) == Some(&(partition, a, b)) {
                    remaining.remove(&next);
                    faces.insert(next);
                    queue.push_back(next);
                }
            }
        }
        let evidence = faces.iter().flat_map(|face| [face.0, face.1]).collect();
        result.connections.push(Connection {
            ends: [BTreeSet::from([a]), BTreeSet::from([b])],
            faces,
            threshold: BTreeSet::new(),
            volume: BTreeSet::new(),
            evidence,
            traversable: matches!(partition, Partition::Portal { open: true }),
            placements: BTreeSet::new(),
        });
    }
    for portal in view.source.portals() {
        if portal.volume.is_empty() {
            continue;
        }
        let mut ends = [BTreeSet::new(), BTreeSet::new()];
        let normal_x = portal
            .faces
            .first()
            .is_some_and(|face| face.0.0 != face.1.0);
        let axis = if normal_x { (1, 0, 0) } else { (0, 0, 1) };
        let mut evidence = portal.volume.clone();
        for &threshold in &portal.threshold {
            for (side, sign) in [(0, -1), (1, 1)] {
                let outside = threshold.offset(axis.0 * sign, 0, axis.2 * sign);
                if portal.volume.contains(&outside) {
                    continue;
                }
                evidence.insert(outside);
                if let Some((floor, _)) = floor_below(view, mask, patches, outside).await
                    && let Some(key) = result.floor_index.get(&floor)
                {
                    ends[side].insert(*key);
                }
            }
        }
        let mut placements = BTreeSet::new();
        for placement in view.source.placements() {
            for cell in &placement.cells {
                view.budget.step().await;
                if portal.volume.contains(cell) {
                    placements.insert(placement.id);
                }
            }
        }
        result.connections.push(Connection {
            ends,
            faces: portal.faces.clone(),
            threshold: portal.threshold.clone(),
            volume: portal.volume.clone(),
            evidence,
            traversable: portal.open,
            placements,
        });
    }
    for &(a, b) in view.source.stairs() {
        let (Some(&ka), Some(&kb)) = (result.floor_index.get(&a), result.floor_index.get(&b))
        else {
            continue;
        };
        if ka == kb {
            continue;
        }
        let (low, high) = if a.1 < b.1 { (a, b) } else { (b, a) };
        let mut valid = true;
        for cell in [low, high] {
            valid &= view
                .read(cell.offset(0, -1, 0))
                .await
                .is_some_and(|m| m.supports);
            for dy in 0..2 {
                valid &= view
                    .read(cell.offset(0, dy, 0))
                    .await
                    .is_some_and(|m| !m.occupied);
            }
        }
        valid &= view
            .read(low.offset(0, 2, 0))
            .await
            .is_some_and(|m| !m.occupied);
        if !valid {
            continue;
        }
        let mut faces = BTreeSet::new();
        let mut traversable = true;
        for dy in 0..2 {
            let face = Face::new(low.offset(0, dy + 1, 0), high.offset(0, dy, 0));
            if let Some(partition) = view.partition(face.0, face.1).await {
                traversable &= matches!(partition, Partition::Portal { open: true });
            }
            faces.insert(face);
        }
        result.connections.push(Connection {
            ends: [BTreeSet::from([ka]), BTreeSet::from([kb])],
            faces,
            threshold: BTreeSet::new(),
            volume: BTreeSet::new(),
            evidence: BTreeSet::from([a, b]),
            traversable,
            placements: BTreeSet::new(),
        });
    }
}
