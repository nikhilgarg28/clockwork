use rand::{rngs::StdRng, Rng, SeedableRng};
use std::io::Write;
use std::time::{Duration, Instant};
use tabled::Table;
mod utils;
use utils::{Executor, Metrics, Work, WorkSpec};

#[derive(Clone)]
pub struct BenchmarkSpec {
    n: usize,
    thin_spec: WorkSpec,
    fat_spec: WorkSpec,
    ratio_thin: u32,
    ratio_fat: u32,
    rps: usize,
}
impl BenchmarkSpec {
    /// returns the percentage of CPU work that is expected to be done
    fn expected_cpu_work_percent(&self) -> f64 {
        let exp_num_thin_tasks =
            self.n as f64 * self.ratio_thin as f64 / (self.ratio_thin + self.ratio_fat) as f64;
        let exp_num_yields_per_thin = self.thin_spec.num_yields_min as f64
            + ((self.thin_spec.num_yields_max - self.thin_spec.num_yields_min) as f64 / 2.0);
        let exp_cpu_work_per_thin_task = {
            let min_nanos = self.thin_spec.cpu_min.as_nanos() as f64;
            let max_nanos = self.thin_spec.cpu_max.as_nanos() as f64;
            let avg_nanos = (min_nanos + max_nanos) / 2.0;
            avg_nanos * exp_num_yields_per_thin as f64
        };
        let exp_num_yields_per_fat = self.fat_spec.num_yields_min as f64
            + ((self.fat_spec.num_yields_max - self.fat_spec.num_yields_min) as f64 / 2.0);
        let exp_cpu_work_per_fat_task = {
            let min_nanos = self.fat_spec.cpu_min.as_nanos() as f64;
            let max_nanos = self.fat_spec.cpu_max.as_nanos() as f64;
            let avg_nanos = (min_nanos + max_nanos) / 2.0;
            avg_nanos * exp_num_yields_per_fat as f64
        };
        let total_thin_cpu_work_ns = exp_cpu_work_per_thin_task * exp_num_thin_tasks;
        let total_fat_cpu_work_ns =
            exp_cpu_work_per_fat_task * (self.n as f64 - exp_num_thin_tasks);
        let total_cpu_work_ns = total_thin_cpu_work_ns + total_fat_cpu_work_ns;
        let expected_duration_secs = self.n as f64 / self.rps as f64;
        let total_cpu_available_ns = expected_duration_secs * 1_000_000_000.0;
        let expected_cpu_work_percent = total_cpu_work_ns / total_cpu_available_ns;
        expected_cpu_work_percent
    }
}

struct Task {
    thin: bool,
    work: Work,
    start: Instant,
}

async fn drive(executor: Executor, tasks: flume::Receiver<Task>) -> Metrics {
    let mut metrics = Metrics::new();
    let mut handles = Vec::with_capacity(64);
    while let Ok(task) = tasks.recv_async().await {
        let recv_time = Instant::now();
        let admit_delay = recv_time.duration_since(task.start);
        metrics.record(admit_delay, &["admit_delay"]);
        let handle = executor.spawn(async move {
            let start_time = Instant::now();
            let start_delay = start_time.duration_since(recv_time);
            task.work.run().await;
            let elapsed = task.start.elapsed();
            (start_delay, elapsed, if task.thin { "thin" } else { "fat" })
        });
        handles.push(handle);
    }
    for h in handles {
        let (start_delay, duration, tag) = h.await.unwrap();
        metrics.record(start_delay, &["start_delay"]);
        metrics.record(duration, &[tag, "execution_time"]);
    }
    metrics
}

/// Generate `n` tasks with a mix of thin and fat tasks
/// `thin_ratio` is the ratio of thin tasks to total tasks
/// `thin` is the work spec for thin tasks
/// `fat` is the work spec for fat tasks
fn generate(
    tx: flume::Sender<Task>,
    n: usize,
    thin: WorkSpec,
    fat: WorkSpec,
    ratio_thin: u32,
    ratio_fat: u32,
    rps: usize, // expected rate of tasks per second
) {
    let mut rng = StdRng::seed_from_u64(0xC0FFEE);
    // Roughly rps via interval. This is not perfect Poisson; good enough.
    let period = Duration::from_nanos(1_000_000_000u64 / rps as u64);

    for _ in 0..n {
        let delay = utils::exponential_delay(&mut rng, period);
        utils::do_cpu_work(delay);
        let (thin, work) = if rng.gen_ratio(ratio_thin, ratio_thin + ratio_fat) {
            (true, thin.sample(&mut rng))
        } else {
            (false, fat.sample(&mut rng))
        };
        tx.send(Task {
            thin,
            work,
            start: Instant::now(),
        })
        .unwrap();
    }
}

