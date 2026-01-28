// Profiling benchmark to analyze clockworker overhead
// Run with: cargo bench --bench overhead_profile
// Profile with: samply record target/release/deps/overhead_profile-*

mod utils;

use std::time::{Duration, Instant};
use tokio::task::LocalSet;

const WARMUP_TASKS: usize = 1000;
const PROFILE_TASKS: usize = 50_000;

/// Simple async work: yields a few times
async fn simple_work() {
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
}

/// Baseline: Vanilla Tokio LocalSet
fn benchmark_tokio_baseline() -> Duration {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let local = LocalSet::new();

        local
            .run_until(async {
                // Warmup
                for _ in 0..WARMUP_TASKS {
                    tokio::task::spawn_local(simple_work());
                }
                tokio::task::yield_now().await;

                // Actual measurement
                let start = Instant::now();
                let mut handles = Vec::with_capacity(PROFILE_TASKS);

                for _ in 0..PROFILE_TASKS {
                    handles.push(tokio::task::spawn_local(simple_work()));
                }

                for h in handles {
                    let _ = h.await;
                }

                start.elapsed()
            })
            .await
    })
}

/// Clockworker with RunnableFifo (minimal overhead scheduler)
fn benchmark_clockworker_fifo() -> Duration {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let local = LocalSet::new();

        local
            .run_until(async {
                let executor = clockworker::ExecutorBuilder::new()
                    .with_queue(0, 1)
                    .build()
                    .unwrap();

                let queue = executor.queue(0).unwrap();
                executor
                    .run_until(async {
                        for _ in 0..WARMUP_TASKS {
                            queue.spawn(simple_work());
                        }
                        tokio::task::yield_now().await;
                        // Actual measurement
                        let start = Instant::now();
                        let mut handles = Vec::with_capacity(PROFILE_TASKS);

                        for _ in 0..PROFILE_TASKS {
                            handles.push(queue.spawn(simple_work()));
                        }

                        for h in handles {
                            let _ = h.await;
                        }

                        start.elapsed()
                    })
                    .await
            })
            .await
    })
}

fn main() {
    println!("Overhead Profiling Benchmark");
    println!("============================");
    println!("Tasks: {} (after {} warmup)", PROFILE_TASKS, WARMUP_TASKS);
    println!();

    // Run each benchmark 3 times
    let mut tokio_times = Vec::new();
    let mut cw_fifo_times = Vec::new();

    for i in 1..=3 {
        println!("Run {}...", i);

        tokio_times.push(benchmark_tokio_baseline());
        cw_fifo_times.push(benchmark_clockworker_fifo());

        println!("  Tokio:          {:?}", tokio_times.last().unwrap());
        println!("  CW (FIFO): {:?}", cw_fifo_times.last().unwrap());
        println!();
    }

    // Calculate averages
    let avg = |times: &[Duration]| {
        let sum: Duration = times.iter().sum();
        sum / times.len() as u32
    };

    let tokio_avg = avg(&tokio_times);
    let fifo_avg = avg(&cw_fifo_times);

    println!("=== Average Results ===");
    println!("Tokio baseline:          {:?}", tokio_avg);
    println!(
        "CW+RunnableFifo:         {:?} ({:+.2}%)",
        fifo_avg,
        ((fifo_avg.as_secs_f64() / tokio_avg.as_secs_f64()) - 1.0) * 100.0
    );
    println!();
    println!("=== Per-Task Overhead ===");
    println!(
        "RunnableFifo: {:.2}ns/task",
        (fifo_avg.as_nanos() as f64 - tokio_avg.as_nanos() as f64) / PROFILE_TASKS as f64
    );
}
