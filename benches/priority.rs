mod utils;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use utils::{Metrics, Step, Work};

// ============================================================================
// Configuration
// ============================================================================

pub struct PriorityBenchmarkSpec {
    /// Number of foreground tasks to complete
    pub foreground_count: usize,
    /// Target foreground arrival rate (tasks per second)
    pub foreground_rps: usize,
    /// Number of concurrent background tasks
    pub background_count: usize,
}

impl Default for PriorityBenchmarkSpec {
    fn default() -> Self {
        Self {
            foreground_count: 5000,
            foreground_rps: 1000,
            background_count: 8,
        }
    }
}

// Track actual CPU time spent in work
static BACKGROUND_CPU_TIME_NS: AtomicU64 = AtomicU64::new(0);
static FOREGROUND_CPU_TIME_NS: AtomicU64 = AtomicU64::new(0);

// ============================================================================
// Background Work Tracking
// ============================================================================

/// Global counter for background work iterations across all background tasks.
/// Each background task increments this every iteration.
static BACKGROUND_ITERATIONS: AtomicU64 = AtomicU64::new(0);

fn reset_counters() {
    BACKGROUND_ITERATIONS.store(0, Ordering::SeqCst);
    BACKGROUND_CPU_TIME_NS.store(0, Ordering::SeqCst);
    FOREGROUND_CPU_TIME_NS.store(0, Ordering::SeqCst);
}

fn get_background_iterations() -> u64 {
    BACKGROUND_ITERATIONS.load(Ordering::SeqCst)
}

// ============================================================================
// Work Definitions
// ============================================================================

/// Generate foreground work: 1-3 segments of [100-500us CPU + 100-500us sleep]
fn generate_foreground_work(rng: &mut impl Rng) -> Work {
    let segments = rng.gen_range(1..=3) as usize;
    let mut steps = Vec::with_capacity(segments * 2);
    for i in 0..segments {
        let cpu = rng.gen_range(100..=500);
        steps.push(Step::CPU(Duration::from_micros(cpu)));
        if i < segments - 1 {
            let io = rng.gen_range(100..=500);
            steps.push(Step::Sleep(Duration::from_micros(io)));
        }
    }
    Work::new(steps)
}

