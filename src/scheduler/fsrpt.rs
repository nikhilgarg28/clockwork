use crate::{queue::TaskId, scheduler::Scheduler};
use ahash::HashMapExt;
use nohash_hasher::IntMap;
use std::collections::VecDeque;
use std::time::Instant;

const NUM_BUCKETS: usize = 32;
const MIN_SAMPLES: u64 = 16; // blend with prior until this many samples

// Sampling and adjustment intervals
const SAMPLE_RATE: u64 = 4; // sample 1 in 4 completions for remaining stats
const ADJUST_INTERVAL: u64 = 64; // adjust K every 64 completions

// EWMA smoothing factors
const REMAINING_ALPHA: f64 = 0.01; // for E[remaining] estimates
const WAIT_ALPHA: f64 = 0.05; // for E[wait] estimates (faster adaptation)
const SLOWDOWN_ALPHA: f64 = 0.01; // for mean slowdown tracking
const P80_ALPHA: f64 = 0.01; // for service time p80 (slow adaptation)

// Fairness parameters
const TARGET_RATIO: f64 = 1.3; // max acceptable fat/thin mean slowdown ratio
const K_MIN: f64 = 1.5; // minimum slowdown bound
const K_MAX: f64 = 100_000.0; // maximum slowdown bound
const K_INITIAL: f64 = 20.0; // starting K

// K adjustment rates
const K_TIGHTEN: f64 = 0.95; // multiply K by this when ratio > target
const K_RELAX: f64 = 1.10; // multiply K by this when ratio <= target

/// Online mean tracker
#[derive(Clone, Copy)]
struct MeanTracker {
    mean: f64,
    count: u64,
    alpha: f64,
}

impl MeanTracker {
    fn new(alpha: f64) -> Self {
        Self {
            mean: 0.0,
            count: 0,
            alpha,
        }
    }

    fn update(&mut self, value: f64) {
        if self.count < MIN_SAMPLES {
            self.mean = (value + self.mean * self.count as f64) / (self.count + 1) as f64;
        } else {
            self.mean = self.alpha * value + (1.0 - self.alpha) * self.mean;
        }
        self.count += 1;
    }

    fn mean(&self) -> f64 {
        self.mean
    }
}

/// Stochastic quantile tracker
/// Collects MIN_SAMPLES to get initial estimate, then uses asymmetric stochastic updates
struct QuantileTracker {
    q: f64,            // target quantile (0.0-1.0)
    estimate: f64,     // current quantile estimate
    samples: Vec<f64>, // samples collected during initial phase
    count: u64,
    alpha: f64, // update rate for stochastic phase
}

impl QuantileTracker {
    fn new(q: f64) -> Self {
        Self {
            q,
            estimate: 0.0,
            samples: Vec::with_capacity(MIN_SAMPLES as usize),
            count: 0,
            alpha: P80_ALPHA, // reuse the alpha constant
        }
    }

    fn update(&mut self, value: f64) {
        if self.count < MIN_SAMPLES {
            // Initial phase: collect samples
            self.samples.push(value);
            if self.count == MIN_SAMPLES - 1 {
                // Sort and take the quantile
                self.samples
                    .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let idx = ((self.q * (self.samples.len() - 1) as f64).round() as usize)
                    .min(self.samples.len() - 1);
                self.estimate = self.samples[idx];
                // Clear samples to free memory (we don't need them anymore)
                self.samples.clear();
            }
        } else {
            // Stochastic update phase
            // Use asymmetric updates: move up less often (when value > estimate)
            // and move down more often (when value < estimate)
            if value > self.estimate {
                // Value is above estimate - move estimate up
                // For p80, this happens ~20% of the time
                // Use smaller step size: alpha * (1-q) / q
                let step = self.alpha * (1.0 - self.q) / self.q;
                self.estimate += step * (value - self.estimate);
            } else {
                // Value is below estimate - move estimate down
                // For p80, this happens ~80% of the time
                // Use larger step size: alpha
                self.estimate -= self.alpha * (self.estimate - value);
            }
        }
        self.count += 1;
    }

    fn estimate(&self) -> Option<f64> {
        if self.count < MIN_SAMPLES {
            return None;
        }
        Some(self.estimate)
    }
}

/// Inner SRPT queues bucketed by E[remaining]
struct SRPTQueues {
    /// Bitmask: bit b set means bucket b has tasks
    present: u32,
    /// queues[b] contains tasks with E[remaining] in [2^b, 2^(b+1))
    /// Each entry: (task_id, group_id, enqueue_time)
    queues: [VecDeque<(TaskId, u64, Instant)>; NUM_BUCKETS],
}

impl SRPTQueues {
    fn new() -> Self {
        Self {
            present: 0,
            queues: std::array::from_fn(|_| VecDeque::with_capacity(64)),
        }
    }

    fn push(&mut self, id: TaskId, gid: u64, enqueue_time: Instant, bucket: usize) {
        self.queues[bucket].push_back((id, gid, enqueue_time));
        self.present |= 1 << bucket;
    }

