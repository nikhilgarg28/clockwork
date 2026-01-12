use crate::queue::TaskId;
use crate::scheduler::Scheduler;
use std::collections::VecDeque;
use std::time::Instant;

/// Scheduler that does FIFO in order of when tasks become runnable.
/// This scheduler is completely stateless and has negligible overhead.
pub struct RunnableFifo {
    q: VecDeque<TaskId>,
}

impl RunnableFifo {
    pub fn new() -> Self {
        Self { q: VecDeque::new() }
    }
}

impl Scheduler for RunnableFifo {
    fn push(&mut self, id: TaskId, _group: u64, _at: Instant) {
        self.q.push_back(id);
    }

    fn pop(&mut self) -> Option<TaskId> {
        self.q.pop_front()
    }

    fn is_runnable(&self) -> bool {
        !self.q.is_empty()
    }
    // since FIFO doesn't have state, nothing to do here
    fn clear_task_state(&mut self, _id: TaskId, _group: u64) {}
    fn clear_group_state(&mut self, _group: u64) {}
    fn observe(&mut self, _id: TaskId, _group: u64, _start: Instant, _end: Instant, _ready: bool) {
        // since FIFO doesn't have state, nothing to do here
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runnable_fifo() {
        let mut scheduler = RunnableFifo::new();
        let now = Instant::now();
        scheduler.push(0, 0, now);
        scheduler.push(1, 0, now);
        scheduler.push(2, 4, now);
        assert_eq!(scheduler.pop(), Some(0));
        assert_eq!(scheduler.pop(), Some(1));
        scheduler.push(1, 0, now);
        assert_eq!(scheduler.pop(), Some(2));
        assert_eq!(scheduler.pop(), Some(1));
        assert_eq!(scheduler.pop(), None);
    }
}
