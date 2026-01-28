use crate::{queue::TaskId, scheduler::Scheduler};
use ahash::{AHashMap as HashMap, HashMapExt};
use nohash_hasher::IntMap;
use std::collections::VecDeque;
use std::time::Instant;

const NUM_BUCKETS: usize = 32;
const SAMPLE_RATE: u64 = 4; // sample 1 in 4 completions (25%)
const REORDER_INTERVAL: u64 = 128; // reorder every 128 completions (8.333%)
const EWMA_ALPHA: f64 = 0.01;
const MIN_SAMPLES: u64 = 16; // blend with prior until this many samples

/// Per-bucket statistics for estimating E[remaining | bucket_i]
#[derive(Clone, Copy)]
struct BucketStats {
    /// EWMA of remaining time (in microseconds) for tasks that entered this bucket
    mean: f64,
    /// Total samples observed for this bucket
    count: u64,
}

impl Default for BucketStats {
    fn default() -> Self {
        Self {
            mean: 0.0,
            count: 0,
        }
    }
}

impl BucketStats {
    /// Update EWMA with a new sample
    fn update(&mut self, remaining_us: u64) {
        let remaining = remaining_us as f64;
        if self.count < MIN_SAMPLES {
            self.mean = (remaining + self.mean * self.count as f64) / (self.count + 1) as f64;
        } else {
            self.mean = EWMA_ALPHA * remaining + (1.0 - EWMA_ALPHA) * self.mean;
        }
        self.count += 1;
    }

    /// Get expected remaining time, blending with LAS-like prior when samples are sparse
    fn expected_remaining(&self, bucket_idx: usize) -> f64 {
        // Prior: LAS-like behavior - assume remaining ≈ service so far ≈ bucket threshold
        // Using 2^bucket_idx as the prior (conservative estimate)
        let prior = (1u64 << bucket_idx) as f64;

        if self.count < MIN_SAMPLES {
            // Linear blend from prior to empirical as samples accumulate
            let weight = self.count as f64 / MIN_SAMPLES as f64;
            weight * self.mean + (1.0 - weight) * prior
        } else {
            self.mean
        }
    }
}

/// Quantized SRPT-approximation scheduler.
///
/// Like QLAS, tasks are placed in queues based on their group's service time bucket.
/// Unlike QLAS, queues are processed in order of *expected remaining time* (learned
/// from historical completions) rather than service time so far.
///
/// This approximates SRPT (Shortest Remaining Processing Time) scheduling, which is
/// optimal for minimizing mean response time, without requiring advance knowledge
/// of task durations.
pub struct QSRPT {
    /// Bitmask: bit p set means priority slot p has runnable tasks.
    /// After reordering, bit 0 = highest priority (shortest expected remaining).
    present: u32,

    /// queues[bucket] contains tasks whose group service time was in [2^bucket, 2^(bucket+1)) μs
    queues: [VecDeque<TaskId>; NUM_BUCKETS],

    /// Cumulative service time (nanoseconds) per group
    service: HashMap<u64, u128>,

    /// Per-bucket statistics for remaining time estimation
    bucket_stats: [BucketStats; NUM_BUCKETS],

    /// priority_to_bucket[p] = which bucket has priority p (0 = highest priority)
    priority_to_bucket: [usize; NUM_BUCKETS],
    /// bucket_to_priority[b] = what priority does bucket b have
    bucket_to_priority: [usize; NUM_BUCKETS],

    /// Total completions observed (for triggering reorder)
    completion_count: u64,
    /// Counter for sampling (sample when this hits SAMPLE_RATE)
    sample_counter: u64,

    /// Round-robin head for fair_pop (starvation prevention) - iterates over buckets
    head: usize,
    /// Iteration counter for triggering fair_pop
    iter: u64,
}

impl QSRPT {
    pub fn new() -> Self {
        let queues = std::array::from_fn(|_| VecDeque::with_capacity(256));
        // Initialize to identity mapping (LAS order: bucket i has priority i)
        let priority_to_bucket = std::array::from_fn(|i| i);
        let bucket_to_priority = std::array::from_fn(|i| i);

        Self {
            present: 0,
            queues,
            service: HashMap::with_capacity(1024),
            bucket_stats: [BucketStats::default(); NUM_BUCKETS],
            priority_to_bucket,
            bucket_to_priority,
            completion_count: 0,
            sample_counter: 0,
            head: 0,
            iter: 0,
        }
    }

    /// Record a sample from a completed group
    fn record_sample(&mut self, total_service_us: u64) {
        // For each bucket this task passed through, record remaining time at bucket entry
        for i in 0..NUM_BUCKETS {
            let bucket_threshold = 1u64 << i;
            if bucket_threshold > total_service_us {
                break;
            }
            let remaining_at_entry = total_service_us.saturating_sub(bucket_threshold);
            self.bucket_stats[i].update(remaining_at_entry);
        }
    }

