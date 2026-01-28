//! # Lock-free MPSC Queue (Vyukov Algorithm)
//!
//! A multi-producer single-consumer queue optimized for async executor task scheduling.
//!
//! ## Algorithm
//!
//! Based on Dmitry Vyukov's MPSC queue. Key properties:
//! - **Push**: One atomic swap + one atomic store (no CAS loop)
//! - **Pop**: Loads only in common case (single consumer = no contention)
//! - **Trade-off**: One allocation per push (vs SegQueue's amortized allocation)
//!
//! ## Why faster than SegQueue for this use case?
//!
//! SegQueue is MPMC - it pays for multi-consumer safety on every pop via CAS.
//! Since you have a single consumer (executor thread), those CAS operations
//! are pure overhead. Vyukov MPSC uses plain loads on the pop path.

use std::cell::UnsafeCell;
use std::future::poll_fn;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;

// ============================================================================
// NODE
// ============================================================================

/// Internal queue node. Allocated on push, freed on pop.
///
/// Uses MaybeUninit because:
/// - Stub node has no value (never read)
/// - After pop, value is moved out but node briefly exists before dealloc
/// - MaybeUninit::drop() is a no-op, so Box::from_raw is safe after reading
struct Node<T> {
    /// The payload. Uninitialized for stub, initialized for real nodes.
    value: MaybeUninit<T>,
    /// Link to next node. Written by producer with Release, read by consumer with Acquire.
    next: AtomicPtr<Node<T>>,
}

// ============================================================================
// MPSC QUEUE
// ============================================================================

/// Lock-free multi-producer single-consumer queue.
///
/// ## Thread Safety
///
/// - `enqueue`: Safe from any thread, concurrently
/// - `try_drain`, `recv_one`: Single consumer only (executor thread)
/// - `close`: Safe from any thread
#[derive(Debug)]
pub struct Mpsc<T> {
    // ─────────────────────────────────────────────────────────────────────────
    // PRODUCER SIDE (threads can push)
    // ─────────────────────────────────────────────────────────────────────────
    /// Points to the most recently pushed node.
    ///
    /// ## Push Algorithm
    /// ```text
    /// 1. prev = head.swap(new_node, AcqRel)  // Claim spot atomically
    /// 2. prev.next.store(new_node, Release)  // Link into list
    /// ```
    ///
    /// ## The Inconsistency Window
    ///
    /// Between steps 1 and 2, the list is "broken" - prev.next is still null
    /// even though new_node is reachable via head. Consumer handles this by
    /// spinning briefly (window is ~1 instruction).
    head: AtomicPtr<Node<T>>,

    // ─────────────────────────────────────────────────────────────────────────
    // CONSUMER SIDE (single thread only)
    // ─────────────────────────────────────────────────────────────────────────
    /// Points to the sentinel node (initially stub, then last-popped node).
    ///
    /// ## Why UnsafeCell?
    ///
    /// Only consumer writes this, so no synchronization needed. UnsafeCell
    /// provides interior mutability while keeping Mpsc: Sync (for producers).
    tail: UnsafeCell<*mut Node<T>>,

    /// The permanent stub node. Never deallocated, serves as initial sentinel.
    ///
    /// ## Why a stub?
    ///
    /// Eliminates empty-queue edge cases. Queue is never truly empty - there's
    /// always at least the sentinel. First pop skips the stub transparently.
    stub: *mut Node<T>,

    // ─────────────────────────────────────────────────────────────────────────
    // COORDINATION
    // ─────────────────────────────────────────────────────────────────────────
    /// Optimization: track "probably has items" to avoid spurious wakes.
    ///
    /// ## Protocol
    ///
    /// - Producer: if swap(true) returned false → wake (transition empty→non-empty)
    /// - Consumer: set false when queue appears empty, with careful ordering
    ///
    /// This is deliberately lossy: extra wakes are fine, missed wakes are not.
    has_items: AtomicBool,

    /// Consumer's waker registration.
    waker: AtomicWaker,

    /// Shutdown flag. Once set, enqueue returns Err and recv_one completes.
    closed: AtomicBool,
}

