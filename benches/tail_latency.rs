//! Tail Latency Benchmark for Clockworker
//!
//! Run with: cargo bench --bench tail_latency
//!
//! This benchmark answers: Does LAS scheduling actually reduce tail latency
//! for short tasks when mixed with long-running tasks?
//!
//! Setup:
//! - Single queue with mixed workload
//! - "Long" tasks: arrive at t=0, do lots of work (simulate background jobs)
//! - "Short" tasks: arrive continuously, do minimal work (simulate requests)
//! - Measure latency distribution of short tasks
//!
//! Expected result:
//! - FIFO: Short tasks get stuck behind long tasks → high p99
//! - LAS: Short tasks (low service time) get priority → low p99

use clockworker::{ArrivalFifo, ExecutorBuilder, RunnableFifo, LAS};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::LocalSet;

// ============================================================================
// Configuration
// ============================================================================

// Long tasks (background work)
const NUM_LONG_TASKS: usize = 20;
const LONG_TASK_YIELDS: usize = 500;
const LONG_TASK_WORK_PER_YIELD_NS: u64 = 5_000; // 5μs of CPU work per yield

// Short tasks (latency-sensitive requests)
const NUM_SHORT_TASKS: usize = 1000;
const SHORT_TASK_YIELDS: usize = 5;
const SHORT_TASK_WORK_PER_YIELD_NS: u64 = 1_000; // 1μs of CPU work per yield
const SHORT_TASK_ARRIVAL_INTERVAL: Duration = Duration::from_micros(500); // 2000/sec arrival rate

// Benchmark iterations
const BENCH_ITERS: usize = 5;

// ============================================================================
// CPU Work Simulation
// ============================================================================

/// Do approximately `ns` nanoseconds of CPU work
#[inline(never)]
fn do_cpu_work(ns: u64) {
    let start = Instant::now();
    let target = Duration::from_nanos(ns);

    // Busy loop with some actual computation to prevent optimization
    let mut acc: u64 = 0;
    while start.elapsed() < target {
        for _ in 0..100 {
            acc = acc.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(acc);
    }
}

// ============================================================================
// Task Futures
// ============================================================================

/// A task that does work and yields multiple times
struct WorkTask {
    yields_remaining: usize,
    work_per_yield_ns: u64,
    start_time: Option<Instant>,
    completion_time: Rc<RefCell<Option<Duration>>>,
}

impl WorkTask {
    fn new(yields: usize, work_ns: u64, completion_time: Rc<RefCell<Option<Duration>>>) -> Self {
        Self {
            yields_remaining: yields,
            work_per_yield_ns: work_ns,
            start_time: None,
            completion_time,
        }
    }
}

impl std::future::Future for WorkTask {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        // Record start time on first poll
        if self.start_time.is_none() {
            self.start_time = Some(Instant::now());
        }

        if self.yields_remaining == 0 {
            // Record completion latency
            let latency = self.start_time.unwrap().elapsed();
            *self.completion_time.borrow_mut() = Some(latency);
            return std::task::Poll::Ready(());
        }

        // Do CPU work
        do_cpu_work(self.work_per_yield_ns);

        self.yields_remaining -= 1;
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    }
}

// ============================================================================
// Latency Statistics
// ============================================================================

#[derive(Debug, Clone)]
struct LatencyStats {
    samples: Vec<Duration>,
}

impl LatencyStats {
    fn new() -> Self {
        Self {
            samples: Vec::new(),
        }
    }

    fn record(&mut self, d: Duration) {
        self.samples.push(d);
    }

    fn percentile(&self, p: f64) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let mut sorted = self.samples.clone();
        sorted.sort();
        let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    fn mean(&self) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let sum: Duration = self.samples.iter().sum();
        sum / self.samples.len() as u32
    }

    fn min(&self) -> Duration {
        *self.samples.iter().min().unwrap_or(&Duration::ZERO)
    }

    fn max(&self) -> Duration {
        *self.samples.iter().max().unwrap_or(&Duration::ZERO)
    }

    fn len(&self) -> usize {
        self.samples.len()
    }
}

// ============================================================================
// Benchmark Runner
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchedulerType {
    RunnableFifo,
    ArrivalFifo,
    LAS,
}

impl std::fmt::Display for SchedulerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchedulerType::RunnableFifo => write!(f, "RunnableFifo"),
            SchedulerType::ArrivalFifo => write!(f, "ArrivalFifo"),
            SchedulerType::LAS => write!(f, "LAS"),
        }
    }
}

