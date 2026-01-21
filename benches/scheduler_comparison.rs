//! Single-Queue Scheduler Comparison Benchmark
//!
//! Run with: cargo bench --bench scheduler_comparison
//!
//! This benchmark isolates the value of within-queue scheduling algorithms
//! (LAS vs FIFO) without any multi-queue complexity.
//!
//! Setup:
//! - Single queue
//! - Tasks with VARIABLE work (bimodal: mostly short, some long)
//! - Continuous arrivals
//! - Measure latency by task type
//!
//! If LAS helps:
//! - Short tasks should have lower p99 because they accumulate less service time
//!   and thus get scheduled ahead of long-running tasks

use clockworker::{ArrivalFifo, ExecutorBuilder, RunnableFifo, LAS};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::task::LocalSet;

use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;

// ============================================================================
// Poisson Arrival Process
// ============================================================================

/// Generate exponentially distributed inter-arrival time
/// For Poisson process with rate λ, inter-arrival times are Exp(λ)
fn exponential_delay(rng: &mut impl Rng, mean: Duration) -> Duration {
    let u: f64 = rng.gen(); // uniform [0, 1)
                            // Inverse transform: -ln(1-u) * mean, but -ln(u) works since u is uniform
    let multiplier = -u.ln();
    Duration::from_secs_f64(mean.as_secs_f64() * multiplier)
}

// ============================================================================
// Configuration
// ============================================================================

// Task distribution: bimodal (short vs long)
const TOTAL_TASKS: usize = 2000;
const SHORT_TASK_FRACTION: f64 = 0.8; // 80% short, 20% long

// Short tasks: ~25μs total work
const SHORT_TASK_YIELDS: usize = 5;
const SHORT_TASK_WORK_PER_YIELD_NS: u64 = 5_000; // 5μs

// Long tasks: ~225μs total work, 9x more than short
const LONG_TASK_YIELDS: usize = 15;
const LONG_TASK_WORK_PER_YIELD_NS: u64 = 15_000; // 15μs

const MEAN_ARRIVAL_INTERVAL: Duration = Duration::from_micros(100); // 10,000 tasks/sec average

// Benchmark iterations
const BENCH_ITERS: usize = 5;

// ============================================================================
// CPU Work Simulation
// ============================================================================

#[inline(never)]
fn do_cpu_work(ns: u64) {
    let start = Instant::now();
    let target = Duration::from_nanos(ns);

    let mut acc: u64 = 0;
    while start.elapsed() < target {
        for _ in 0..100 {
            acc = acc.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(acc);
    }
}

// ============================================================================
// Task Implementation
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskType {
    Short,
    Long,
}

struct VariableWorkTask {
    task_type: TaskType,
    yields_remaining: usize,
    work_per_yield_ns: u64,
    start_time: Option<Instant>,
    completion_record: Rc<RefCell<Option<(TaskType, Duration)>>>,
}

impl VariableWorkTask {
    fn short(completion_record: Rc<RefCell<Option<(TaskType, Duration)>>>) -> Self {
        Self {
            task_type: TaskType::Short,
            yields_remaining: SHORT_TASK_YIELDS,
            work_per_yield_ns: SHORT_TASK_WORK_PER_YIELD_NS,
            start_time: None,
            completion_record,
        }
    }

    fn long(completion_record: Rc<RefCell<Option<(TaskType, Duration)>>>) -> Self {
        Self {
            task_type: TaskType::Long,
            yields_remaining: LONG_TASK_YIELDS,
            work_per_yield_ns: LONG_TASK_WORK_PER_YIELD_NS,
            start_time: None,
            completion_record,
        }
    }
}

impl std::future::Future for VariableWorkTask {
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
            let latency = self.start_time.unwrap().elapsed();
            *self.completion_record.borrow_mut() = Some((self.task_type, latency));
            return std::task::Poll::Ready(());
        }

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

    fn len(&self) -> usize {
        self.samples.len()
    }
}

// ============================================================================
// Deterministic Task Sequence
// ============================================================================

/// Generate a deterministic sequence of task types
/// This ensures all schedulers see the same workload
fn generate_task_sequence(n: usize, short_fraction: f64) -> Vec<TaskType> {
    let mut sequence = Vec::with_capacity(n);
    let short_count = (n as f64 * short_fraction).round() as usize;

    // Interleave: every Nth task is long
    let long_interval = if short_fraction < 1.0 {
        (1.0 / (1.0 - short_fraction)).round() as usize
    } else {
        usize::MAX
    };

    for i in 0..n {
        if i % long_interval == long_interval - 1
            && sequence.iter().filter(|t| **t == TaskType::Long).count() < (n - short_count)
        {
            sequence.push(TaskType::Long);
        } else {
            sequence.push(TaskType::Short);
        }
    }

    // Verify distribution
    let actual_short = sequence.iter().filter(|t| **t == TaskType::Short).count();
    assert!(
        (actual_short as f64 / n as f64 - short_fraction).abs() < 0.05,
        "Task distribution off: expected {:.0}% short, got {:.0}%",
        short_fraction * 100.0,
        actual_short as f64 / n as f64 * 100.0
    );

    sequence
}