    fn pop(&mut self) -> Option<(TaskId, u64, Instant, usize)> {
        if self.present == 0 {
            return None;
        }

        let bucket = self.present.trailing_zeros() as usize;
        let (id, gid, enqueue_time) = self.queues[bucket].pop_front()?;

        if self.queues[bucket].is_empty() {
            self.present &= !(1 << bucket);
        }

        Some((id, gid, enqueue_time, bucket))
    }

    fn is_empty(&self) -> bool {
        self.present == 0
    }
}

/// Fair SRPT Scheduler
///
/// Minimizes mean response time subject to fairness constraint:
///   E[slowdown | fat] / E[slowdown | thin] <= τ
///
/// Two-tier architecture:
/// - Urgent (FIFO): tasks predicted to violate slowdown bound K
/// - Normal (SRPT): tasks ordered by E[remaining]
///
/// K is auto-tuned to maintain fairness between thin and fat task classes.
pub struct FairSRPT {
    // === Queues ===
    /// Urgent queue: FIFO for tasks with slack < E[wait]
    /// Stores (task_id, enqueue_time)
    urgent: VecDeque<TaskId>,

    /// Normal queues: SRPT ordered by E[remaining]
    normal: SRPTQueues,

    // === Per-group state ===
    /// map group_id -> (service_time_ns, wait_time_ns)
    service: IntMap<u64, (u128, u128)>,

    // === Estimation statistics ===
    /// E[remaining | service_bucket] - indexed by service time bucket
    remaining_stats: [MeanTracker; NUM_BUCKETS],

    /// E[wait | e_remaining_bucket] - indexed by E[remaining] bucket
    wait_stats: [MeanTracker; NUM_BUCKETS],

    // === Service time distribution ===
    /// 80th percentile of service times (for thin/fat classification)
    service_p80: QuantileTracker,

    // === Fairness tracking ===
    /// Mean slowdown for thin tasks (service < p80)
    thin_stats: MeanTracker,

    /// Mean slowdown for fat tasks (service >= p80)
    fat_stats: MeanTracker,

    // === Adaptive threshold ===
    /// Slowdown bound K - tasks with predicted slowdown > K go urgent
    k: f64,

    // === Counters ===
    /// Total completions (for K adjustment interval)
    completion_count: u64,

    /// Urgent enqueues
    urgent_enqueue_count: u64,

    /// Normal enqueues
    normal_enqueue_count: u64,
}

impl FairSRPT {
    pub fn new() -> Self {
        Self {
            urgent: VecDeque::with_capacity(64),
            normal: SRPTQueues::new(),
            service: IntMap::with_capacity(1024),
            remaining_stats: std::array::from_fn(|_| MeanTracker::new(REMAINING_ALPHA)),
            wait_stats: std::array::from_fn(|_| MeanTracker::new(WAIT_ALPHA)),
            service_p80: QuantileTracker::new(0.8), // p80 tracker
            thin_stats: MeanTracker::new(SLOWDOWN_ALPHA),
            fat_stats: MeanTracker::new(SLOWDOWN_ALPHA),
            k: K_INITIAL,
            completion_count: 0,
            urgent_enqueue_count: 0,
            normal_enqueue_count: 0,
        }
    }

    /// Convert service time (μs) to bucket index
    #[inline]
    fn service_to_bucket(service_ns: u128) -> usize {
        let service_us = (service_ns / 1000) as usize;
        if service_us <= 1 {
            0
        } else {
            (service_us.ilog2() as usize).min(NUM_BUCKETS - 1)
        }
    }

    /// Convert E[remaining] to bucket index
    fn remaining_to_bucket(e_remaining_ns: u128) -> usize {
        let e_remaining_us = (e_remaining_ns / 1000) as usize;
        if e_remaining_us <= 1 {
            0
        } else {
            (e_remaining_us.ilog2() as usize).min(NUM_BUCKETS - 1)
        }
    }

    /// Record completion and update statistics
    fn record_completion(&mut self, _gid: u64, service_ns: u128, wait_ns: u128) {
        if service_ns == 0 {
            return;
        }
        let service_ns = service_ns as f64;
        let slowdown = (wait_ns as f64 + service_ns) / service_ns;

        // Update service p80 estimate using P² algorithm
        self.service_p80.update(service_ns as f64);
        let p80_estimate = self.service_p80.estimate().unwrap_or(1000.0); // fallback to 1ms if not enough samples

        println!(
            "Completion: service_ns: {}, wait_ns: {}, service_p80: {}, slowdown: {}",
            service_ns, wait_ns, p80_estimate, slowdown
        );

        // Classify and record slowdown
        if service_ns < p80_estimate {
            self.thin_stats.update(slowdown);
        } else {
            self.fat_stats.update(slowdown);
        }
    }

    /// Sample remaining time statistics from a completed group
    fn record_remaining_sample(&mut self, total_service_ns: u128) {
        // For each service bucket this task passed through,
        // record what the remaining time was at bucket entry
        for i in 0..NUM_BUCKETS {
            let bucket_threshold = 1u64 << i;
            if bucket_threshold as u128 > total_service_ns {
                break;
            }
            let remaining_at_entry = total_service_ns - bucket_threshold as u128;
            self.remaining_stats[i].update(remaining_at_entry as f64);
        }
    }