async fn run_mixed_workload(scheduler: SchedulerType) -> (LatencyStats, LatencyStats, Duration) {
    let executor = match scheduler {
        SchedulerType::RunnableFifo => ExecutorBuilder::new()
            .with_queue(0u8, 1, RunnableFifo::new())
            .build()
            .unwrap(),
        SchedulerType::ArrivalFifo => ExecutorBuilder::new()
            .with_queue(0u8, 1, ArrivalFifo::new())
            .build()
            .unwrap(),
        SchedulerType::LAS => ExecutorBuilder::new()
            .with_queue(0u8, 1, LAS::new())
            .build()
            .unwrap(),
    };

    let queue = executor.queue(0).unwrap();

    // Start executor
    let executor_clone = executor.clone();
    let runner = tokio::task::spawn_local(async move {
        executor_clone.run().await;
    });

    let benchmark_start = Instant::now();

    // Storage for completion times
    let long_completions: Vec<Rc<RefCell<Option<Duration>>>> = (0..NUM_LONG_TASKS)
        .map(|_| Rc::new(RefCell::new(None)))
        .collect();
    let short_completions: Vec<Rc<RefCell<Option<Duration>>>> = (0..NUM_SHORT_TASKS)
        .map(|_| Rc::new(RefCell::new(None)))
        .collect();

    // Spawn long tasks immediately (they represent ongoing background work)
    let mut long_handles = Vec::with_capacity(NUM_LONG_TASKS);
    for i in 0..NUM_LONG_TASKS {
        let completion = long_completions[i].clone();
        let task = WorkTask::new(LONG_TASK_YIELDS, LONG_TASK_WORK_PER_YIELD_NS, completion);
        long_handles.push(queue.spawn(task));
    }

    // Spawn short tasks with arrival delay (they represent incoming requests)
    let mut short_handles = Vec::with_capacity(NUM_SHORT_TASKS);
    for i in 0..NUM_SHORT_TASKS {
        // Stagger arrivals
        if i > 0 {
            tokio::time::sleep(SHORT_TASK_ARRIVAL_INTERVAL).await;
        }

        let completion = short_completions[i].clone();
        let task = WorkTask::new(SHORT_TASK_YIELDS, SHORT_TASK_WORK_PER_YIELD_NS, completion);
        short_handles.push(queue.spawn(task));
    }

    // Wait for all short tasks to complete (they're what we're measuring)
    for h in short_handles {
        let _ = h.await;
    }

    // Wait for long tasks too
    for h in long_handles {
        let _ = h.await;
    }

    let total_duration = benchmark_start.elapsed();

    runner.abort();

    // Collect latency stats
    let mut short_stats = LatencyStats::new();
    for c in &short_completions {
        if let Some(d) = *c.borrow() {
            short_stats.record(d);
        }
    }

    let mut long_stats = LatencyStats::new();
    for c in &long_completions {
        if let Some(d) = *c.borrow() {
            long_stats.record(d);
        }
    }

    (short_stats, long_stats, total_duration)
}

/// Run the same workload on raw tokio for baseline
async fn run_mixed_workload_tokio() -> (LatencyStats, LatencyStats, Duration) {
    let benchmark_start = Instant::now();

    // Storage for completion times
    let long_completions: Vec<Rc<RefCell<Option<Duration>>>> = (0..NUM_LONG_TASKS)
        .map(|_| Rc::new(RefCell::new(None)))
        .collect();
    let short_completions: Vec<Rc<RefCell<Option<Duration>>>> = (0..NUM_SHORT_TASKS)
        .map(|_| Rc::new(RefCell::new(None)))
        .collect();

    // Spawn long tasks immediately
    let mut long_handles = Vec::with_capacity(NUM_LONG_TASKS);
    for i in 0..NUM_LONG_TASKS {
        let completion = long_completions[i].clone();
        let task = WorkTask::new(LONG_TASK_YIELDS, LONG_TASK_WORK_PER_YIELD_NS, completion);
        long_handles.push(tokio::task::spawn_local(task));
    }

    // Spawn short tasks with arrival delay
    let mut short_handles = Vec::with_capacity(NUM_SHORT_TASKS);
    for i in 0..NUM_SHORT_TASKS {
        if i > 0 {
            tokio::time::sleep(SHORT_TASK_ARRIVAL_INTERVAL).await;
        }

        let completion = short_completions[i].clone();
        let task = WorkTask::new(SHORT_TASK_YIELDS, SHORT_TASK_WORK_PER_YIELD_NS, completion);
        short_handles.push(tokio::task::spawn_local(task));
    }

    // Wait for all tasks
    for h in short_handles {
        let _ = h.await;
    }
    for h in long_handles {
        let _ = h.await;
    }

    let total_duration = benchmark_start.elapsed();

    // Collect latency stats
    let mut short_stats = LatencyStats::new();
    for c in &short_completions {
        if let Some(d) = *c.borrow() {
            short_stats.record(d);
        }
    }

    let mut long_stats = LatencyStats::new();
    for c in &long_completions {
        if let Some(d) = *c.borrow() {
            long_stats.record(d);
        }
    }

    (short_stats, long_stats, total_duration)
}