/// Run background work in a loop until shutdown signal
async fn background_loop(shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Relaxed) {
        let cpu_start = Instant::now();
        let work = Work::new(vec![
            Step::CPU(Duration::from_micros(100)),
            Step::Sleep(Duration::from_micros(100)),
        ]);
        work.run().await;
        let cpu_time = cpu_start.elapsed().as_nanos();
        BACKGROUND_CPU_TIME_NS.fetch_add(cpu_time as u64, Ordering::Relaxed);
        BACKGROUND_ITERATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

// ============================================================================
// Foreground Task
// ============================================================================

struct ForegroundTask {
    work: Work,
    submit_time: Instant,
}

// ============================================================================
// Task Generator (runs on separate thread)
// ============================================================================

/// Generate foreground tasks with Poisson-like arrival pattern.
/// Runs on a separate thread to avoid interfering with the runtime under test.
fn generate_foreground_tasks(tx: flume::Sender<ForegroundTask>, count: usize, rps: usize) {
    let mut rng = StdRng::seed_from_u64(0xC0FFEE);
    let period = Duration::from_nanos(1_000_000_000u64 / rps as u64);

    for _ in 0..count {
        // Wait for exponentially distributed delay (Poisson arrival)
        let delay = utils::exponential_delay(&mut rng, period);
        utils::do_cpu_work(delay);

        // Generate and send task
        let work = generate_foreground_work(&mut rng);
        let task = ForegroundTask {
            work,
            submit_time: Instant::now(),
        };

        if tx.send(task).is_err() {
            break; // Receiver dropped, benchmark ending
        }
    }
}

// ============================================================================
// Benchmark: Clockworker
// ============================================================================

pub fn benchmark_clockworker(spec: PriorityBenchmarkSpec) -> BenchmarkResult {
    reset_counters();
    let (tx, rx) = flume::unbounded();
    let target_core = core_affinity::get_core_ids()
        .and_then(|cores| cores.into_iter().next())
        .unwrap();
    let other_core = core_affinity::get_core_ids()
        .and_then(|cores| cores.into_iter().nth(1))
        .unwrap();

    let gen_handle = {
        let count = spec.foreground_count;
        let rps = spec.foreground_rps;
        thread::spawn(move || {
            core_affinity::set_for_current(other_core);
            generate_foreground_tasks(tx, count, rps)
        })
    };
    let start_time = Instant::now();
    let main_handle = thread::spawn(move || {
        // Pin to target core
        core_affinity::set_for_current(target_core);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let metrics = rt.block_on(async move {
            let local = tokio::task::LocalSet::new();
            // Wait for driver to complete
            let metrics = local
                .run_until(async move {
                    let shutdown = Arc::new(AtomicBool::new(false));
                    let executor = clockworker::ExecutorBuilder::new()
                        .with_queue(0, 99)
                        .with_queue(1, 1)
                        .build()
                        .unwrap();
                    let executor_clone = executor.clone();
                    let result = executor_clone
                        .run_until(async move {
                            for _ in 0..spec.background_count {
                                let shutdown = shutdown.clone();
                                executor.queue(1).unwrap().spawn(async move {
                                    background_loop(shutdown).await;
                                });
                            }
                            let handle = executor
                                .queue(0)
                                .unwrap()
                                .spawn(async move {
                                    let mut handles = Vec::with_capacity(8192);
                                    while let Ok(task) = rx.recv_async().await {
                                        let handle = executor.queue(0).unwrap().spawn(async move {
                                            let start_time = Instant::now();
                                            let cpu_start = Instant::now();
                                            let queue_delay =
                                                start_time.duration_since(task.submit_time);
                                            task.work.run().await;
                                            let cpu_time = cpu_start.elapsed().as_nanos();
                                            FOREGROUND_CPU_TIME_NS
                                                .fetch_add(cpu_time as u64, Ordering::Relaxed);
                                            let total_latency = task.submit_time.elapsed();
                                            (queue_delay, total_latency)
                                        });
                                        handles.push(handle);
                                    }

                                    // Collect results
                                    let mut metrics = Metrics::new();
                                    for handle in handles {
                                        let (queue_delay, total_latency) = handle.await.unwrap();
                                        metrics.record(queue_delay, &["queue_delay"]);
                                        metrics.record(total_latency, &["total_latency"]);
                                    }
                                    shutdown.store(true, Ordering::SeqCst);
                                    metrics
                                })
                                .await;
                            handle.unwrap()
                        })
                        .await;
                    result
                })
                .await;
            metrics
        });
        metrics
    });
    let metrics = main_handle.join().unwrap();
    let elapsed = start_time.elapsed();
    gen_handle.join().unwrap();
    let bg_iterations = get_background_iterations();

    BenchmarkResult {
        metrics,
        elapsed,
        background_iterations: bg_iterations,
    }
}

// ============================================================================
// Benchmark: Vanilla Tokio Single-Threaded
// ============================================================================

pub fn benchmark_tokio_single(spec: PriorityBenchmarkSpec) -> BenchmarkResult {
    reset_counters();

    let (tx, rx) = flume::unbounded();
    let shutdown = Arc::new(AtomicBool::new(false));
    let target_core = core_affinity::get_core_ids()
        .and_then(|cores| cores.into_iter().next())
        .unwrap();
    let other_core = core_affinity::get_core_ids()
        .and_then(|cores| cores.into_iter().nth(1))
        .unwrap();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Spawn generator on separate thread
    let gen_handle = {
        let count = spec.foreground_count;
        let rps = spec.foreground_rps;
        thread::spawn(move || {
            core_affinity::set_for_current(other_core);
            generate_foreground_tasks(tx, count, rps)
        })
    };

    let benchmark_start = Instant::now();

    let metrics = rt.block_on(async {
        core_affinity::set_for_current(target_core);
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                // Spawn background tasks (no priority distinction - same as foreground)
                for _ in 0..spec.background_count {
                    let shutdown = shutdown.clone();
                    tokio::task::spawn_local(async move {
                        background_loop(shutdown).await;
                    });
                }

                // Drive foreground tasks
                let mut handles = Vec::with_capacity(8192);
                while let Ok(task) = rx.recv_async().await {
                    let handle = tokio::task::spawn_local(async move {
                        let start_time = Instant::now();
                        let cpu_start = Instant::now();
                        let queue_delay = start_time.duration_since(task.submit_time);
                        task.work.run().await;
                        let cpu_time = cpu_start.elapsed().as_nanos();
                        FOREGROUND_CPU_TIME_NS.fetch_add(cpu_time as u64, Ordering::Relaxed);
                        let total_latency = task.submit_time.elapsed();
                        (queue_delay, total_latency)
                    });
                    handles.push(handle);
                }

                let mut metrics = Metrics::new();
                for h in handles {
                    let (queue_delay, total_latency) = h.await.unwrap();
                    metrics.record(queue_delay, &["queue_delay"]);
                    metrics.record(total_latency, &["total_latency"]);
                }

                metrics
            })
            .await
    });

    let elapsed = benchmark_start.elapsed();

    shutdown.store(true, Ordering::SeqCst);
    gen_handle.join().unwrap();

    let bg_iterations = get_background_iterations();

    BenchmarkResult {
        metrics,
        elapsed,
        background_iterations: bg_iterations,
    }
}

