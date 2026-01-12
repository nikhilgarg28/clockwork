use std::collections::VecDeque;
use std::time::Instant;

pub type TaskId = usize;

pub trait QueueKey: Eq + Sized + Copy + Send + Sync + std::fmt::Debug + 'static {}
impl<K> QueueKey for K where K: Eq + Sized + Copy + Send + Sync + std::fmt::Debug + 'static {}

pub struct Queue<K: QueueKey> {
    id: K,
    share: u64,
    scheduler: Box<dyn Scheduler>,
}
impl<K: QueueKey> Queue<K> {
    pub fn new(id: K, share: u64, scheduler: Box<dyn Scheduler>) -> Self {
        Self {
            id,
            share,
            scheduler,
        }
    }
    pub fn id(&self) -> K {
        self.id
    }
    pub fn share(&self) -> u64 {
        self.share
    }
    pub fn scheduler(self) -> Box<dyn Scheduler> {
        self.scheduler
    }
}

/// Per-queue scheduler: chooses *which task* to run within the queue.
pub trait Scheduler {
    fn push(&mut self, id: TaskId);
    fn pop(&mut self) -> Option<TaskId>;
    fn clear_state(&mut self, _id: TaskId);
    fn is_runnable(&self) -> bool;
    fn record(&mut self, id: TaskId, start: Instant, end: Instant, ready: bool);
}

/// Default FIFO queue.
pub struct FifoQueue {
    q: VecDeque<TaskId>,
}

impl FifoQueue {
    pub fn new() -> Self {
        Self { q: VecDeque::new() }
    }
}

impl Scheduler for FifoQueue {
    fn push(&mut self, id: TaskId) {
        self.q.push_back(id);
    }

    fn pop(&mut self) -> Option<TaskId> {
        self.q.pop_front()
    }

    fn is_runnable(&self) -> bool {
        !self.q.is_empty()
    }
    // since FIFO doesn't have state, nothing to do here
    fn clear_state(&mut self, _id: TaskId) {}

    fn record(&mut self, _id: TaskId, _start: Instant, _end: Instant, _ready: bool) {
        // since FIFO doesn't have state, nothing to do here
    }
}
