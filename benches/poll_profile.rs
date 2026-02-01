//! Poll Overhead Profiling Benchmark
//!
//! This benchmark is designed for profiling the executor's poll path using
//! flamegraph and pprof.
//!
//! Usage:
//!   # Run with flamegraph (requires cargo-flamegraph: cargo install flamegraph)
//!   cargo flamegraph --bench poll_profile
//!
//!   # Run with pprof
//!   PROFILE=pprof cargo bench --bench poll_profile
//!
//!   # Run normally (just timing)
//!   cargo bench --bench poll_profile
//!
//! Configuration via environment variables:
//!   N_TASKS: Number of tasks to spawn (default: 10000)
//!   YIELDS_PER_TASK: Number of yields per task (default: 100)
//!   EXECUTOR: "clockworker" or "tokio" (default: "clockworker")

use clockworker::ExecutorBuilder;
use protobuf::Message as ProtoMessage;
use std::env;
use std::io::Write;
use std::time::{Duration, Instant};
use tokio::task::LocalSet;

mod utils;

struct YieldKTimes {
    k: usize,
    done: usize,
}
impl std::future::Future for YieldKTimes {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        if self.done >= self.k {
            std::task::Poll::Ready(())
        } else {
            self.done += 1;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

async fn run_benchmark(executor: utils::Executor, n: usize, k: usize) -> (Duration, usize) {
    let start = Instant::now();
    let mut handles = Vec::with_capacity(n);

    // Spawn all tasks
    for _ in 0..n {
        handles.push(executor.spawn(YieldKTimes { k, done: 0 }));
    }

    // Wait for all tasks to complete
    for h in handles {
        let _ = h.await;
    }

    let elapsed = start.elapsed();
    let total_polls = n * (k + 1); // +1 for final Ready poll per task
    (elapsed, total_polls)
}

fn main() {
    // Configuration from environment or defaults
    let n: usize = env::var("N_TASKS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);
    let k: usize = env::var("YIELDS_PER_TASK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let executor_type = env::var("EXECUTOR").unwrap_or_else(|_| "clockworker".to_string());
    let use_pprof = env::var("PROFILE").map(|s| s == "pprof").unwrap_or(false);

    println!("Poll Overhead Profiling Benchmark");
    println!("==================================");
    println!("Tasks: {}", n);
    println!("Yields per task: {}", k);
    println!("Total polls: {}", n * (k + 1));
    println!("Executor: {}", executor_type);
    println!();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Setup pprof if requested
    let _guard: Option<pprof::ProfilerGuard> = if use_pprof {
        println!("Starting pprof profiler...");
        println!("Note: On macOS, pprof may require running with sudo for signal-based profiling.");

        // Try to build the profiler guard
        let guard_result = pprof::ProfilerGuardBuilder::default()
            .frequency(1000)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build();

        match guard_result {
            Ok(guard) => {
                println!("pprof profiler started successfully");
                Some(guard)
            }
            Err(e) => {
                eprintln!("\n⚠️  Failed to start pprof profiler: {}", e);
                eprintln!("\nTo enable pprof on macOS, try one of these:");
                eprintln!("  1. Run with sudo (requires root):");
                eprintln!("     sudo PROFILE=pprof cargo bench --bench poll_profile");
                eprintln!("\n  2. Use cargo-flamegraph instead (recommended, no sudo needed):");
                eprintln!("     cargo flamegraph --bench poll_profile");
                eprintln!("\n  3. Use Instruments (macOS native profiler):");
                eprintln!("     xcrun xctrace record --template 'Time Profiler' --launch -- cargo bench --bench poll_profile");
                eprintln!("\nContinuing without pprof profiling...\n");
                None
            }
        }
    } else {
        None
    };

    let (elapsed, total_polls) = rt.block_on(async {
        let local = LocalSet::new();
        let executor = match executor_type.as_str() {
            "clockworker" => {
                let cw_executor = ExecutorBuilder::new()
                    .with_queue(0u8, 1)
                    .build()
                    .unwrap();
                utils::Executor::start_clockworker(cw_executor, local).await
            }
            "tokio" => utils::Executor::start_tokio(local).await,
            _ => {
                eprintln!("Unknown executor: {}. Using clockworker.", executor_type);
                let cw_executor = ExecutorBuilder::new()
                    .with_queue(0u8, 1)
                    .build()
                    .unwrap();
                utils::Executor::start_clockworker(cw_executor, local).await
            }
        };

        println!("Running benchmark...");
        executor
            .run_until(run_benchmark(executor.clone(), n, k))
            .await
    });

    // Print results
    let polls_per_sec = total_polls as f64 / elapsed.as_secs_f64();
    println!("Results:");
    println!("  Total time: {:?}", elapsed);
    println!("  Total polls: {}", total_polls);
    println!("  Polls/sec: {:.2}", polls_per_sec);
    println!(
        "  Avg time per poll: {:.2}μs",
        elapsed.as_micros() as f64 / total_polls as f64
    );

    // Generate pprof report if requested
    if use_pprof {
        if let Some(guard) = _guard {
            println!("\nGenerating pprof report...");
            match guard.report().build() {
                Ok(report) => {
                    // Generate pprof format
                    match report.pprof() {
                        Ok(profile) => {
                            let mut file = std::fs::File::create("profile.pb")
                                .expect("Failed to create profile.pb");
                            let mut content = Vec::new();
                            profile
                                .write_to_vec(&mut content)
                                .expect("Failed to serialize profile");
                            file.write_all(&content).expect("Failed to write profile");
                            println!("pprof report generated to profile.pb");
                            println!("View with: go tool pprof profile.pb");
                            println!("Or: pprof -http=:8080 profile.pb");
                        }
                        Err(e) => {
                            eprintln!("Warning: Failed to generate pprof format: {}", e);
                        }
                    }

                    // Generate flamegraph
                    let flame_file = std::fs::File::create("flamegraph.svg")
                        .expect("Failed to create flamegraph.svg");
                    report.flamegraph(flame_file).unwrap();
                    println!("Flamegraph written to flamegraph.svg");
                }
                Err(e) => {
                    eprintln!("Failed to generate pprof report: {}", e);
                    eprintln!("Note: On macOS, pprof may require running with sudo");
                }
            }
        } else {
            eprintln!("Note: pprof profiler was not started. Use flamegraph instead:");
            eprintln!("  cargo flamegraph --bench poll_profile");
        }
    }
}
