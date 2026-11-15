//! Cooperative architectural computation over an immutable authoritative view.
//! Every read yields when its deterministic budget is exhausted. Unknown input
//! remains unknown; time slicing never changes architectural membership rules.
#[cfg(test)]
mod fixture_source;
#[cfg(test)]
mod live_fixtures;
pub mod live_source;
pub mod local_topology;
pub mod openings;
pub mod topology;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::{Future, poll_fn},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Cell(pub i32, pub i32, pub i32);
impl Cell {
    pub fn offset(self, x: i32, y: i32, z: i32) -> Self {
        Self(self.0 + x, self.1 + y, self.2 + z)
    }
    pub fn horizontal(self) -> [Self; 4] {
        [
            self.offset(-1, 0, 0),
            self.offset(1, 0, 0),
            self.offset(0, 0, -1),
            self.offset(0, 0, 1),
        ]
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Material {
    pub occupied: bool,
    pub supports: bool,
    pub shelters: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Face(pub Cell, pub Cell);
impl Face {
    pub fn new(a: Cell, b: Cell) -> Self {
        Self(a.min(b), a.max(b))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Partition {
    Wall,
    Window,
    Portal { open: bool },
}
#[derive(Clone, Debug)]
pub struct Placement {
    pub id: u64,
    pub cells: BTreeSet<Cell>,
}
#[derive(Clone, Debug)]
pub struct AuthoredPortal {
    pub faces: BTreeSet<Face>,
    pub threshold: BTreeSet<Cell>,
    pub volume: BTreeSet<Cell>,
    pub open: bool,
}
/// Sources supply only demanded regions and their discovered dependencies.
/// `sky_above` requires proof from generation/authoritative column summaries.
pub trait GeometrySource: Send + Sync {
    fn topology_cache(&self) -> Option<&Arc<Mutex<local_topology::Cache>>> {
        None
    }
    fn chunk_fingerprints(&self) -> Option<BTreeMap<[i32; 3], [u8; 32]>> {
        None
    }
    fn cells(&self) -> Box<dyn Iterator<Item = Cell> + Send + '_>;
    fn material(&self, cell: Cell) -> Option<Material>;
    fn sky_above(&self, cell: Cell) -> bool;
    fn partitions(&self) -> &BTreeMap<Face, Partition>;
    fn portals(&self) -> &[AuthoredPortal];
    fn placements(&self) -> &[Placement];
    fn stairs(&self) -> &[(Cell, Cell)];
}
#[derive(Default)]
pub struct Budget {
    remaining: AtomicUsize,
}
impl Budget {
    pub async fn step(&self) {
        poll_fn(|_| {
            match self
                .remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            {
                Ok(_) => Poll::Ready(()),
                Err(_) => Poll::Pending,
            }
        })
        .await
    }
}
#[derive(Clone)]
pub struct View {
    pub source: Arc<dyn GeometrySource>,
    pub budget: Arc<Budget>,
    pub dependencies: Arc<Mutex<BTreeSet<Cell>>>,
}
impl View {
    pub async fn read(&self, cell: Cell) -> Option<Material> {
        self.budget.step().await;
        self.dependencies.lock().unwrap().insert(cell);
        self.source.material(cell)
    }
    pub async fn partition(&self, a: Cell, b: Cell) -> Option<Partition> {
        self.budget.step().await;
        self.dependencies.lock().unwrap().extend([a, b]);
        self.source.partitions().get(&Face::new(a, b)).copied()
    }
}
/// The owning service polls once per fair queue turn. Pending futures are never
/// blocked on a sleeping worker thread, and no wall-clock timeout fabricates data.
pub struct Computation<T> {
    budget: Arc<Budget>,
    future: Pin<Box<dyn Future<Output = T> + Send>>,
    pub operations: usize,
}
impl<T> Computation<T> {
    pub fn new(budget: Arc<Budget>, future: impl Future<Output = T> + Send + 'static) -> Self {
        Self {
            budget,
            future: Box::pin(future),
            operations: 0,
        }
    }
    pub fn advance(&mut self, operations: usize) -> Poll<T> {
        self.budget.remaining.store(operations, Ordering::Relaxed);
        let result = self
            .future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        self.operations += operations - self.budget.remaining.load(Ordering::Relaxed);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cooperative_reads_resume_exactly_at_operation_boundaries() {
        let budget = Arc::new(Budget::default());
        let task_budget = budget.clone();
        let mut task = Computation::new(budget, async move {
            let mut value = 0;
            for n in 0..100 {
                task_budget.step().await;
                value += n;
            }
            value
        });
        assert!(task.advance(0).is_pending());
        for _ in 0..9 {
            assert!(task.advance(10).is_pending());
        }
        assert_eq!(task.advance(10), Poll::Ready(4950));
        assert_eq!(task.operations, 100);
    }
}
