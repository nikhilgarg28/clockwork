use crate::mpsc::Mpsc;
use crate::queue::TaskId;
use crate::scheduler::Scheduler;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

/// Scheduler that does FIFO in order of when tasks become runnable.
/// This scheduler is completely stateless and has negligible overhead.
/// When initialized with an mpsc, it drains directly from the mpsc in pop().
pub struct RunnableFifo {
    mpsc: Option<Arc<Mpsc<TaskId>>>,
    q: VecDeque<TaskId>,
}

impl RunnableFifo {
    pub fn new() -> Self {
        Self {
            mpsc: None,
            q: VecDeque::with_capacity(1024),
        }
    }
}

impl Scheduler for RunnableFifo {
    fn init(&mut self, mpsc: Arc<Mpsc<TaskId>>) {
        self.mpsc = Some(mpsc);
    }

    #[inline]
    fn push(&mut self, id: TaskId, _group: u64, _at: Instant) {
        // If mpsc is set, tasks are enqueued directly to mpsc by wakers
        // This push() should not be called anymore, but keep for compatibility
        self.q.push_back(id);
    }

    #[inline]
    fn pop(&mut self) -> Option<TaskId> {
        // First, drain from mpsc if available
        match &self.mpsc {
            Some(mpsc) if !mpsc.is_empty() => {
                mpsc.pop()
            }
            _ => self.q.pop_front(),
        }
    }

    #[inline]
    fn is_runnable(&self) -> bool {
        // Check if VecDeque has items
        if !self.q.is_empty() {
            return true;
        }
        // Check if mpsc might have items (non-blocking hint)
        // pop() will handle the actual draining
        if let Some(mpsc) = &self.mpsc {
            return !mpsc.is_empty();
        }
        false
    }
    // since FIFO doesn't have state, nothing to do here
    fn clear_task_state(&mut self, _id: TaskId, _group: u64) {}
    fn clear_group_state(&mut self, _group: u64) {}
    fn observe(&mut self, _id: TaskId, _group: u64, _start: Instant, _end: Instant, _ready: bool) {}
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