// ============================================================================
// Benchmark: Two Runtimes with OS Priority
// ============================================================================

pub fn benchmark_two_runtime(spec: PriorityBenchmarkSpec) -> BenchmarkResult {
    reset_counters();

    let (task_tx, task_rx) = flume::unbounded::<ForegroundTask>();
    let (result_tx, result_rx) = flume::unbounded::<(Duration, Duration)>();
    let shutdown = Arc::new(AtomicBool::new(false));

    // Determine target core (use core 0, or first available)
    let target_core = core_affinity::get_core_ids()
        .and_then(|cores| cores.into_iter().next())
        .expect("No CPU cores available");

    // Spawn background runtime thread (low priority)
    let bg_shutdown = shutdown.clone();
    let bg_count = spec.background_count;
    let bg_handle = thread::spawn(move || {
        // Pin to target core
        core_affinity::set_for_current(target_core);

        // Set low priority (nice +19 on Unix)
        #[cfg(unix)]
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let local = tokio::task::LocalSet::new();
            local
                .run_until(async {
                    // Spawn background tasks
                    for _ in 0..bg_count {
                        let shutdown = bg_shutdown.clone();
                        tokio::task::spawn_local(async move {
                            background_loop(shutdown).await;
                        });
                    }

                    // Keep running until shutdown
                    while !bg_shutdown.load(Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await;
        });
    });

    // Spawn foreground runtime thread (high priority)
    let fg_shutdown = shutdown.clone();
    let fg_handle = thread::spawn(move || {
        // Pin to same core
        core_affinity::set_for_current(target_core);

        // Set high priority (nice -20 on Unix, requires root or CAP_SYS_NICE)
        #[cfg(unix)]
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, -20);
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let local = tokio::task::LocalSet::new();
            local
                .run_until(async {
                    let mut handles = Vec::with_capacity(8192);

                    while let Ok(task) = task_rx.recv_async().await {
                        let result_tx = result_tx.clone();
                        let handle = tokio::task::spawn_local(async move {
                            let start_time = Instant::now();
                            let queue_delay = start_time.duration_since(task.submit_time);
                            let cpu_start = Instant::now();
                            task.work.run().await;
                            let cpu_time = cpu_start.elapsed().as_nanos();
                            FOREGROUND_CPU_TIME_NS.fetch_add(cpu_time as u64, Ordering::Relaxed);
                            let total_latency = task.submit_time.elapsed();
                            let _ = result_tx.send((queue_delay, total_latency));
                        });
                        handles.push(handle);
                    }

                    // Wait for all tasks to complete
                    for h in handles {
                        let _ = h.await;
                    }

                    fg_shutdown.store(true, Ordering::SeqCst);
                })
                .await;
        });
    });

    // Spawn generator on a DIFFERENT core to avoid interference
    let other_core = core_affinity::get_core_ids()
        .and_then(|cores| cores.into_iter().nth(1))
        .unwrap_or(target_core); // Fall back if only one core

    let gen_handle = {
        let count = spec.foreground_count;
        let rps = spec.foreground_rps;
        thread::spawn(move || {
            core_affinity::set_for_current(other_core);
            generate_foreground_tasks(task_tx, count, rps);
        })
    };

    // Collect results
    let benchmark_start = Instant::now();
    gen_handle.join().unwrap();
    fg_handle.join().unwrap();
    let elapsed = benchmark_start.elapsed();

    shutdown.store(true, Ordering::SeqCst);
    bg_handle.join().unwrap();

    // Collect metrics from results channel
    let mut metrics = Metrics::new();
    while let Ok((queue_delay, total_latency)) = result_rx.try_recv() {
        metrics.record(queue_delay, &["queue_delay"]);
        metrics.record(total_latency, &["total_latency"]);
    }

    let bg_iterations = get_background_iterations();

    BenchmarkResult {
        metrics,
        elapsed,
        background_iterations: bg_iterations,
    }
}

