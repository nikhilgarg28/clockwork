//! Overhead benchmarks for Clockworker
//!
//! Run with: cargo bench --bench overhead
//!
//! Benchmarks:
//! - 1A: Spawn throughput (minimal work tasks)
//! - 1B: Yield/poll overhead (tasks that yield K times)
//! - 1C: IO reactor integration (timer-based tasks)

use clockworker::{ExecutorBuilder, RunnableFifo, LAS};
use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::LocalSet;

// ============================================================================
// Configuration
// ============================================================================

const WARMUP_ITERS: usize = 3;
const BENCH_ITERS: usize = 10;

// 1A: Spawn throughput
const SPAWN_TASK_COUNTS: &[usize] = &[1_000, 10_000, 100_000];

// 1B: Yield overhead
const YIELD_TASK_COUNT: usize = 1_000;
const YIELDS_PER_TASK: &[usize] = &[10, 100, 1_000];

// 1C: IO reactor
const IO_TASK_COUNT: usize = 100;
const SLEEPS_PER_TASK: usize = 10;
const SLEEP_DURATION: Duration = Duration::from_micros(500);

// ============================================================================
// Result types
// ============================================================================

#[derive(Debug, Clone)]
struct BenchResult {
    name: String,
    variant: String,
    parameter: usize,
    iterations: Vec<Duration>,
}

impl BenchResult {
    fn new(name: &str, variant: &str, parameter: usize) -> Self {
        Self {
            name: name.to_string(),
            variant: variant.to_string(),
            parameter,
            iterations: Vec::new(),
        }
    }

    fn record(&mut self, duration: Duration) {
        self.iterations.push(duration);
    }

    fn mean(&self) -> Duration {
        let sum: Duration = self.iterations.iter().sum();
        sum / self.iterations.len() as u32
    }

    fn min(&self) -> Duration {
        *self.iterations.iter().min().unwrap()
    }

    fn max(&self) -> Duration {
        *self.iterations.iter().max().unwrap()
    }

    fn stddev(&self) -> Duration {
        let mean_ns = self.mean().as_nanos() as f64;
        let variance: f64 = self
            .iterations
            .iter()
            .map(|d| {
                let diff = d.as_nanos() as f64 - mean_ns;
                diff * diff
            })
            .sum::<f64>()
            / self.iterations.len() as f64;
        Duration::from_nanos(variance.sqrt() as u64)
    }

    fn throughput(&self, work_units: usize) -> f64 {
        let mean_secs = self.mean().as_secs_f64();
        work_units as f64 / mean_secs
    }
}

// ============================================================================
// Benchmark 1A: Spawn Throughput
// ============================================================================

/// Clockworker: spawn N tasks that increment a counter and return immediately
async fn bench_1a_clockworker(n: usize) -> Duration {
    let counter = Arc::new(AtomicU64::new(0));
    let executor = ExecutorBuilder::new()
        .with_queue(0u8, 1, RunnableFifo::new())
        .build()
        .unwrap();

    let queue = executor.queue(0).unwrap();

    // Spawn executor runner
    let executor_clone = executor.clone();
    let runner = tokio::task::spawn_local(async move {
        executor_clone.run().await;
    });

    let start = Instant::now();

    // Spawn all tasks
    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let c = counter.clone();
        handles.push(queue.spawn(async move {
            c.fetch_add(1, Ordering::Relaxed);
        }));
    }

    // Wait for all to complete
    for h in handles {
        let _ = h.await;
    }

    let elapsed = start.elapsed();

    // Shutdown

    runner.abort();

    assert_eq!(counter.load(Ordering::Relaxed), n as u64);
    elapsed
}