fn benchmark_tokio(spec: BenchmarkSpec) -> Metrics {
    let (tx, rx) = flume::unbounded();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Generate tasks in a separate thread
    std::thread::spawn(move || {
        generate(
            tx,
            spec.n,
            spec.thin_spec,
            spec.fat_spec,
            spec.ratio_thin,
            spec.ratio_fat,
            spec.rps,
        );
    });
    let metrics = rt.block_on(async move {
        let local = tokio::task::LocalSet::new();
        let executor = Executor::start_tokio(local).await;
        executor.run_until(drive(executor.clone(), rx)).await
    });
    metrics
}

fn benchmark_clockworker(
    executor: std::rc::Rc<clockworker::Executor<u8>>,
    spec: BenchmarkSpec,
) -> Metrics {
    let (tx, rx) = flume::unbounded();
    // start the task generator in a separate thread
    std::thread::spawn(move || {
        generate(
            tx,
            spec.n,
            spec.thin_spec,
            spec.fat_spec,
            spec.ratio_thin,
            spec.ratio_fat,
            spec.rps,
        );
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let metrics = rt.block_on(async move {
        let local = tokio::task::LocalSet::new();
        let executor = Executor::start_clockworker(executor.clone(), local).await;
        executor.run_until(drive(executor.clone(), rx)).await
    });
    metrics
}

fn main() {
    let spec = BenchmarkSpec {
        n: 5000,
        thin_spec: WorkSpec {
            cpu_min: Duration::from_micros(100),
            cpu_max: Duration::from_micros(500),
            io_min: Duration::from_micros(100),
            io_max: Duration::from_micros(1000),
            num_yields_min: 0,
            num_yields_max: 5,
        },
        fat_spec: WorkSpec {
            cpu_min: Duration::from_micros(100),
            cpu_max: Duration::from_micros(500),
            io_min: Duration::from_micros(100),
            io_max: Duration::from_micros(1000),
            num_yields_min: 5,
            num_yields_max: 10,
        },
        // 90% thin tasks, 10% fat tasks
        ratio_thin: 99,
        ratio_fat: 1,
        rps: 1000,
    };
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║          Clockworker Tail Latency Benchmark                  ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("Configuration:");
    println!("  Number of tasks: {}", spec.n);
    println!("  Thin task spec: {:?}", spec.thin_spec);
    println!("  Fat task spec: {:?}", spec.fat_spec);
    println!("  Thin task ratio: {}", spec.ratio_thin);
    println!("  Fat task ratio: {}", spec.ratio_fat);
    println!("  RPS: {}", spec.rps);
    println!(
        "  Expected CPU work percent: {:.2}%",
        spec.expected_cpu_work_percent() * 100.0
    );
    println!();
    print!("Running tokio benchmark...");
    std::io::stdout().flush().unwrap();
    let start = Instant::now();
    let tokio_metrics = benchmark_tokio(spec.clone());
    println!(" done in {:?}", start.elapsed());

    print!("Running clockworker benchmark(LAS)...");
    std::io::stdout().flush().unwrap();
    let executor = clockworker::ExecutorBuilder::new()
        .with_queue_scheduler(0u8, 1, clockworker::scheduler::LAS::new())
        .build()
        .unwrap();
    let start = Instant::now();
    let clockworker_las_metrics = benchmark_clockworker(executor, spec.clone());
    println!(" done in {:?}", start.elapsed());
    print!("Running clockworker benchmark(Runnable FIFO)...");
    let executor = clockworker::ExecutorBuilder::new()
        .with_queue(0u8, 1)
        .build()
        .unwrap();
    std::io::stdout().flush().unwrap();
    let start = Instant::now();
    let clockworker_fifo_metrics = benchmark_clockworker(executor, spec.clone());
    println!(" done in {:?}", start.elapsed());

    print!("Running clockworker benchmark(QLAS)...");
    let executor = clockworker::ExecutorBuilder::new()
        .with_queue_scheduler(0u8, 1, clockworker::scheduler::QLAS::new())
        .build()
        .unwrap();
    std::io::stdout().flush().unwrap();
    let start = Instant::now();
    let clockworker_qlas_metrics = benchmark_clockworker(executor, spec.clone());
    println!(" done in {:?}", start.elapsed());

    print!("Running clockworker benchmark(QSRPT)...");
    let executor = clockworker::ExecutorBuilder::new()
        .with_queue_scheduler(0u8, 1, clockworker::scheduler::QSRPT::new())
        .build()
        .unwrap();
    std::io::stdout().flush().unwrap();
    let start = Instant::now();
    let clockworker_qsrpt_metrics = benchmark_clockworker(executor, spec.clone());
    println!(" done in {:?}", start.elapsed());

    print!("Running clockworker benchmark(FairSRPT)...");
    let executor = clockworker::ExecutorBuilder::new()
        .with_queue_scheduler(0u8, 1, clockworker::scheduler::FairSRPT::new())
        .build()
        .unwrap();
    std::io::stdout().flush().unwrap();
    let start = Instant::now();
    let clockworker_fairsrpt_metrics = benchmark_clockworker(executor, spec.clone());
    println!(" done in {:?}", start.elapsed());

    let results = vec![
        ("Tokio", tokio_metrics),
        ("Clockworker(LAS)", clockworker_las_metrics),
        ("Clockworker(Runnable FIFO)", clockworker_fifo_metrics),
        ("Clockworker(QLAS)", clockworker_qlas_metrics),
        ("Clockworker(QSRPT)", clockworker_qsrpt_metrics),
        ("Clockworker(FairSRPT)", clockworker_fairsrpt_metrics),
    ];
    print_results(&results);
}

fn print_results(results: &[(&str, Metrics)]) {
    // Find Tokio baseline
    let tokio_idx = results
        .iter()
        .position(|(name, _)| name == &"Tokio")
        .expect("Tokio baseline not found");
    let tokio_metrics = &results[tokio_idx].1;

    #[derive(tabled::Tabled)]
    struct LatencyTable {
        name: String,
        p5_thin_us: String,
        p50_thin_us: String,
        p90_thin_us: String,
        p99_thin_us: String,
        mean_thin_us: String,
        p5_fat_us: String,
        p50_fat_us: String,
        p90_fat_us: String,
        p99_fat_us: String,
        mean_fat_us: String,
        mean_admit_delay_us: String,
        mean_start_delay_us: String,
        p50_execution_time_us: String,
        p90_execution_time_us: String,
        p99_execution_time_us: String,
        mean_execution_time_us: String,
    }

    let mut rows = Vec::new();
    for (name, metrics) in results {
        let format_value = |quantile: f64, tag: &str| -> String {
            let value_us = metrics.quantile(quantile, tag).as_micros() as f64;
            if name == &"Tokio" {
                format!("{:.2}", value_us)
            } else {
                let baseline_us = tokio_metrics.quantile(quantile, tag).as_micros() as f64;
                let pct_change = if baseline_us > 0.0 {
                    ((value_us - baseline_us) / baseline_us) * 100.0
                } else {
                    0.0
                };
                format!("{:.2} ({:+.1}%)", value_us, pct_change)
            }
        };

        let format_mean = |tag: &str| -> String {
            let value_us = metrics.mean(tag).as_micros() as f64;
            if name == &"Tokio" {
                format!("{:.2}", value_us)
            } else {
                let baseline_us = tokio_metrics.mean(tag).as_micros() as f64;
                let pct_change = if baseline_us > 0.0 {
                    ((value_us - baseline_us) / baseline_us) * 100.0
                } else {
                    0.0
                };
                format!("{:.2} ({:+.1}%)", value_us, pct_change)
            }
        };

        rows.push(LatencyTable {
            name: name.to_string(),
            p5_thin_us: format_value(5.0, "thin"),
            p50_thin_us: format_value(50.0, "thin"),
            p90_thin_us: format_value(90.0, "thin"),
            p99_thin_us: format_value(99.0, "thin"),
            mean_thin_us: format_mean("thin"),
            p5_fat_us: format_value(5.0, "fat"),
            p50_fat_us: format_value(50.0, "fat"),
            p90_fat_us: format_value(90.0, "fat"),
            p99_fat_us: format_value(99.0, "fat"),
            mean_fat_us: format_mean("fat"),
            mean_admit_delay_us: format_mean("admit_delay"),
            mean_start_delay_us: format_mean("start_delay"),
            mean_execution_time_us: format_mean("execution_time"),
            p50_execution_time_us: format_value(50.0, "execution_time"),
            p90_execution_time_us: format_value(90.0, "execution_time"),
            p99_execution_time_us: format_value(99.0, "execution_time"),
        });
    }
    let table = Table::builder(rows).index().column(0).transpose().build();
    println!("\nResults (ms, % change vs Tokio):\n{}", table.to_string());
}
