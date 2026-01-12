use crate::{queue::TaskId, scheduler::Scheduler};
use ahash::AHashMap;
use std::{
    collections::{BTreeMap, VecDeque},
    time::Instant,
};

/// Schedules that picks tasks on the basis of lease accessed service time.
/// For each group, stores (total service time, list of tasks within the group).
pub struct LAS {
    // stores (service time, group id) only for groups that have runnable tasks
    runnable: BTreeMap<(u128, u64), ()>,
    // maps group id to group info
    tasks: AHashMap<u64, Group>,
}

#[derive(Debug)]
struct Group {
    total_service_time: u128,
    tasks: VecDeque<TaskId>,
}
impl LAS {
    pub fn new() -> Self {
        Self {
            runnable: BTreeMap::new(),
            tasks: AHashMap::new(),
        }
    }
}
impl Scheduler for LAS {
    fn push(&mut self, id: TaskId, gid: u64, _at: Instant) {
        match self.tasks.get_mut(&gid) {
            None => {
                let group = Group {
                    total_service_time: 0,
                    tasks: VecDeque::from_iter([id]),
                };
                self.tasks.insert(gid, group);
                self.runnable.insert((0, gid), ());
            }
            Some(group) => {
                let old_len_ = group.tasks.len();
                group.tasks.push_back(id);
                if old_len_ == 0 {
                    self.runnable.insert((group.total_service_time, gid), ());
                }
            }
        }
    }
    fn pop(&mut self) -> Option<TaskId> {
        let entry = self.runnable.first_entry();
        if entry.is_none() {
            return None;
        }
        let (_, gid) = *entry.unwrap().key();
        match self.tasks.get_mut(&gid) {
            None => unreachable!("group should exist"),
            Some(group) => {
                let task = group.tasks.pop_front();
                if group.tasks.is_empty() {
                    self.runnable.remove(&(group.total_service_time, gid));
                }
                task
            }
        }
    }
    fn clear_task_state(&mut self, _id: TaskId, _gid: u64) {}

    fn clear_group_state(&mut self, gid: u64) {
        let group = self.tasks.get_mut(&gid).expect("group should exist");
        assert!(
            group.tasks.is_empty(),
            "for user groups, tasks should be empty"
        );
        self.tasks.remove(&gid);
    }

    fn is_runnable(&self) -> bool {
        !self.runnable.is_empty()
    }
    fn observe(&mut self, _id: TaskId, gid: u64, start: Instant, end: Instant, _ready: bool) {
        if let Some(group) = self.tasks.get_mut(&gid) {
            let old = group.total_service_time;
            group.total_service_time += end.duration_since(start).as_nanos();
            self.runnable.remove(&(old, gid));
            self.runnable.insert((group.total_service_time, gid), ());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_las() {
        let mut scheduler = LAS::new();
        let now = Instant::now();
        //initially not runnable but it will be after pushing a task
        assert!(!scheduler.is_runnable());
        scheduler.push(0, 0, now);
        scheduler.push(1, 0, now);
        scheduler.push(2, 1, now);
        assert!(scheduler.is_runnable());

        // initially all groups have total service time 0
        // so it returns task 0 from group 0
        assert_eq!(scheduler.pop(), Some(0));
        // now observe that task 0 was run for 1 nanosecond
        let start = Instant::now();
        let end = start + std::time::Duration::from_nanos(2);
        scheduler.observe(0, 0, start, end, true);
        assert_eq!(scheduler.tasks.get(&0).unwrap().total_service_time, 2);

        // now group 0 has service time and group 1 has no service time
        // so it returns task 2 from group 1
        assert_eq!(scheduler.pop(), Some(2));
        assert!(scheduler.is_runnable());

        // meanwhile enqueue another task in group 1
        scheduler.push(3, 1, now);
        assert!(scheduler.is_runnable());

        // now observe that task 2 ran for 1 nanosecond
        let start = Instant::now();
        let end = start + std::time::Duration::from_nanos(1);
        scheduler.observe(2, 1, start, end, true);
        assert_eq!(scheduler.pop(), Some(3));
        assert!(scheduler.is_runnable());
        // last pop should give us task with id 1 from group 0
        assert_eq!(scheduler.pop(), Some(1));
        assert!(!scheduler.is_runnable());
    }
}