// SAFETY:
// - head: AtomicPtr, safe for concurrent access
// - tail: UnsafeCell but only consumer accesses, upheld by API contract
// - stub: *mut but only accessed through atomic ops or single consumer
// - has_items, waker, closed: designed for concurrent access
unsafe impl<T: Send> Send for Mpsc<T> {}
unsafe impl<T: Send> Sync for Mpsc<T> {}

impl<T> Mpsc<T> {
    /// Creates a new empty queue.
    pub fn new() -> Self {
        // Allocate stub with uninitialized value (never read)
        let stub = Box::into_raw(Box::new(Node {
            value: MaybeUninit::uninit(),
            next: AtomicPtr::new(ptr::null_mut()),
        }));

        Self {
            head: AtomicPtr::new(stub),
            tail: UnsafeCell::new(stub),
            stub,
            has_items: AtomicBool::new(false),
            waker: AtomicWaker::new(),
            closed: AtomicBool::new(false),
        }
    }

    /// Enqueues an item. Returns Err(()) if queue is closed.
    ///
    /// # Thread Safety
    ///
    /// Safe to call from any thread, including concurrently.
    ///
    /// # Allocation
    ///
    /// Allocates one Node<T> per call. If this is a bottleneck, consider
    /// a node pool (but measure first - allocator is usually fast).
    pub fn enqueue(&self, item: T) -> Result<(), ()> {
        // Check closed first (Acquire to see close()'s Release)
        if self.closed.load(Ordering::Acquire) {
            return Err(());
        }

        // Allocate node
        let node = Box::into_raw(Box::new(Node {
            value: MaybeUninit::new(item),
            next: AtomicPtr::new(ptr::null_mut()),
        }));

        // ─────────────────────────────────────────────────────────────────────
        // PUSH STEP 1: Claim our spot by swapping head
        // ─────────────────────────────────────────────────────────────────────
        //
        // AcqRel because:
        // - Acquire: sync with previous pusher's Release store to prev.next
        //   (we need to see prev as a valid node to write its next pointer)
        // - Release: sync with consumer's Acquire load of head
        //   (consumer needs to see our node as fully initialized)
        let prev = self.head.swap(node, Ordering::AcqRel);

        // ─────────────────────────────────────────────────────────────────────
        // PUSH STEP 2: Link previous node to us
        // ─────────────────────────────────────────────────────────────────────
        //
        // Release because: syncs with consumer's Acquire load of next.
        // After this store, consumer can reach our node by following prev.next.
        //
        // IMPORTANT: Between step 1 and step 2, the list is inconsistent:
        // - We're the new head
        // - But prev.next is still null
        //
        // Consumer detects this (tail != head but tail.next is null) and spins.
        // Window is ~1 store instruction, so spin is essentially never hit.
        //
        // SAFETY: prev is valid - it's either stub (always valid) or a node
        // from a previous push (valid until popped, and we're not popped yet).
        unsafe {
            (*prev).next.store(node, Ordering::Release);
        }

        // ─────────────────────────────────────────────────────────────────────
        // WAKE: Notify consumer if queue was empty
        // ─────────────────────────────────────────────────────────────────────
        //
        // The swap returns the OLD value. If it was false (queue thought empty),
        // we're the one who transitioned to non-empty, so we must wake.
        //
        // Release ordering ensures the wake happens-after our push is visible.
        if !self.has_items.swap(true, Ordering::Release) {
            self.waker.wake();
        }

        Ok(())
    }

