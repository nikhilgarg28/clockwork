use crate::queue::TaskId;
use crate::scheduler::Scheduler;
use ahash::AHashMap;
use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

/// Scheduler that does FIFO in order of when tasks arrived.
pub struct ArrivalFifo {
    runnable: BTreeMap<(Instant, u64), ()>,
    arrivals: AHashMap<u64, (Instant, VecDeque<TaskId>)>,
}

impl ArrivalFifo {
    pub fn new() -> Self {
        Self {
            runnable: BTreeMap::new(),
            arrivals: AHashMap::new(),
        }
    }
}

impl Scheduler for ArrivalFifo {
    fn push(&mut self, id: TaskId, group: u64, at: Instant) {
        match self.arrivals.get_mut(&group) {
            None => {
                self.arrivals.insert(group, (at, VecDeque::from_iter([id])));
                self.runnable.insert((at, group), ());
            }
            Some(arrivals) => {
                let old_len = arrivals.1.len();
                arrivals.1.push_back(id);
                if old_len == 0 {
                    self.runnable.insert((arrivals.0, group), ());
                }
            }
        }
    }
    fn pop(&mut self) -> Option<TaskId> {
        let entry = self.runnable.first_entry();
        if entry.is_none() {
            return None;
        }
        let (_, group) = *entry.unwrap().key();
        let arrivals = self.arrivals.get_mut(&group).unwrap();
        let id = arrivals.1.pop_front().unwrap();
        if arrivals.1.is_empty() {
            self.runnable.remove(&(arrivals.0, group));
        }
        Some(id)
    }
    fn clear_task_state(&mut self, _id: TaskId, _gid: u64) {}

    fn clear_group_state(&mut self, gid: u64) {
        let (_, tasks) = self.arrivals.get_mut(&gid).expect("group should exist");
        assert!(tasks.is_empty(), "for user groups, tasks should be empty");
        self.arrivals.remove(&gid);
    }
    fn is_runnable(&self) -> bool {
        !self.runnable.is_empty()
    }
    fn observe(&mut self, _id: TaskId, _group: u64, _start: Instant, _end: Instant, _ready: bool) {
        // state depends on enqueue time, so nothing to do here
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arrival_fifo() {
        use std::time::Duration;
        let mut scheduler = ArrivalFifo::new();
        let now0 = Instant::now();
        let now1 = now0 + Duration::from_nanos(1);
        let now2 = now1 + Duration::from_nanos(2);
        scheduler.push(0, 0, now0);
        scheduler.push(1, 0, now1);
        scheduler.push(2, 1, now2);
        assert_eq!(scheduler.pop(), Some(0));
        scheduler.push(3, 0, now0 + Duration::from_nanos(5));
        assert_eq!(scheduler.pop(), Some(1));
        assert_eq!(scheduler.pop(), Some(3));
        assert_eq!(scheduler.pop(), Some(2));
        assert_eq!(scheduler.pop(), None);
        scheduler.clear_task_state(2, 1);
        scheduler.push(4, 1, now0 + Duration::from_nanos(10));
        scheduler.push(5, 0, now0 + Duration::from_nanos(15));
        // even though 5 enqueued later, it should run first because it's in group 0
        assert_eq!(scheduler.pop(), Some(5));
        assert_eq!(scheduler.pop(), Some(4));
        assert_eq!(scheduler.pop(), None);
        scheduler.clear_group_state(0);
        // now group 0 should have no advantage but group 1's arrival is now + 10
        scheduler.push(6, 0, now0 + Duration::from_nanos(20));
        scheduler.push(7, 1, now0 + Duration::from_nanos(25));
        assert_eq!(scheduler.pop(), Some(7));
        assert_eq!(scheduler.pop(), Some(6));
        assert_eq!(scheduler.pop(), None);
    }
}
