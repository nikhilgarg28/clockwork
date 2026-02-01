use crate::mpsc::Mpsc;
use crate::queue::{QueueKey, TaskId};
use futures::task::ArcWake;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shared, thread-safe header touched by the Waker and JoinHandle.
#[derive(Debug)]
pub struct TaskHeader<K: QueueKey> {
    id: TaskId,
    qid: K,
    // changes to true when task is enqueued
    // if this flag is true, it's not enqueued again
    queued: AtomicBool,

    // changes to true when task returns Ready after poll
    // if this flag is true, executor can clear its state
    done: AtomicBool,

    // changes to true when task is cancelled
    // this is the ground truth for cancellation, not join state
    cancelled: AtomicBool,
    ingress_tx: Arc<Mpsc<TaskId>>,
}

impl<K: QueueKey> TaskHeader<K> {
    pub fn new(id: TaskId, qid: K, ingress_tx: Arc<Mpsc<TaskId>>) -> Self {
        Self {
            id,
            qid,
            queued: AtomicBool::new(false),
            done: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            ingress_tx,
        }
    }
    pub fn qid(&self) -> K {
        self.qid
    }
    #[inline]
    pub fn enqueue(&self) {
        if self.is_done() || self.is_cancelled() {
            return;
        }
        // Only one outstanding notification per task.
        if !self.queued.swap(true, Ordering::AcqRel) {
            // Unbounded send should not block.
            // If receiver is dropped, ignore.
            let _ = self.ingress_tx.enqueue(self.id);
        }
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    #[inline]
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
    pub fn set_done(&self) {
        self.done.store(true, Ordering::Release);
    }
    #[inline]
    pub fn set_queued(&self, queued: bool) {
        self.queued.store(queued, Ordering::Release);
    }
}

impl<K: QueueKey> ArcWake for TaskHeader<K> {
    #[inline]
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.enqueue();
    }
}
