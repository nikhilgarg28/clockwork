mod arrival_fifo;
mod fsrpt;
mod las;
mod qlas;
mod qsrpt;
mod runnable_fifo;
pub use arrival_fifo::ArrivalFifo;
pub use fsrpt::FairSRPT;
pub use las::LAS;
pub use qlas::QLAS;
pub use qsrpt::QSRPT;
pub use runnable_fifo::RunnableFifo;

use crate::mpsc::Mpsc;
use crate::queue::TaskId;
use std::sync::Arc;
use std::time::Instant;

/// Per-queue scheduler: chooses *which task* to run within the queue.
pub trait Scheduler {
    /// Initialize scheduler with its queue's mpsc channel.
    /// The scheduler can use this to drain tasks directly from the mpsc in pop().
    fn init(&mut self, mpsc: Arc<Mpsc<TaskId>>);
    
    fn push(&mut self, id: TaskId, group: u64, at: Instant);
    fn pop(&mut self) -> Option<TaskId>;
    fn clear_task_state(&mut self, id: TaskId, group: u64);
    fn clear_group_state(&mut self, group: u64);
    fn is_runnable(&self) -> bool;
    fn observe(&mut self, id: TaskId, group: u64, start: Instant, end: Instant, ready: bool);
}