    /// Adjust K based on fairness ratio
    fn adjust_k(&mut self) {
        let thin_mean = self.thin_stats.mean();
        let fat_mean = self.fat_stats.mean();

        if fat_mean > thin_mean * TARGET_RATIO {
            // Fat tasks suffering: tighten threshold, rescue more
            println!(
                "tightening k, fat_mean: {}, thin_mean: {}",
                fat_mean, thin_mean
            );
            self.k *= K_TIGHTEN;
        } else {
            // Fair enough: relax toward SRPT for better mean
            println!(
                "relaxing k, fat_mean: {}, thin_mean: {}",
                fat_mean, thin_mean
            );
            self.k *= K_RELAX;
        }

        self.k = self.k.clamp(K_MIN, K_MAX);
    }
}

impl Scheduler for FairSRPT {
    fn init(&mut self, _mpsc: std::sync::Arc<crate::mpsc::Mpsc<TaskId>>) {
        // FairSRPT doesn't use mpsc directly - it builds its own structure in push()
    }
    
    fn push(&mut self, id: TaskId, gid: u64, at: Instant) {
        // Get current service time
        let (service_ns, wait_ns) = self.service.get(&gid).map_or((0, 0), |(s, w)| (*s, *w));

        // Compute E[remaining]
        let service_bucket = Self::service_to_bucket(service_ns);
        let e_remaining = self.remaining_stats[service_bucket].mean().floor() as u128;
        let e_remaining_bucket = Self::remaining_to_bucket(e_remaining);

        // Compute slack: how much wait can we still tolerate?
        // slack = (K-1) × (service + E[remaining]) - wait_so_far
        let total_expected_service = service_ns + e_remaining;
        let slack = (self.k - 1.0) * total_expected_service as f64 - wait_ns as f64;

        // Get expected wait for this priority level
        let e_wait = self.wait_stats[e_remaining_bucket].mean();
        if fastrand::f32() < 0.01 {
            println!("service_ns: {}, wait_ns: {}, e_remaining: {}, e_remaining_bucket: {}, slack: {}, e_wait: {}", service_ns, wait_ns, e_remaining, e_remaining_bucket, slack, e_wait);
        }

        // Route decision: urgent if one more hop would violate slowdown bound
        if slack < e_wait {
            self.urgent.push_back(id);
            self.urgent_enqueue_count += 1;
        } else {
            self.normal.push(id, gid, at, e_remaining_bucket);
            self.normal_enqueue_count += 1;
        }
    }

    fn pop(&mut self) -> Option<TaskId> {
        if let Some(id) = self.urgent.pop_front() {
            return Some(id);
        } else if let Some((id, gid, enqueue_time, bucket)) = self.normal.pop() {
            let now = Instant::now();
            let wait_ns = now.duration_since(enqueue_time).as_nanos() as f64;
            self.wait_stats[bucket].update(wait_ns);
            self.service
                .entry(gid)
                .and_modify(|(_s, w)| *w += wait_ns as u128)
                .or_insert((0, wait_ns as u128));
            return Some(id);
        } else {
            return None;
        }
    }

    fn clear_task_state(&mut self, _id: TaskId, _gid: u64) {}

    fn clear_group_state(&mut self, gid: u64) {
        let timing = self.service.remove(&gid);
        self.completion_count += 1;
        if self.completion_count % SAMPLE_RATE == 0 && timing.is_some() {
            let (service_ns, wait_ns) = timing.unwrap();
            self.record_completion(gid, service_ns, wait_ns);
            self.record_remaining_sample(service_ns);
        }
        if self.completion_count % ADJUST_INTERVAL == 0 {
            self.adjust_k();
            let fraction = self.urgent_enqueue_count as f64
                / (self.urgent_enqueue_count + self.normal_enqueue_count) as f64;
            println!("k: {}, fraction: {}", self.k, fraction);
        }
    }

    fn is_runnable(&self) -> bool {
        !self.urgent.is_empty() || !self.normal.is_empty()
    }

    fn observe(&mut self, _id: TaskId, gid: u64, start: Instant, end: Instant, _ready: bool) {
        let diff = end.duration_since(start).as_nanos() as u128;
        self.service
            .entry(gid)
            .and_modify(|(s, _)| *s += diff)
            .or_insert((diff, 0));
    }
}

impl Default for FairSRPT {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_to_bucket() {
        assert_eq!(FairSRPT::service_to_bucket(500), 0);
        assert_eq!(FairSRPT::service_to_bucket(1000), 0);
        assert_eq!(FairSRPT::service_to_bucket(1001), 1);
        assert_eq!(FairSRPT::service_to_bucket(1500), 1);
        assert_eq!(FairSRPT::service_to_bucket(2000), 2);
        assert_eq!(FairSRPT::service_to_bucket(10000), 9);
        assert_eq!(FairSRPT::service_to_bucket(10240), 10);
    }
}