// ============================================================================
// Results
// ============================================================================

pub struct BenchmarkResult {
    pub metrics: Metrics,
    pub elapsed: Duration,
    pub background_iterations: u64,
}

impl BenchmarkResult {
    pub fn background_throughput(&self) -> f64 {
        self.background_iterations as f64 / self.elapsed.as_secs_f64()
    }

    pub fn print_summary(&self, name: &str) {
        println!("\n=== {} ===", name);
        println!("Foreground tasks: {}", self.metrics.len());
        println!("Elapsed: {:.2?}", self.elapsed);
        println!("Background iterations: {}", self.background_iterations);
        println!(
            "Background throughput: {:.0} iter/sec",
            self.background_throughput()
        );
        println!();
        println!("Queue Delay (time from submit to task start):");
        println!(
            "  p50:  {:>10.2?}",
            self.metrics.quantile(50.0, "queue_delay")
        );
        println!(
            "  p90:  {:>10.2?}",
            self.metrics.quantile(90.0, "queue_delay")
        );
        println!(
            "  p99:  {:>10.2?}",
            self.metrics.quantile(99.0, "queue_delay")
        );
        println!(
            "  p999: {:>10.2?}",
            self.metrics.quantile(99.9, "queue_delay")
        );
        println!(
            "  max:  {:>10.2?}",
            self.metrics.quantile(100.0, "queue_delay")
        );
        println!();
        println!("Total Latency (submit to completion):");
        println!(
            "  p50:  {:>10.2?}",
            self.metrics.quantile(50.0, "total_latency")
        );
        println!(
            "  p90:  {:>10.2?}",
            self.metrics.quantile(90.0, "total_latency")
        );
        println!(
            "  p99:  {:>10.2?}",
            self.metrics.quantile(99.0, "total_latency")
        );
        println!(
            "  p999: {:>10.2?}",
            self.metrics.quantile(99.9, "total_latency")
        );
        println!(
            "  max:  {:>10.2?}",
            self.metrics.quantile(100.0, "total_latency")
        );
        let bg_cpu = Duration::from_nanos(BACKGROUND_CPU_TIME_NS.load(Ordering::SeqCst));
        let fg_cpu = Duration::from_nanos(FOREGROUND_CPU_TIME_NS.load(Ordering::SeqCst));
        println!(
            "Actual CPU time - BG: {:.2?}, FG: {:.2?}, Total: {:.2?}",
            bg_cpu,
            fg_cpu,
            bg_cpu + fg_cpu
        );
        println!(
            "CPU utilization: {:.1}%",
            (bg_cpu + fg_cpu).as_secs_f64() / self.elapsed.as_secs_f64() * 100.0
        );
    }
}

