use crate::scheduler::Scheduler;
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