    /// Marks the queue as closed.
    ///
    /// After this:
    /// - `enqueue` returns `Err(())`
    /// - `recv_one` returns `Err(())` once queue is drained
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        // Wake consumer so it can observe closed state
        self.waker.wake();
    }

    /// Returns true if the queue has been closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    // ─────────────────────────────────────────────────────────────────────────
    // CONSUMER METHODS (single thread only!)
    // ─────────────────────────────────────────────────────────────────────────

    /// Attempts to pop one item.
    ///
    /// # Safety
    ///
    /// Must only be called from a single thread (the consumer/executor).
    #[inline]
    unsafe fn pop_one(&self) -> Option<T> {
        let tail = *self.tail.get();
        let mut next = (*tail).next.load(Ordering::Acquire);

        // ─────────────────────────────────────────────────────────────────────
        // STUB HANDLING
        // ─────────────────────────────────────────────────────────────────────
        //
        // First pop after init: tail points to stub. We need to skip it.
        // After skipping, tail points to the first real node (which becomes
        // the new sentinel), and we return that node's value on the NEXT pop.
        //
        // This "lag by one" is fundamental to Vyukov: we always return the
        // previous sentinel's value, not the current one.
        if tail == self.stub {
            if next.is_null() {
                // Stub.next is null. Either empty or inconsistency window.
                next = self.spin_for_next(tail)?;
            }
            // Skip past stub: make first real node the new sentinel
            *self.tail.get() = next;
            // Now retry with the real node as tail
            return self.pop_one();
        }

        // ─────────────────────────────────────────────────────────────────────
        // NORMAL CASE
        // ─────────────────────────────────────────────────────────────────────
        //
        // If tail.next exists, we can:
        // 1. Read tail's value (move it out)
        // 2. Advance tail to next (next becomes new sentinel)
        // 3. Free the old tail node
        if !next.is_null() {
            // Move value out. MaybeUninit::assume_init_read does a bitwise copy
            // and marks the source as "moved from" (logically, not physically).
            let value = (*tail).value.assume_init_read();

            // Advance tail
            *self.tail.get() = next;

            // Free old node.
            // SAFETY: We've extracted the value, and MaybeUninit's Drop is a no-op,
            // so dropping the Box just frees memory without double-drop.
            drop(Box::from_raw(tail));

            return Some(value);
        }

        // ─────────────────────────────────────────────────────────────────────
        // EMPTY OR INCONSISTENCY WINDOW
        // ─────────────────────────────────────────────────────────────────────
        //
        // tail.next is null. Two possibilities:
        // 1. Queue is truly empty (tail == head)
        // 2. Producer just swapped head but hasn't stored next yet
        //
        // We distinguish by checking if tail == head.
        self.spin_for_next(tail).map(|next| {
            let value = (*tail).value.assume_init_read();
            *self.tail.get() = next;
            drop(Box::from_raw(tail));
            value
        })
    }

    /// Spins waiting for `(*node).next` to become non-null, or returns None if empty.
    ///
    /// # The Inconsistency Window
    ///
    /// ```text
    /// Producer A                    Consumer
    /// ──────────                    ────────
    /// swap(head, nodeA) → prev
    ///                               load tail.next → null
    ///                               load head → nodeA
    ///                               tail != head, so not empty...
    ///                               but tail.next is null!
    ///                               (spin here)
    /// prev.next.store(nodeA)
    ///                               load tail.next → nodeA ✓
    /// ```
    ///
    /// This window is literally one store instruction, so we rarely spin more
    /// than 1-2 iterations. The spin_loop hint yields to hyperthreads.
    #[cold]
    #[inline(never)]
    unsafe fn spin_for_next(&self, node: *mut Node<T>) -> Option<*mut Node<T>> {
        let head = self.head.load(Ordering::Acquire);

        if node == head {
            // Truly empty
            return None;
        }

        // Inconsistency window - producer is mid-push. Spin briefly.
        //
        // SUBTLE: This spin is safe because:
        // 1. Producer is not blocked - it's actively executing
        // 2. Window is ~1-2 instructions
        // 3. No risk of priority inversion (producer doesn't hold locks)
        let mut spin = 0u32;
        loop {
            std::hint::spin_loop();

            let next = (*node).next.load(Ordering::Acquire);
            if !next.is_null() {
                return Some(next);
            }

            spin += 1;
            if spin > 1000 {
                // Should never happen unless producer thread died mid-push.
                // In debug, panic so we notice. In release, give up.
                #[cfg(debug_assertions)]
                panic!("MPSC spin limit exceeded - producer may have crashed mid-push");

                #[cfg(not(debug_assertions))]
                return None;
            }
        }
    }

    /// Checks if queue is empty (consumer only).
    #[inline]
    unsafe fn is_empty_inner(&self) -> bool {
        let tail = *self.tail.get();
        let next = (*tail).next.load(Ordering::Acquire);
        if !next.is_null() {
            return false;
        }
        let head = self.head.load(Ordering::Acquire);
        tail == head
    }

    /// Drains up to N items into the provided array.
    ///
    /// Returns the number of items drained.
    ///
    /// # Safety Contract
    ///
    /// Must only be called from the single consumer thread.
    pub fn try_drain<const N: usize>(&self, out: &mut [T; N]) -> usize {
        let mut n = 0;

        // Drain loop
        while n < N {
            // SAFETY: Single consumer invariant
            match unsafe { self.pop_one() } {
                Some(v) => {
                    out[n] = v;
                    n += 1;
                }
                None => break,
            }
        }

        // ─────────────────────────────────────────────────────────────────────
        // CLEAR has_items FLAG (with race fix)
        // ─────────────────────────────────────────────────────────────────────
        //
        // If we emptied the queue, clear the flag so future pushes will wake us.
        // But there's a race:
        //
        // Consumer                         Producer
        // ────────                         ────────
        // pop() → None (empty)
        // is_empty() → true
        //                                  push(item)
        //                                  has_items.swap(true) → true (!)
        //                                  no wake
        // has_items.store(false)
        // ☠️ Lost wakeup!
        //
        // Fix: after storing false, re-check. If items appeared, restore flag.
        //
        // SAFETY: single consumer
        if unsafe { self.is_empty_inner() } {
            self.has_items.store(false, Ordering::Release);

            // Race fix: producer might have pushed between is_empty and store
            if !unsafe { self.is_empty_inner() } && !self.has_items.swap(true, Ordering::AcqRel) {
                // We set it back to true, but swap returned false, meaning
                // the producer also saw false and will wake us. But we're
                // already awake (we're in try_drain), so that's fine.
                // The wake() here is defensive - in case producer's wake
                // happened before our swap.
                self.waker.wake();
            }
        }

        n
    }

    /// Async receive. Parks until an item is available or queue is closed.
    ///
    /// Returns `Ok(item)` or `Err(())` if closed and empty.
    ///
    /// # Safety Contract
    ///
    /// Must only be polled from the single consumer thread.
    pub async fn recv_one(&self) -> Result<T, ()> {
        poll_fn(|cx| self.poll_recv_one(cx)).await
    }

    fn poll_recv_one(&self, cx: &mut Context<'_>) -> Poll<Result<T, ()>> {
        // ─────────────────────────────────────────────────────────────────────
        // FAST PATH
        // ─────────────────────────────────────────────────────────────────────
        //
        // SAFETY: single consumer
        if let Some(v) = unsafe { self.pop_one() } {
            return Poll::Ready(Ok(v));
        }

        // Check closed before parking
        if self.closed.load(Ordering::Acquire) {
            return Poll::Ready(Err(()));
        }

        // ─────────────────────────────────────────────────────────────────────
        // REGISTER WAKER + CLEAR FLAG + RECHECK
        // ─────────────────────────────────────────────────────────────────────
        //
        // Order is CRITICAL:
        // 1. Register waker (so producer can wake us)
        // 2. Clear has_items (so producer knows to wake)
        // 3. Recheck queue (catch items pushed before we cleared flag)
        //
        // WRONG order (your original bug):
        // 1. Register waker
        // 2. Recheck queue → empty
        // 3. Clear has_items
        // 4. Return Pending
        //
        // Race with wrong order:
        //
        // Consumer                         Producer
        // ────────                         ────────
        // pop() → None
        // register(waker)
        // pop() → None
        //                                  push(item)
        //                                  has_items.swap(true) → true
        //                                  no wake!
        // has_items.store(false)
        // return Pending
        // ☠️ Item in queue, consumer sleeping, no wake coming.
        //
        // CORRECT order catches this:
        //
        // Consumer                         Producer
        // ────────                         ────────
        // pop() → None
        // register(waker)
        // has_items.store(false)
        //                                  push(item)
        //                                  has_items.swap(true) → false
        //                                  WAKE!
        // pop() → None or Some
        //
        // Either producer wakes us, or we see the item on recheck.

        self.waker.register(cx.waker());

        // BEFORE recheck! This is the fix for your bug.
        self.has_items.store(false, Ordering::Release);

        // Recheck
        // SAFETY: single consumer
        if let Some(v) = unsafe { self.pop_one() } {
            return Poll::Ready(Ok(v));
        }

        // Final closed check
        if self.closed.load(Ordering::Acquire) {
            return Poll::Ready(Err(()));
        }

        Poll::Pending
    }
}

