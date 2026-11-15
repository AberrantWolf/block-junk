//! Resumable frame recognition; failed probes remain in the view's dependency set.
use super::*;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opening {
    pub floor: BTreeSet<Cell>,
    pub volume: BTreeSet<Cell>,
    pub approaches: [BTreeSet<Cell>; 2],
    pub faces: BTreeSet<Face>,
    pub profiles: Vec<Vec<i32>>,
    pub evidence: BTreeSet<Cell>,
}
fn tangent(c: Cell, x: bool, n: i32) -> Cell {
    if x {
        c.offset(0, 0, n)
    } else {
        c.offset(n, 0, 0)
    }
}
fn normal(c: Cell, x: bool, n: i32) -> Cell {
    if x {
        c.offset(n, 0, 0)
    } else {
        c.offset(0, 0, n)
    }
}
async fn probe(view: &View, cell: Cell, evidence: &mut BTreeSet<Cell>) -> Option<Material> {
    evidence.insert(cell);
    view.read(cell).await
}
async fn candidate(
    view: &View,
    seed: Cell,
    axis_x: bool,
    evidence: &mut BTreeSet<Cell>,
) -> Option<Opening> {
    if probe(view, seed, evidence).await?.occupied
        || !probe(view, tangent(seed, axis_x, -1), evidence)
            .await?
            .occupied
    {
        return None;
    }
    // Reject an already-disqualified near jamb before scanning a potentially
    // enormous hall for the opposite jamb. This is the same frame condition
    // checked below, ordered to keep long cave walls cheap.
    if probe(
        view,
        normal(tangent(seed, axis_x, -1), axis_x, -1),
        evidence,
    )
    .await?
    .occupied
    {
        return None;
    }
    let mut width = 0;
    while !probe(view, tangent(seed, axis_x, width), evidence)
        .await?
        .occupied
    {
        width += 1;
    }
    if width == 0 {
        return None;
    }
    let jambs = [tangent(seed, axis_x, -1), tangent(seed, axis_x, width)];
    for jamb in jambs {
        if !probe(view, jamb, evidence).await?.occupied
            || probe(view, normal(jamb, axis_x, -1), evidence)
                .await?
                .occupied
        {
            return None;
        }
    }
    let mut depth = 0;
    loop {
        let a = probe(view, normal(jambs[0], axis_x, depth), evidence)
            .await?
            .occupied;
        let b = probe(view, normal(jambs[1], axis_x, depth), evidence)
            .await?
            .occupied;
        match (a, b) {
            (true, true) => depth += 1,
            (false, false) => break,
            _ => return None,
        }
    }
    if depth == 0 {
        return None;
    }
    let mut result = Opening {
        floor: BTreeSet::new(),
        volume: BTreeSet::new(),
        approaches: [BTreeSet::new(), BTreeSet::new()],
        faces: BTreeSet::new(),
        profiles: Vec::new(),
        evidence: BTreeSet::new(),
    };
    for slice in 0..depth {
        let mut profile = Vec::new();
        for offset in 0..width {
            let floor = normal(tangent(seed, axis_x, offset), axis_x, slice);
            if !probe(view, floor.offset(0, -1, 0), evidence)
                .await?
                .supports
            {
                return None;
            }
            let mut height = 0;
            while !probe(view, floor.offset(0, height, 0), evidence)
                .await?
                .occupied
            {
                result.volume.insert(floor.offset(0, height, 0));
                height += 1;
            }
            if height < 2 {
                return None;
            }
            profile.push(height);
            result.floor.insert(floor);
        }
        let maximum = *profile.iter().max()?;
        if profile[(profile.len() - 1) / 2] != maximum && profile[profile.len() / 2] != maximum {
            return None;
        }
        let mut falling = false;
        for pair in profile.windows(2) {
            view.budget.step().await;
            if pair[1] < pair[0] {
                falling = true;
            } else if falling && pair[1] > pair[0] {
                return None;
            }
        }
        for (jamb, height) in [(jambs[0], profile[0]), (jambs[1], *profile.last()?)] {
            for dy in 0..=height {
                if !probe(view, normal(jamb, axis_x, slice).offset(0, dy, 0), evidence)
                    .await?
                    .occupied
                {
                    return None;
                }
            }
        }
        for offset in 0..width - 1 {
            let a = profile[offset as usize];
            let b = profile[offset as usize + 1];
            let lower = offset + i32::from(b < a);
            for dy in a.min(b)..=a.max(b) {
                if !probe(
                    view,
                    normal(tangent(seed, axis_x, lower), axis_x, slice).offset(0, dy, 0),
                    evidence,
                )
                .await?
                .occupied
                {
                    return None;
                }
            }
        }
        result.profiles.push(profile);
    }
    for offset in 0..width {
        for (side, slice) in [(0, -1), (1, depth)] {
            let floor = normal(tangent(seed, axis_x, offset), axis_x, slice);
            if !probe(view, floor.offset(0, -1, 0), evidence)
                .await?
                .supports
            {
                return None;
            }
            for dy in 0..2 {
                if probe(view, floor.offset(0, dy, 0), evidence)
                    .await?
                    .occupied
                {
                    return None;
                }
            }
            result.approaches[side].insert(floor);
        }
    }
    for face in view.source.partitions().keys() {
        view.budget.step().await;
        if result.volume.contains(&face.0) || result.volume.contains(&face.1) {
            return None;
        }
    }
    for portal in view.source.portals() {
        for cell in &portal.volume {
            view.budget.step().await;
            if result.volume.contains(cell) {
                return None;
            }
        }
    }
    for offset in 0..width {
        let floor = tangent(seed, axis_x, offset);
        for dy in 0..result.profiles[0][offset as usize] {
            let b = floor.offset(0, dy, 0);
            let a = normal(b, axis_x, -1);
            if probe(view, a, evidence).await?.occupied {
                return None;
            }
            result.faces.insert(Face::new(a, b));
        }
    }
    result.evidence = std::mem::take(evidence);
    Some(result)
}
#[cfg(test)]
pub async fn infer(view: &View) -> Vec<Opening> {
    infer_tracked(view).await.openings
}
#[derive(Clone, Default)]
pub struct Inference {
    pub openings: Vec<Opening>,
    pub pending: BTreeMap<Cell, BTreeSet<Cell>>,
}
async fn infer_raw<'a>(
    view: &'a View,
    cells: Box<dyn Iterator<Item = Cell> + Send + 'a>,
) -> Inference {
    let mut pending = BTreeMap::<Cell, BTreeSet<Cell>>::new();
    let mut candidates = Vec::new();
    for seed in cells {
        for axis in [true, false] {
            let mut evidence = BTreeSet::new();
            if let Some(opening) = candidate(view, seed, axis, &mut evidence).await {
                candidates.push(opening);
            } else {
                let mut unknown = BTreeSet::new();
                for cell in evidence {
                    view.budget.step().await;
                    if view.source.material(cell).is_none() && !view.source.sky_above(cell) {
                        unknown.insert(cell);
                    }
                }
                if !unknown.is_empty()
                    && view.read(seed).await.is_some_and(|m| !m.occupied)
                    && view
                        .read(tangent(seed, axis, -1))
                        .await
                        .is_some_and(|m| m.occupied)
                    && view
                        .read(seed.offset(0, -1, 0))
                        .await
                        .is_some_and(|m| m.supports)
                {
                    pending.entry(seed).or_default().extend(unknown);
                }
            }
        }
    }
    Inference {
        openings: candidates,
        pending,
    }
}
pub struct CachedInference {
    dependencies: BTreeMap<[i32; 3], Option<[u8; 32]>>,
    reads: BTreeSet<Cell>,
    result: Inference,
}
pub async fn infer_tracked(view: &View) -> Inference {
    let mut result = if let (Some(cache), Some(fingerprints)) = (
        view.source.topology_cache(),
        view.source.chunk_fingerprints(),
    ) {
        let size = crate::protocol::CHUNK_SIZE as i32;
        let mut result = Inference::default();
        for &coord in fingerprints.keys() {
            let found = {
                let cache = cache.lock().unwrap();
                cache
                    .openings
                    .get(&coord)
                    .filter(|entry| {
                        entry
                            .dependencies
                            .iter()
                            .all(|(coord, stamp)| fingerprints.get(coord).copied() == *stamp)
                    })
                    .cloned()
            };
            let entry = if let Some(entry) = found {
                for &cell in &entry.reads {
                    view.budget.step().await;
                    view.dependencies.lock().unwrap().insert(cell);
                }
                entry
            } else {
                let child = View {
                    source: view.source.clone(),
                    budget: view.budget.clone(),
                    dependencies: Arc::default(),
                };
                let cells = Box::new((0..size).flat_map(move |x| {
                    (0..size).flat_map(move |y| {
                        (0..size).map(move |z| {
                            Cell(
                                coord[0] * size + x,
                                coord[1] * size + y,
                                coord[2] * size + z,
                            )
                        })
                    })
                }));
                let result = infer_raw(&child, cells).await;
                let reads = std::mem::take(&mut *child.dependencies.lock().unwrap());
                let mut dependencies = BTreeMap::new();
                for &cell in &reads {
                    view.budget.step().await;
                    let coord = [
                        cell.0.div_euclid(size),
                        cell.1.div_euclid(size),
                        cell.2.div_euclid(size),
                    ];
                    dependencies.insert(coord, fingerprints.get(&coord).copied());
                    view.dependencies.lock().unwrap().insert(cell);
                }
                let entry = Arc::new(CachedInference {
                    dependencies,
                    reads,
                    result,
                });
                cache.lock().unwrap().openings.insert(coord, entry.clone());
                entry
            };
            result
                .openings
                .extend(entry.result.openings.iter().cloned());
            for (seed, cells) in &entry.result.pending {
                view.budget.step().await;
                result.pending.entry(*seed).or_default().extend(cells);
            }
        }
        result
    } else {
        infer_raw(view, view.source.cells()).await
    };
    let candidates = std::mem::take(&mut result.openings);
    let mut owners = BTreeMap::<Cell, usize>::new();
    let mut ambiguous = BTreeSet::new();
    for (index, opening) in candidates.iter().enumerate() {
        for &cell in &opening.volume {
            view.budget.step().await;
            if let Some(previous) = owners.insert(cell, index) {
                ambiguous.extend([previous, index]);
            }
        }
    }
    let openings = candidates
        .into_iter()
        .enumerate()
        .filter_map(|(index, opening)| (!ambiguous.contains(&index)).then_some(opening))
        .collect();
    result.openings = openings;
    result
}