// ============================================================================
// Output
// ============================================================================

fn print_latency_table(name: &str, stats: &LatencyStats) {
    println!("  {} (n={}):", name, stats.len());
    println!(
        "    p50={:>8.2}ms  p90={:>8.2}ms  p99={:>8.2}ms  p999={:>8.2}ms  max={:>8.2}ms",
        stats.percentile(50.0).as_secs_f64() * 1000.0,
        stats.percentile(90.0).as_secs_f64() * 1000.0,
        stats.percentile(99.0).as_secs_f64() * 1000.0,
        stats.percentile(99.9).as_secs_f64() * 1000.0,
        stats.max().as_secs_f64() * 1000.0,
    );
}

#[derive(Debug)]
struct BenchmarkResult {
    scheduler: String,
    iteration: usize,
    short_p50_ns: u64,
    short_p90_ns: u64,
    short_p99_ns: u64,
    short_p999_ns: u64,
    short_max_ns: u64,
    long_mean_ns: u64,
    total_duration_ns: u64,
}

fn write_csv(results: &[BenchmarkResult], filename: &str) -> std::io::Result<()> {
    let mut file = File::create(filename)?;
    writeln!(
        file,
        "scheduler,iteration,short_p50_ns,short_p90_ns,short_p99_ns,short_p999_ns,short_max_ns,long_mean_ns,total_duration_ns"
    )?;

    for r in results {
        writeln!(
            file,
            "{},{},{},{},{},{},{},{},{}",
            r.scheduler,
            r.iteration,
            r.short_p50_ns,
            r.short_p90_ns,
            r.short_p99_ns,
            r.short_p999_ns,
            r.short_max_ns,
            r.long_mean_ns,
            r.total_duration_ns,
        )?;
    }

    Ok(())
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║          Clockworker Tail Latency Benchmark                  ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("Configuration:");
    println!(
        "  Long tasks:  {} tasks × {} yields × {}μs work/yield",
        NUM_LONG_TASKS,
        LONG_TASK_YIELDS,
        LONG_TASK_WORK_PER_YIELD_NS / 1000
    );
    println!(
        "  Short tasks: {} tasks × {} yields × {}μs work/yield",
        NUM_SHORT_TASKS,
        SHORT_TASK_YIELDS,
        SHORT_TASK_WORK_PER_YIELD_NS / 1000
    );
    println!(
        "  Short task arrival: every {:?}",
        SHORT_TASK_ARRIVAL_INTERVAL
    );
    println!("  Iterations: {}", BENCH_ITERS);
    println!();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut all_results = Vec::new();

    // Aggregate stats across iterations
    let mut aggregated: BTreeMap<String, Vec<LatencyStats>> = BTreeMap::new();

    let schedulers = [
        ("Tokio (baseline)", None),
        ("RunnableFifo", Some(SchedulerType::RunnableFifo)),
        ("ArrivalFifo", Some(SchedulerType::ArrivalFifo)),
        ("LAS", Some(SchedulerType::LAS)),
    ];

    for (name, scheduler_opt) in &schedulers {
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("Running: {}", name);
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

        let mut iter_stats = Vec::new();

        for i in 0..BENCH_ITERS {
            print!("  Iteration {}/{}...", i + 1, BENCH_ITERS);
            std::io::stdout().flush().unwrap();

            let local = LocalSet::new();
            let (short_stats, long_stats, total_duration) = rt.block_on(local.run_until(async {
                match scheduler_opt {
                    Some(s) => run_mixed_workload(*s).await,
                    None => run_mixed_workload_tokio().await,
                }
            }));

            println!(
                " done ({:.1}ms, short p99={:.2}ms)",
                total_duration.as_secs_f64() * 1000.0,
                short_stats.percentile(99.0).as_secs_f64() * 1000.0
            );

            all_results.push(BenchmarkResult {
                scheduler: name.to_string(),
                iteration: i,
                short_p50_ns: short_stats.percentile(50.0).as_nanos() as u64,
                short_p90_ns: short_stats.percentile(90.0).as_nanos() as u64,
                short_p99_ns: short_stats.percentile(99.0).as_nanos() as u64,
                short_p999_ns: short_stats.percentile(99.9).as_nanos() as u64,
                short_max_ns: short_stats.max().as_nanos() as u64,
                long_mean_ns: long_stats.mean().as_nanos() as u64,
                total_duration_ns: total_duration.as_nanos() as u64,
            });

            iter_stats.push(short_stats);
        }

        // Combine all samples from all iterations
        let mut combined = LatencyStats::new();
        for s in &iter_stats {
            for sample in &s.samples {
                combined.record(*sample);
            }
        }

        print_latency_table("Short tasks (combined)", &combined);
        aggregated.insert(name.to_string(), iter_stats);
        println!();
    }

    // Summary comparison
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("SUMMARY: Short Task Latency (lower is better)");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    println!(
        "{:<20} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "Scheduler", "p50", "p90", "p99", "p999", "max"
    );
    println!("{}", "-".repeat(72));

    for (name, _) in &schedulers {
        let stats_list = aggregated.get(*name).unwrap();
        let mut combined = LatencyStats::new();
        for s in stats_list {
            for sample in &s.samples {
                combined.record(*sample);
            }
        }

        println!(
            "{:<20} {:>9.2}ms {:>9.2}ms {:>9.2}ms {:>9.2}ms {:>9.2}ms",
            name,
            combined.percentile(50.0).as_secs_f64() * 1000.0,
            combined.percentile(90.0).as_secs_f64() * 1000.0,
            combined.percentile(99.0).as_secs_f64() * 1000.0,
            combined.percentile(99.9).as_secs_f64() * 1000.0,
            combined.max().as_secs_f64() * 1000.0,
        );
    }

    // Calculate improvement
    println!();
    let tokio_stats: LatencyStats = {
        let mut combined = LatencyStats::new();
        for s in aggregated.get("Tokio (baseline)").unwrap() {
            for sample in &s.samples {
                combined.record(*sample);
            }
        }
        combined
    };

    let las_stats: LatencyStats = {
        let mut combined = LatencyStats::new();
        for s in aggregated.get("LAS").unwrap() {
            for sample in &s.samples {
                combined.record(*sample);
            }
        }
        combined
    };

    let fifo_stats: LatencyStats = {
        let mut combined = LatencyStats::new();
        for s in aggregated.get("RunnableFifo").unwrap() {
            for sample in &s.samples {
                combined.record(*sample);
            }
        }
        combined
    };

    println!("Improvement Analysis:");

    let tokio_p99 = tokio_stats.percentile(99.0).as_secs_f64() * 1000.0;
    let las_p99 = las_stats.percentile(99.0).as_secs_f64() * 1000.0;
    let fifo_p99 = fifo_stats.percentile(99.0).as_secs_f64() * 1000.0;

    println!(
        "  LAS vs Tokio p99:      {:.2}ms vs {:.2}ms ({:+.1}%)",
        las_p99,
        tokio_p99,
        (las_p99 / tokio_p99 - 1.0) * 100.0
    );
    println!(
        "  LAS vs FIFO p99:       {:.2}ms vs {:.2}ms ({:+.1}%)",
        las_p99,
        fifo_p99,
        (las_p99 / fifo_p99 - 1.0) * 100.0
    );

    let tokio_p999 = tokio_stats.percentile(99.9).as_secs_f64() * 1000.0;
    let las_p999 = las_stats.percentile(99.9).as_secs_f64() * 1000.0;
    let fifo_p999 = fifo_stats.percentile(99.9).as_secs_f64() * 1000.0;

    println!(
        "  LAS vs Tokio p99.9:    {:.2}ms vs {:.2}ms ({:+.1}%)",
        las_p999,
        tokio_p999,
        (las_p999 / tokio_p999 - 1.0) * 100.0
    );
    println!(
        "  LAS vs FIFO p99.9:     {:.2}ms vs {:.2}ms ({:+.1}%)",
        las_p999,
        fifo_p999,
        (las_p999 / fifo_p999 - 1.0) * 100.0
    );

    // Write CSV
    println!();
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("Writing results to tail_latency_results.csv...");
    if let Err(e) = write_csv(&all_results, "tail_latency_results.csv") {
        eprintln!("Failed to write CSV: {}", e);
    } else {
        println!("Done!");
    }
}