impl<T> Default for Mpsc<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for Mpsc<T> {
    fn drop(&mut self) {
        // Drain remaining items (this also frees their nodes)
        // SAFETY: We have &mut self, so no concurrent access
        unsafe { while self.pop_one().is_some() {} }

        // Free the stub (or current sentinel if stub was replaced)
        // SAFETY: After draining, tail points to the last sentinel.
        // All other nodes have been freed. We need to free this last one.
        unsafe {
            // The final sentinel's value is uninitialized (stub) or already
            // read (was a real node whose value we returned). Either way,
            // MaybeUninit's drop is a no-op.
            let tail = *self.tail.get();
            drop(Box::from_raw(tail));
        }
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_basic_push_pop() {
        let q = Mpsc::new();

        q.enqueue(1).unwrap();
        q.enqueue(2).unwrap();
        q.enqueue(3).unwrap();

        unsafe {
            assert_eq!(q.pop_one(), Some(1));
            assert_eq!(q.pop_one(), Some(2));
            assert_eq!(q.pop_one(), Some(3));
            assert_eq!(q.pop_one(), None);
        }
    }

    #[test]
    fn test_fifo_order() {
        let q = Mpsc::new();

        for i in 0..100 {
            q.enqueue(i).unwrap();
        }

        unsafe {
            for i in 0..100 {
                assert_eq!(q.pop_one(), Some(i));
            }
            assert_eq!(q.pop_one(), None);
        }
    }

    #[test]
    fn test_try_drain() {
        let q = Mpsc::new();

        for i in 0..10 {
            q.enqueue(i).unwrap();
        }

        let mut out: [i32; 4] = [0; 4];
        let n = q.try_drain(&mut out);
        assert_eq!(n, 4);

        // Verify values
        for i in 0..4 {
            assert_eq!(out[i], i as i32);
        }

        // Drain rest
        let n = q.try_drain(&mut out);
        assert_eq!(n, 4);

        let n = q.try_drain(&mut out);
        assert_eq!(n, 2);

        let n = q.try_drain(&mut out);
        assert_eq!(n, 0);
    }

    #[test]
    fn test_close() {
        let q = Mpsc::new();

        q.enqueue(1).unwrap();
        q.close();

        assert!(q.enqueue(2).is_err());

        // Can still drain existing items
        unsafe {
            assert_eq!(q.pop_one(), Some(1));
            assert_eq!(q.pop_one(), None);
        }
    }

    #[test]
    fn test_concurrent_push() {
        let q = Arc::new(Mpsc::new());
        let num_threads = 4;
        let items_per_thread = 10_000;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let q = Arc::clone(&q);
                thread::spawn(move || {
                    for i in 0..items_per_thread {
                        q.enqueue(t * items_per_thread + i).unwrap();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Pop all and verify count
        let mut count = 0;
        unsafe {
            while q.pop_one().is_some() {
                count += 1;
            }
        }

        assert_eq!(count, num_threads * items_per_thread);
    }

    #[test]
    fn test_empty_queue() {
        let q: Mpsc<i32> = Mpsc::new();

        unsafe {
            assert_eq!(q.pop_one(), None);
            assert_eq!(q.pop_one(), None);
        }
    }

    #[test]
    fn test_drop_with_items() {
        let q = Mpsc::new();
        for i in 0..100 {
            q.enqueue(Box::new(i)).unwrap();
        }
        // Drop should free all items without leaks
        drop(q);
    }
}