// ============================================================================
// Benchmark Runners
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

async fn run_clockworker(
    scheduler: SchedulerType,
    task_sequence: &[TaskType],
) -> (LatencyStats, LatencyStats, Duration) {
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

    let executor_clone = executor.clone();
    let runner = tokio::task::spawn_local(async move {
        executor_clone.run().await;
    });

    let benchmark_start = Instant::now();

    // Completion records
    let completions: Vec<Rc<RefCell<Option<(TaskType, Duration)>>>> = (0..task_sequence.len())
        .map(|_| Rc::new(RefCell::new(None)))
        .collect();

    // Spawn tasks according to sequence with arrival delays
    let mut rng = StdRng::seed_from_u64(42); // deterministic for reproducibility
    let mut handles = Vec::with_capacity(task_sequence.len());
    for (i, task_type) in task_sequence.iter().enumerate() {
        if i > 0 {
            let delay = exponential_delay(&mut rng, MEAN_ARRIVAL_INTERVAL);
            tokio::time::sleep(delay).await;
        }

        let completion = completions[i].clone();
        let task = match task_type {
            TaskType::Short => VariableWorkTask::short(completion),
            TaskType::Long => VariableWorkTask::long(completion),
        };
        handles.push(queue.spawn(task));
    }

    // Wait for all tasks
    for h in handles {
        let _ = h.await;
    }

    let total_duration = benchmark_start.elapsed();
    runner.abort();

    // Collect stats by task type
    let mut short_stats = LatencyStats::new();
    let mut long_stats = LatencyStats::new();

    for c in &completions {
        if let Some((task_type, latency)) = *c.borrow() {
            match task_type {
                TaskType::Short => short_stats.record(latency),
                TaskType::Long => long_stats.record(latency),
            }
        }
    }

    (short_stats, long_stats, total_duration)
}

async fn run_tokio(task_sequence: &[TaskType]) -> (LatencyStats, LatencyStats, Duration) {
    let benchmark_start = Instant::now();

    let completions: Vec<Rc<RefCell<Option<(TaskType, Duration)>>>> = (0..task_sequence.len())
        .map(|_| Rc::new(RefCell::new(None)))
        .collect();

    let mut rng = StdRng::seed_from_u64(42); // deterministic for reproducibility
    let mut handles = Vec::with_capacity(task_sequence.len());
    for (i, task_type) in task_sequence.iter().enumerate() {
        if i > 0 {
            let delay = exponential_delay(&mut rng, MEAN_ARRIVAL_INTERVAL);
            tokio::time::sleep(delay).await;
        }

        let completion = completions[i].clone();
        let task = match task_type {
            TaskType::Short => VariableWorkTask::short(completion),
            TaskType::Long => VariableWorkTask::long(completion),
        };
        handles.push(tokio::task::spawn_local(task));
    }

    for h in handles {
        let _ = h.await;
    }

    let total_duration = benchmark_start.elapsed();

    let mut short_stats = LatencyStats::new();
    let mut long_stats = LatencyStats::new();

    for c in &completions {
        if let Some((task_type, latency)) = *c.borrow() {
            match task_type {
                TaskType::Short => short_stats.record(latency),
                TaskType::Long => long_stats.record(latency),
            }
        }
    }

    (short_stats, long_stats, total_duration)
}

// ============================================================================
// Output
// ============================================================================

#[derive(Debug)]
struct BenchmarkResult {
    scheduler: String,
    iteration: usize,
    short_count: usize,
    short_p50_ns: u64,
    short_p90_ns: u64,
    short_p99_ns: u64,
    short_p999_ns: u64,
    short_max_ns: u64,
    long_count: usize,
    long_p50_ns: u64,
    long_p99_ns: u64,
    long_max_ns: u64,
    total_duration_ns: u64,
}