/// Tokio baseline: spawn_local N tasks that increment a counter
async fn bench_1a_tokio(n: usize) -> Duration {
    let counter = Arc::new(AtomicU64::new(0));

    let start = Instant::now();

    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let c = counter.clone();
        handles.push(tokio::task::spawn_local(async move {
            c.fetch_add(1, Ordering::Relaxed);
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let elapsed = start.elapsed();

    assert_eq!(counter.load(Ordering::Relaxed), n as u64);
    elapsed
}

fn run_1a() -> Vec<BenchResult> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut results = Vec::new();

    for &n in SPAWN_TASK_COUNTS {
        println!("\n  [1A] Spawn throughput: n={}", n);

        // Clockworker
        let mut cw_result = BenchResult::new("1A_spawn", "clockworker", n);

        // Warmup
        for _ in 0..WARMUP_ITERS {
            let local = LocalSet::new();
            rt.block_on(local.run_until(bench_1a_clockworker(n)));
        }

        // Bench
        for _ in 0..BENCH_ITERS {
            let local = LocalSet::new();
            let dur = rt.block_on(local.run_until(bench_1a_clockworker(n)));
            cw_result.record(dur);
            print!(".");
            std::io::stdout().flush().unwrap();
        }
        println!();

        // Tokio baseline
        let mut tokio_result = BenchResult::new("1A_spawn", "tokio", n);

        // Warmup
        for _ in 0..WARMUP_ITERS {
            let local = LocalSet::new();
            rt.block_on(local.run_until(bench_1a_tokio(n)));
        }

        // Bench
        for _ in 0..BENCH_ITERS {
            let local = LocalSet::new();
            let dur = rt.block_on(local.run_until(bench_1a_tokio(n)));
            tokio_result.record(dur);
            print!(".");
            std::io::stdout().flush().unwrap();
        }
        println!();

        // Print comparison
        let cw_tput = cw_result.throughput(n);
        let tokio_tput = tokio_result.throughput(n);
        let overhead = (cw_result.mean().as_nanos() as f64 / tokio_result.mean().as_nanos() as f64
            - 1.0)
            * 100.0;

        println!(
            "    Clockworker: {:.2} tasks/sec (mean: {:?}, stddev: {:?})",
            cw_tput,
            cw_result.mean(),
            cw_result.stddev()
        );
        println!(
            "    Tokio:       {:.2} tasks/sec (mean: {:?}, stddev: {:?})",
            tokio_tput,
            tokio_result.mean(),
            tokio_result.stddev()
        );
        println!("    Overhead:    {:.1}%", overhead);

        results.push(cw_result);
        results.push(tokio_result);
    }

    results
}

// ============================================================================
// Benchmark 1B: Yield/Poll Overhead
// ============================================================================

/// A future that yields K times before completing
struct YieldNTimes {
    remaining: usize,
}

impl YieldNTimes {
    fn new(n: usize) -> Self {
        Self { remaining: n }
    }
}

impl std::future::Future for YieldNTimes {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.remaining == 0 {
            std::task::Poll::Ready(())
        } else {
            self.remaining -= 1;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

/// Clockworker: N tasks each yielding K times
async fn bench_1b_clockworker(n: usize, k: usize, scheduler: &str) -> Duration {
    let executor = match scheduler {
        "las" => ExecutorBuilder::new()
            .with_queue(0u8, 1, LAS::new())
            .build()
            .unwrap(),
        "fifo" => ExecutorBuilder::new()
            .with_queue(0u8, 1, RunnableFifo::new())
            .build()
            .unwrap(),
        _ => panic!("Unknown scheduler: {}", scheduler),
    };

    let queue = executor.queue(0).unwrap();

    let executor_clone = executor.clone();
    let runner = tokio::task::spawn_local(async move {
        executor_clone.run().await;
    });

    let start = Instant::now();

    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        handles.push(queue.spawn(YieldNTimes::new(k)));
    }

    for h in handles {
        let _ = h.await;
    }

    let elapsed = start.elapsed();

    runner.abort();

    elapsed
}

/// Tokio baseline: N tasks each yielding K times
async fn bench_1b_tokio(n: usize, k: usize) -> Duration {
    let start = Instant::now();

    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        handles.push(tokio::task::spawn_local(YieldNTimes::new(k)));
    }

    for h in handles {
        let _ = h.await;
    }

    start.elapsed()
}

fn run_1b() -> Vec<BenchResult> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut results = Vec::new();
    let n = YIELD_TASK_COUNT;

    for &k in YIELDS_PER_TASK {
        let total_polls = n * (k + 1); // +1 for final Ready poll
        println!(
            "\n  [1B] Yield overhead: n={}, k={} ({} total polls)",
            n, k, total_polls
        );

        // Clockworker with FIFO (lightweight scheduler)
        let mut cw_fifo = BenchResult::new("1B_yield", "clockworker_fifo", k);
        for _ in 0..WARMUP_ITERS {
            let local = LocalSet::new();
            rt.block_on(local.run_until(bench_1b_clockworker(n, k, "fifo")));
        }
        for _ in 0..BENCH_ITERS {
            let local = LocalSet::new();
            let dur = rt.block_on(local.run_until(bench_1b_clockworker(n, k, "fifo")));
            cw_fifo.record(dur);
            print!(".");
            std::io::stdout().flush().unwrap();
        }
        println!();

        // Clockworker with LAS (heavier scheduler)
        let mut cw_las = BenchResult::new("1B_yield", "clockworker_las", k);
        for _ in 0..WARMUP_ITERS {
            let local = LocalSet::new();
            rt.block_on(local.run_until(bench_1b_clockworker(n, k, "las")));
        }
        for _ in 0..BENCH_ITERS {
            let local = LocalSet::new();
            let dur = rt.block_on(local.run_until(bench_1b_clockworker(n, k, "las")));
            cw_las.record(dur);
            print!(".");
            std::io::stdout().flush().unwrap();
        }
        println!();

        // Tokio baseline
        let mut tokio_result = BenchResult::new("1B_yield", "tokio", k);
        for _ in 0..WARMUP_ITERS {
            let local = LocalSet::new();
            rt.block_on(local.run_until(bench_1b_tokio(n, k)));
        }
        for _ in 0..BENCH_ITERS {
            let local = LocalSet::new();
            let dur = rt.block_on(local.run_until(bench_1b_tokio(n, k)));
            tokio_result.record(dur);
            print!(".");
            std::io::stdout().flush().unwrap();
        }
        println!();

        // Print comparison
        let tokio_polls_per_sec = tokio_result.throughput(total_polls);
        let fifo_polls_per_sec = cw_fifo.throughput(total_polls);
        let las_polls_per_sec = cw_las.throughput(total_polls);

        let fifo_overhead =
            (cw_fifo.mean().as_nanos() as f64 / tokio_result.mean().as_nanos() as f64 - 1.0)
                * 100.0;
        let las_overhead =
            (cw_las.mean().as_nanos() as f64 / tokio_result.mean().as_nanos() as f64 - 1.0) * 100.0;

        println!(
            "    Tokio:            {:.2e} polls/sec (mean: {:?})",
            tokio_polls_per_sec,
            tokio_result.mean()
        );
        println!(
            "    Clockworker/FIFO: {:.2e} polls/sec (mean: {:?}) [overhead: {:.1}%]",
            fifo_polls_per_sec,
            cw_fifo.mean(),
            fifo_overhead
        );
        println!(
            "    Clockworker/LAS:  {:.2e} polls/sec (mean: {:?}) [overhead: {:.1}%]",
            las_polls_per_sec,
            cw_las.mean(),
            las_overhead
        );

        results.push(tokio_result);
        results.push(cw_fifo);
        results.push(cw_las);
    }

    results
}

// ============================================================================
// Benchmark 1C: IO Reactor Integration
// ============================================================================

/// Clockworker: tasks that sleep (exercises reactor integration)
async fn bench_1c_clockworker(
    n: usize,
    sleeps: usize,
    sleep_dur: Duration,
) -> (Duration, Vec<Duration>) {
    let executor = ExecutorBuilder::new()
        .with_queue(0u8, 1, RunnableFifo::new())
        .build()
        .unwrap();

    let queue = executor.queue(0).unwrap();

    let executor_clone = executor.clone();
    let runner = tokio::task::spawn_local(async move {
        executor_clone.run().await;
    });

    // Collect actual sleep durations to measure accuracy
    let sleep_accuracies: Arc<std::sync::Mutex<Vec<Duration>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let start = Instant::now();

    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let accuracies = sleep_accuracies.clone();
        let dur = sleep_dur;
        handles.push(queue.spawn(async move {
            for _ in 0..sleeps {
                let before = Instant::now();
                tokio::time::sleep(dur).await;
                let actual = before.elapsed();
                accuracies.lock().unwrap().push(actual);
            }
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let elapsed = start.elapsed();

    runner.abort();

    let accuracies = Arc::try_unwrap(sleep_accuracies)
        .unwrap()
        .into_inner()
        .unwrap();
    (elapsed, accuracies)
}

/// Tokio baseline: tasks that sleep
async fn bench_1c_tokio(n: usize, sleeps: usize, sleep_dur: Duration) -> (Duration, Vec<Duration>) {
    let sleep_accuracies: Arc<std::sync::Mutex<Vec<Duration>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let start = Instant::now();

    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let accuracies = sleep_accuracies.clone();
        let dur = sleep_dur;
        handles.push(tokio::task::spawn_local(async move {
            for _ in 0..sleeps {
                let before = Instant::now();
                tokio::time::sleep(dur).await;
                let actual = before.elapsed();
                accuracies.lock().unwrap().push(actual);
            }
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let elapsed = start.elapsed();

    let accuracies = Arc::try_unwrap(sleep_accuracies)
        .unwrap()
        .into_inner()
        .unwrap();
    (elapsed, accuracies)
}

fn analyze_sleep_accuracy(
    accuracies: &[Duration],
    expected: Duration,
) -> (Duration, Duration, f64) {
    let mean: Duration = accuracies.iter().sum::<Duration>() / accuracies.len() as u32;
    let max = *accuracies.iter().max().unwrap();
    let overslept_ratio = mean.as_nanos() as f64 / expected.as_nanos() as f64;
    (mean, max, overslept_ratio)
}

fn run_1c() -> Vec<BenchResult> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut results = Vec::new();

    println!(
        "\n  [1C] IO reactor integration: n={}, sleeps={}, sleep_dur={:?}",
        IO_TASK_COUNT, SLEEPS_PER_TASK, SLEEP_DURATION
    );

    let expected_total = SLEEP_DURATION * (SLEEPS_PER_TASK as u32);
    println!(
        "    Expected minimum time: {:?} (if fully parallel)",
        expected_total
    );

    // Clockworker
    let mut cw_result = BenchResult::new("1C_io", "clockworker", IO_TASK_COUNT);
    let mut cw_accuracies = Vec::new();

    for _ in 0..WARMUP_ITERS {
        let local = LocalSet::new();
        rt.block_on(local.run_until(bench_1c_clockworker(
            IO_TASK_COUNT,
            SLEEPS_PER_TASK,
            SLEEP_DURATION,
        )));
    }

    for _ in 0..BENCH_ITERS {
        let local = LocalSet::new();
        let (dur, accuracies) = rt.block_on(local.run_until(bench_1c_clockworker(
            IO_TASK_COUNT,
            SLEEPS_PER_TASK,
            SLEEP_DURATION,
        )));
        cw_result.record(dur);
        cw_accuracies.extend(accuracies);
        print!(".");
        std::io::stdout().flush().unwrap();
    }
    println!();

    // Tokio baseline
    let mut tokio_result = BenchResult::new("1C_io", "tokio", IO_TASK_COUNT);
    let mut tokio_accuracies = Vec::new();

    for _ in 0..WARMUP_ITERS {
        let local = LocalSet::new();
        rt.block_on(local.run_until(bench_1c_tokio(
            IO_TASK_COUNT,
            SLEEPS_PER_TASK,
            SLEEP_DURATION,
        )));
    }

    for _ in 0..BENCH_ITERS {
        let local = LocalSet::new();
        let (dur, accuracies) = rt.block_on(local.run_until(bench_1c_tokio(
            IO_TASK_COUNT,
            SLEEPS_PER_TASK,
            SLEEP_DURATION,
        )));
        tokio_result.record(dur);
        tokio_accuracies.extend(accuracies);
        print!(".");
        std::io::stdout().flush().unwrap();
    }
    println!();

    // Analyze
    let (cw_mean_sleep, cw_max_sleep, cw_ratio) =
        analyze_sleep_accuracy(&cw_accuracies, SLEEP_DURATION);
    let (tokio_mean_sleep, tokio_max_sleep, tokio_ratio) =
        analyze_sleep_accuracy(&tokio_accuracies, SLEEP_DURATION);

    println!("    Tokio:");
    println!("      Total time: {:?} (mean)", tokio_result.mean());
    println!(
        "      Sleep accuracy: mean={:?} max={:?} ratio={:.2}x",
        tokio_mean_sleep, tokio_max_sleep, tokio_ratio
    );

    println!("    Clockworker:");
    println!("      Total time: {:?} (mean)", cw_result.mean());
    println!(
        "      Sleep accuracy: mean={:?} max={:?} ratio={:.2}x",
        cw_mean_sleep, cw_max_sleep, cw_ratio
    );

    let overhead =
        (cw_result.mean().as_nanos() as f64 / tokio_result.mean().as_nanos() as f64 - 1.0) * 100.0;
    println!("    Overhead: {:.1}%", overhead);

    if cw_ratio > tokio_ratio * 1.5 {
        println!(
            "    ⚠️  WARNING: Clockworker sleeps are {:.1}x longer than Tokio's - possible reactor starvation",
            cw_ratio / tokio_ratio
        );
    }

    results.push(cw_result);
    results.push(tokio_result);

    results
}

// ============================================================================
// Output
// ============================================================================

fn write_csv(results: &[BenchResult], filename: &str) -> std::io::Result<()> {
    let mut file = File::create(filename)?;
    writeln!(file, "benchmark,variant,parameter,iteration,duration_ns")?;

    for result in results {
        for (i, dur) in result.iterations.iter().enumerate() {
            writeln!(
                file,
                "{},{},{},{},{}",
                result.name,
                result.variant,
                result.parameter,
                i,
                dur.as_nanos()
            )?;
        }
    }

    Ok(())
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║          Clockworker Overhead Benchmarks                     ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("Configuration:");
    println!("  Warmup iterations: {}", WARMUP_ITERS);
    println!("  Bench iterations:  {}", BENCH_ITERS);
    println!();

    let mut all_results = Vec::new();

    // 1A: Spawn throughput
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("Benchmark 1A: Spawn Throughput");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    all_results.extend(run_1a());

    // 1B: Yield overhead
    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("Benchmark 1B: Yield/Poll Overhead");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    all_results.extend(run_1b());

    // 1C: IO reactor
    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("Benchmark 1C: IO Reactor Integration");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    all_results.extend(run_1c());

    // Write results
    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("Writing results to benchmark_results.csv...");
    if let Err(e) = write_csv(&all_results, "benchmark_results.csv") {
        eprintln!("Failed to write CSV: {}", e);
    } else {
        println!("Done!");
    }
}