    /// Recompute priority mappings and rebuild present bitmask
    fn recompute_priority_order(&mut self) {
        // Create (expected_remaining, bucket_idx) pairs and sort
        let mut order: [(f64, usize); NUM_BUCKETS] =
            std::array::from_fn(|i| (self.bucket_stats[i].expected_remaining(i), i));

        // Sort by expected remaining time (ascending = shortest remaining first)
        order.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        // Build both mappings
        for (priority, (_, bucket)) in order.iter().enumerate() {
            self.priority_to_bucket[priority] = *bucket;
            self.bucket_to_priority[*bucket] = priority;
        }

        // Rebuild present bitmask based on new priority assignments
        self.present = 0;
        for (bucket, queue) in self.queues.iter().enumerate() {
            if !queue.is_empty() {
                let priority = self.bucket_to_priority[bucket];
                self.present |= 1 << priority;
            }
        }
    }

    /// Handle group completion: sample and potentially reorder
    fn on_completion(&mut self, total_service_ns: u128) {
        self.completion_count += 1;

        // Sample at configured rate
        self.sample_counter += 1;
        if self.sample_counter >= SAMPLE_RATE {
            self.sample_counter = 0;
            // Convert to microseconds for bucket math
            let total_service_us = (total_service_ns / 1000) as u64;
            self.record_sample(total_service_us);
        }

        // Reorder queues periodically
        if self.completion_count % REORDER_INTERVAL == 0 {
            self.recompute_priority_order();
        }
    }

    /// Starvation prevention: round-robin through buckets (not priorities)
    fn fair_pop(&mut self) -> Option<TaskId> {
        debug_assert!(self.present != 0);
        for _ in 0..NUM_BUCKETS {
            let bucket = self.head;
            self.head = (bucket + 1) % NUM_BUCKETS;

            let q = &mut self.queues[bucket];
            if let Some(id) = q.pop_front() {
                if q.is_empty() {
                    let priority = self.bucket_to_priority[bucket];
                    self.present &= !(1 << priority);
                }
                return Some(id);
            }
        }
        unreachable!("fair_pop called with present != 0 but all queues empty");
    }

    /// Pop from highest priority non-empty queue (O(1) via trailing_zeros)
    fn priority_pop(&mut self) -> Option<TaskId> {
        debug_assert!(self.present != 0);

        let priority = self.present.trailing_zeros() as usize;
        let bucket = self.priority_to_bucket[priority];

        let q = &mut self.queues[bucket];
        let id = q.pop_front();

        if q.is_empty() {
            self.present &= !(1 << priority);
        }

        id
    }
}

impl Scheduler for QSRPT {
    fn init(&mut self, _mpsc: std::sync::Arc<crate::mpsc::Mpsc<TaskId>>) {
        // QSRPT doesn't use mpsc directly - it builds its own structure in push()
    }
    
    fn push(&mut self, id: TaskId, gid: u64, _at: Instant) {
        // Convert nanoseconds to microseconds for bucket calculation
        let service_us = self.service.get(&gid).map_or(0, |s| *s / 1000) as u64;
        let bucket = if service_us <= 1 {
            0
        } else {
            service_us.ilog2().min(31) as usize
        };

        self.queues[bucket].push_back(id);

        // Set the priority bit corresponding to this bucket
        let priority = self.bucket_to_priority[bucket];
        self.present |= 1 << priority;
    }

    fn pop(&mut self) -> Option<TaskId> {
        if self.present == 0 {
            return None;
        }

        self.iter += 1;

        // Starvation prevention: every 32nd pop, round-robin through buckets
        // if self.iter % 32 == 0 {
        //     return self.fair_pop();
        // }

        // Normal path: O(1) pop from highest priority queue
        self.priority_pop()
    }

    fn clear_task_state(&mut self, _id: TaskId, _gid: u64) {}

    fn clear_group_state(&mut self, gid: u64) {
        // Capture total service time for sampling before removing
        if let Some(&total_service_ns) = self.service.get(&gid) {
            self.on_completion(total_service_ns);
        }
        self.service.remove(&gid);
    }

    fn is_runnable(&self) -> bool {
        self.present != 0
    }

    fn observe(&mut self, _id: TaskId, gid: u64, start: Instant, end: Instant, _ready: bool) {
        let diff = end.duration_since(start).as_nanos() as u128;
        *self.service.entry(gid).or_insert(0) += diff;
    }
}

impl Default for QSRPT {
    fn default() -> Self {
        Self::new()
    }
}