fn write_csv(results: &[BenchmarkResult], filename: &str) -> std::io::Result<()> {
    let mut file = File::create(filename)?;
    writeln!(
        file,
        "scheduler,iteration,short_count,short_p50_ns,short_p90_ns,short_p99_ns,short_p999_ns,short_max_ns,long_count,long_p50_ns,long_p99_ns,long_max_ns,total_duration_ns"
    )?;

    for r in results {
        writeln!(
            file,
            "{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.scheduler,
            r.iteration,
            r.short_count,
            r.short_p50_ns,
            r.short_p90_ns,
            r.short_p99_ns,
            r.short_p999_ns,
            r.short_max_ns,
            r.long_count,
            r.long_p50_ns,
            r.long_p99_ns,
            r.long_max_ns,
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
    println!("║      Single-Queue Scheduler Comparison Benchmark             ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("Configuration:");
    println!("  Total tasks:     {}", TOTAL_TASKS);
    println!(
        "  Short tasks:     {:.0}% ({} yields × {}μs = ~{}μs work)",
        SHORT_TASK_FRACTION * 100.0,
        SHORT_TASK_YIELDS,
        SHORT_TASK_WORK_PER_YIELD_NS / 1000,
        SHORT_TASK_YIELDS as u64 * SHORT_TASK_WORK_PER_YIELD_NS / 1000
    );
    println!(
        "  Long tasks:      {:.0}% ({} yields × {}μs = ~{}μs work)",
        (1.0 - SHORT_TASK_FRACTION) * 100.0,
        LONG_TASK_YIELDS,
        LONG_TASK_WORK_PER_YIELD_NS / 1000,
        LONG_TASK_YIELDS as u64 * LONG_TASK_WORK_PER_YIELD_NS / 1000
    );
    println!("  Mean arrival interval: {:?}", MEAN_ARRIVAL_INTERVAL);
    println!("  Iterations:      {}", BENCH_ITERS);
    println!();

    // Generate deterministic task sequence (same for all schedulers)
    let task_sequence = generate_task_sequence(TOTAL_TASKS, SHORT_TASK_FRACTION);
    let short_count = task_sequence
        .iter()
        .filter(|t| **t == TaskType::Short)
        .count();
    let long_count = task_sequence
        .iter()
        .filter(|t| **t == TaskType::Long)
        .count();
    println!("Task sequence: {} short, {} long", short_count, long_count);
    println!();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut all_results = Vec::new();
    let mut aggregated: BTreeMap<String, (Vec<LatencyStats>, Vec<LatencyStats>)> = BTreeMap::new();

    let schedulers: Vec<(&str, Option<SchedulerType>)> = vec![
        ("Tokio", None),
        ("RunnableFifo", Some(SchedulerType::RunnableFifo)),
        ("ArrivalFifo", Some(SchedulerType::ArrivalFifo)),
        ("LAS", Some(SchedulerType::LAS)),
    ];

    for (name, scheduler_opt) in &schedulers {
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("Running: {}", name);
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

        let mut short_stats_list = Vec::new();
        let mut long_stats_list = Vec::new();

        for i in 0..BENCH_ITERS {
            print!("  Iteration {}/{}...", i + 1, BENCH_ITERS);
            std::io::stdout().flush().unwrap();

            let local = LocalSet::new();
            let (short_stats, long_stats, total_duration) = rt.block_on(local.run_until(async {
                match scheduler_opt {
                    Some(s) => run_clockworker(*s, &task_sequence).await,
                    None => run_tokio(&task_sequence).await,
                }
            }));

            println!(
                " done ({:.0}ms, short p99={:.1}μs, long p99={:.1}μs)",
                total_duration.as_secs_f64() * 1000.0,
                short_stats.percentile(99.0).as_secs_f64() * 1_000_000.0,
                long_stats.percentile(99.0).as_secs_f64() * 1_000_000.0,
            );

            all_results.push(BenchmarkResult {
                scheduler: name.to_string(),
                iteration: i,
                short_count: short_stats.len(),
                short_p50_ns: short_stats.percentile(50.0).as_nanos() as u64,
                short_p90_ns: short_stats.percentile(90.0).as_nanos() as u64,
                short_p99_ns: short_stats.percentile(99.0).as_nanos() as u64,
                short_p999_ns: short_stats.percentile(99.9).as_nanos() as u64,
                short_max_ns: short_stats.percentile(100.0).as_nanos() as u64,
                long_count: long_stats.len(),
                long_p50_ns: long_stats.percentile(50.0).as_nanos() as u64,
                long_p99_ns: long_stats.percentile(99.0).as_nanos() as u64,
                long_max_ns: long_stats.percentile(100.0).as_nanos() as u64,
                total_duration_ns: total_duration.as_nanos() as u64,
            });

            short_stats_list.push(short_stats);
            long_stats_list.push(long_stats);
        }

        aggregated.insert(name.to_string(), (short_stats_list, long_stats_list));
        println!();
    }

    // Summary
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("SUMMARY: Short Task Latency (lower is better)");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    println!(
        "{:<15} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "Scheduler", "p50", "p90", "p99", "p999", "max"
    );
    println!("{}", "-".repeat(67));

    let mut combined_stats: BTreeMap<String, LatencyStats> = BTreeMap::new();

    for (name, _) in &schedulers {
        let (short_list, _) = aggregated.get(*name).unwrap();
        let mut combined = LatencyStats::new();
        for s in short_list {
            for sample in &s.samples {
                combined.record(*sample);
            }
        }

        println!(
            "{:<15} {:>9.1}μs {:>9.1}μs {:>9.1}μs {:>9.1}μs {:>9.1}μs",
            name,
            combined.percentile(50.0).as_secs_f64() * 1_000_000.0,
            combined.percentile(90.0).as_secs_f64() * 1_000_000.0,
            combined.percentile(99.0).as_secs_f64() * 1_000_000.0,
            combined.percentile(99.9).as_secs_f64() * 1_000_000.0,
            combined.percentile(100.0).as_secs_f64() * 1_000_000.0,
        );

        combined_stats.insert(name.to_string(), combined);
    }

    // Long task summary
    println!();
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("Long Task Latency (fairness check)");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    println!(
        "{:<15} {:>10} {:>10} {:>10}",
        "Scheduler", "p50", "p99", "max"
    );
    println!("{}", "-".repeat(47));

    for (name, _) in &schedulers {
        let (_, long_list) = aggregated.get(*name).unwrap();
        let mut combined = LatencyStats::new();
        for s in long_list {
            for sample in &s.samples {
                combined.record(*sample);
            }
        }

        println!(
            "{:<15} {:>9.1}μs {:>9.1}μs {:>9.1}μs",
            name,
            combined.percentile(50.0).as_secs_f64() * 1_000_000.0,
            combined.percentile(99.0).as_secs_f64() * 1_000_000.0,
            combined.percentile(100.0).as_secs_f64() * 1_000_000.0,
        );
    }

    // Analysis
    println!();
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("ANALYSIS");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    let tokio = combined_stats.get("Tokio").unwrap();
    let fifo = combined_stats.get("RunnableFifo").unwrap();
    let las = combined_stats.get("LAS").unwrap();

    let tokio_p99 = tokio.percentile(99.0).as_secs_f64() * 1_000_000.0;
    let fifo_p99 = fifo.percentile(99.0).as_secs_f64() * 1_000_000.0;
    let las_p99 = las.percentile(99.0).as_secs_f64() * 1_000_000.0;

    let tokio_p999 = tokio.percentile(99.9).as_secs_f64() * 1_000_000.0;
    let fifo_p999 = fifo.percentile(99.9).as_secs_f64() * 1_000_000.0;
    let las_p999 = las.percentile(99.9).as_secs_f64() * 1_000_000.0;

    println!();
    println!("Short task p99 comparison:");
    println!("  Tokio:        {:>7.1}μs (baseline)", tokio_p99);
    println!(
        "  FIFO:         {:>7.1}μs ({:+.1}% vs Tokio)",
        fifo_p99,
        (fifo_p99 / tokio_p99 - 1.0) * 100.0
    );
    println!(
        "  LAS:          {:>7.1}μs ({:+.1}% vs Tokio)",
        las_p99,
        (las_p99 / tokio_p99 - 1.0) * 100.0
    );

    println!();
    println!("Short task p99.9 comparison:");
    println!("  Tokio:        {:>7.1}μs (baseline)", tokio_p999);
    println!(
        "  FIFO:         {:>7.1}μs ({:+.1}% vs Tokio)",
        fifo_p999,
        (fifo_p999 / tokio_p999 - 1.0) * 100.0
    );
    println!(
        "  LAS:          {:>7.1}μs ({:+.1}% vs Tokio)",
        las_p999,
        (las_p999 / tokio_p999 - 1.0) * 100.0
    );

    if las_p99 < fifo_p99 {
        println!();
        println!(
            "✅ LAS reduces short task p99 by {:.1}x vs FIFO",
            fifo_p99 / las_p99
        );
    } else {
        println!();
        println!("⚠️  LAS did NOT improve p99 vs FIFO");
    }

    // Write CSV
    println!();
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("Writing results to scheduler_comparison_results.csv...");
    if let Err(e) = write_csv(&all_results, "scheduler_comparison_results.csv") {
        eprintln!("Failed to write CSV: {}", e);
    } else {
        println!("Done!");
    }
}