/// Diagnostic: verify threads are actually on the same core
fn verify_single_core() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Barrier;

    let barrier = Arc::new(Barrier::new(2));
    let counter1 = Arc::new(AtomicU64::new(0));
    let counter2 = Arc::new(AtomicU64::new(0));

    let target_core = core_affinity::get_core_ids()
        .and_then(|cores| cores.into_iter().next())
        .unwrap();

    let c1 = counter1.clone();
    let b1 = barrier.clone();
    let t1 = thread::spawn(move || {
        core_affinity::set_for_current(target_core);
        b1.wait(); // Sync start
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) {
            c1.fetch_add(1, Ordering::Relaxed);
        }
    });

    let c2 = counter2.clone();
    let b2 = barrier.clone();
    let t2 = thread::spawn(move || {
        core_affinity::set_for_current(target_core);
        b2.wait(); // Sync start
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) {
            c2.fetch_add(1, Ordering::Relaxed);
        }
    });

    t1.join().unwrap();
    t2.join().unwrap();

    // Also measure single-thread baseline
    let counter_solo = AtomicU64::new(0);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(1) {
        counter_solo.fetch_add(1, Ordering::Relaxed);
    }

    let solo = counter_solo.load(Ordering::Relaxed);
    let combined = counter1.load(Ordering::Relaxed) + counter2.load(Ordering::Relaxed);

    println!("Core affinity diagnostic:");
    println!("  Single thread: {} iterations", solo);
    println!("  Two threads combined: {} iterations", combined);
    println!("  Ratio: {:.2}x", combined as f64 / solo as f64);
    println!();

    if combined as f64 / solo as f64 > 1.5 {
        println!("  WARNING: Threads appear to be on different cores!");
        println!("  macOS may be ignoring core_affinity hints.");
    } else {
        println!("  OK: Threads appear to be sharing a core.");
    }
    println!();
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    verify_single_core();
    let spec = PriorityBenchmarkSpec::default();

    println!("Priority Benchmark");
    println!("==================");
    println!(
        "Foreground: {} tasks @ {} rps",
        spec.foreground_count, spec.foreground_rps
    );
    println!("Background: {} continuous tasks", spec.background_count);
    println!("Foreground work: 1-3 segments of 100μs CPU + 100μs sleep");
    println!("Background work: loop of 100μs CPU + 100μs sleep");

    // Run benchmarks
    println!("\nRunning Clockworker benchmark...");
    let cw_result = benchmark_clockworker(PriorityBenchmarkSpec::default());
    cw_result.print_summary("Clockworker");

    println!("\nRunning Tokio single-threaded benchmark...");
    let tokio_result = benchmark_tokio_single(PriorityBenchmarkSpec::default());
    tokio_result.print_summary("Tokio Single-Threaded");

    println!("\nRunning Two-Runtime (OS priority) benchmark...");
    let two_rt_result = benchmark_two_runtime(PriorityBenchmarkSpec::default());
    two_rt_result.print_summary("Two Runtimes (OS Priority)");

    // Comparison table
    println!("\n");
    println!("=== COMPARISON TABLE ===");
    println!();
    print_comparison_table(&[
        ("Clockworker", &cw_result),
        ("Tokio Single", &tokio_result),
        ("Two-RT/OS", &two_rt_result),
    ]);
}

fn print_comparison_table(results: &[(&str, &BenchmarkResult)]) {
    println!(
        "{:<15} {:>10} {:>10} {:>10} {:>10} {:>12}",
        "Config", "p50", "p99", "p999", "max", "BG iter/s"
    );
    println!(
        "{:-<15} {:-<10} {:-<10} {:-<10} {:-<10} {:-<12}",
        "", "", "", "", "", ""
    );

    for (name, result) in results {
        println!(
            "{:<15} {:>10.2?} {:>10.2?} {:>10.2?} {:>10.2?} {:>12.0}",
            name,
            result.metrics.quantile(50.0, "total_latency"),
            result.metrics.quantile(99.0, "total_latency"),
            result.metrics.quantile(99.9, "total_latency"),
            result.metrics.quantile(100.0, "total_latency"),
            result.background_throughput(),
        );
    }
}
