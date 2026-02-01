#[allow(dead_code)]
mod utils;
use clockworker::ExecutorBuilder;
use futures::future::select;
use futures::FutureExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tabled::Table;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::LocalSet,
    time::{sleep, Duration as TokioDuration},
};

use crate::utils::Metrics;

const PACKET_SIZE: usize = 1024;

async fn handle_stream(mut stream: TcpStream) {
    let mut buf = vec![0; PACKET_SIZE];
    loop {
        match stream.read_exact(&mut buf).await {
            Ok(_) => {}
            Err(_) => return,
        }
        match stream.write_all(&buf).await {
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

async fn serve_clockworker(addr: &str, shutdown: Arc<AtomicBool>) {
    let listener = TcpListener::bind(addr).await.unwrap();
    let executor = ExecutorBuilder::new().with_queue(0, 1).build().unwrap();
    let queue = executor.queue(0).unwrap();

    executor
        .run_until(async move {
            loop {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                let accept_fut = listener.accept();
                let timeout_fut = sleep(TokioDuration::from_millis(100));
                let accept_fut = accept_fut.boxed();
                let timeout_fut = timeout_fut.boxed();

                match select(accept_fut, timeout_fut).await {
                    futures::future::Either::Left((result, _)) => match result {
                        Ok((stream, _addr)) => {
                            queue.spawn(handle_stream(stream));
                        }
                        Err(_) => break,
                    },
                    futures::future::Either::Right((_, _)) => {
                        // Timeout - check shutdown and continue
                        continue;
                    }
                }
            }
        })
        .await;
}

async fn serve_clockworker_monoio(addr: &str, shutdown: Arc<AtomicBool>) {
    use monoio::{
        io::{AsyncReadRentExt, AsyncWriteRentExt},
        net::TcpListener,
    };

    let listener = TcpListener::bind(addr).unwrap();
    let executor = ExecutorBuilder::new().with_queue(0, 1).build().unwrap();
    let queue = executor.queue(0).unwrap();

    executor
        .run_until(async move {
            loop {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                // Use timeout to periodically check shutdown
                let accept_result =
                    monoio::time::timeout(Duration::from_millis(100), listener.accept()).await;

                match accept_result {
                    Ok(result) => match result {
                        Ok((mut stream, _addr)) => {
                            queue.spawn(async move {
                                let mut buf = vec![0; PACKET_SIZE];
                                loop {
                                    let (r, buf_r) = stream.read_exact(buf).await;
                                    if r.is_err() {
                                        return;
                                    }
                                    let (w, buf_w) = stream.write_all(buf_r).await;
                                    if w.is_err() {
                                        return;
                                    }
                                    buf = buf_w;
                                }
                            });
                        }
                        Err(_) => break,
                    },
                    Err(_) => {
                        // Timeout - check shutdown and continue
                        continue;
                    }
                }
            }
        })
        .await;
}

async fn serve_tokio(addr: &str, shutdown: Arc<AtomicBool>) {
    let listener = TcpListener::bind(addr).await.unwrap();
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let accept_fut = listener.accept();
        let timeout_fut = sleep(TokioDuration::from_millis(100));
        let accept_fut = accept_fut.boxed();
        let timeout_fut = timeout_fut.boxed();

        match select(accept_fut, timeout_fut).await {
            futures::future::Either::Left((result, _)) => match result {
                Ok((stream, _addr)) => {
                    tokio::task::spawn_local(handle_stream(stream));
                }
                Err(_) => break,
            },
            futures::future::Either::Right((_, _)) => {
                // Timeout - check shutdown and continue
                continue;
            }
        }
    }
}

async fn serve_monoio(addr: &str, shutdown: Arc<AtomicBool>) {
    use monoio::{
        io::{AsyncReadRentExt, AsyncWriteRentExt},
        net::TcpListener,
    };

    let listener = TcpListener::bind(addr).unwrap();
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Use timeout to periodically check shutdown
        // Monoio futures aren't Send, so we use a different pattern
        let accept_result =
            monoio::time::timeout(Duration::from_millis(100), listener.accept()).await;

        match accept_result {
            Ok(result) => match result {
                Ok((mut stream, _addr)) => {
                    monoio::spawn(async move {
                        let mut buf = vec![0; PACKET_SIZE];
                        loop {
                            let (r, buf_r) = stream.read_exact(buf).await;
                            if r.is_err() {
                                return;
                            }
                            let (w, buf_w) = stream.write_all(buf_r).await;
                            if w.is_err() {
                                return;
                            }
                            buf = buf_w;
                        }
                    });
                }
                Err(_) => break,
            },
            Err(_) => {
                // Timeout - check shutdown and continue
                continue;
            }
        }
    }
}

fn generate_traffic(
    addr: String,
    num_connections: usize,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Metrics> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        rt.block_on(local.run_until(async {
            let mut handles = Vec::new();
            for _ in 0..num_connections {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let addr_clone = addr.clone();
                let shutdown_clone = shutdown.clone();
                handles.push(tokio::task::spawn_local(async move {
                    let mut durations = Vec::new();
                    let mut stream = match TcpStream::connect(&addr_clone).await {
                        Ok(s) => s,
                        Err(_) => return durations,
                    };
                    let mut buf = vec![0; PACKET_SIZE];
                    while !shutdown_clone.load(Ordering::Relaxed) {
                        let start = std::time::Instant::now();
                        if stream.write_all(&buf).await.is_err() {
                            break;
                        }
                        if stream.read_exact(&mut buf).await.is_err() {
                            break;
                        }
                        let duration = std::time::Instant::now().duration_since(start);
                        durations.push(duration);
                    }
                    durations
                }));
            }
            let mut metrics = Metrics::new();
            for handle in handles {
                if let Ok(durations) = handle.await {
                    for duration in durations {
                        metrics.record(duration, &["write"]);
                    }
                }
            }
            metrics
        }))
    })
}

fn run_benchmark(
    addr: &str,
    num_connections: usize,
    spawn_server: impl FnOnce(Arc<AtomicBool>) -> std::thread::JoinHandle<()>,
) -> (f64, Metrics) {
    let shutdown = Arc::new(AtomicBool::new(false));

    // Start server thread
    let server_shutdown = shutdown.clone();
    let server_handle = spawn_server(server_shutdown);

    // Start client thread
    let client_shutdown = shutdown.clone();
    let addr_client = addr.to_string();
    let client_handle = generate_traffic(addr_client, num_connections, client_shutdown);

    // Sleep 5 seconds
    thread::sleep(Duration::from_secs(5));

    // Signal shutdown
    shutdown.store(true, Ordering::Release);

    // Wait for both threads
    server_handle.join().unwrap();
    let metrics = client_handle.join().unwrap();

    // Calculate throughput
    let total_requests = metrics.len();
    let throughput = total_requests as f64 / 5.0;

    (throughput, metrics)
}

fn print_comparison_table(results: &[(&str, f64, Metrics)]) {
    #[derive(tabled::Tabled)]
    struct ComparisonTable {
        runtime: String,
        throughput_req_s: String,
        p50_ms: String,
        p90_ms: String,
        p99_ms: String,
        p99_9_ms: String,
    }

    let mut rows = Vec::new();
    for (runtime, throughput, metrics) in results {
        rows.push(ComparisonTable {
            runtime: runtime.to_string(),
            throughput_req_s: format!("{:.2}", throughput),
            p50_ms: format_duration(metrics.quantile(50.0, "write")),
            p90_ms: format_duration(metrics.quantile(90.0, "write")),
            p99_ms: format_duration(metrics.quantile(99.0, "write")),
            p99_9_ms: format_duration(metrics.quantile(99.9, "write")),
        });
    }

    let table = Table::builder(rows).index().column(0).transpose().build();
    println!("\n=== TCP Benchmark Results ===\n{}", table);
}

fn format_duration(d: Duration) -> String {
    let micros = d.as_micros();
    format!("{:.2}", micros as f64 / 1000.0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Skip bench name if present (args[1] might be "tcp" when run via cargo bench)
    let base_addr = args
        .iter()
        .skip(1)
        .find(|s| s.contains(':'))
        .map(|s| s.as_str())
        .unwrap_or("127.0.0.1");

    let num_connections = 100;
    let mut results = Vec::new();

    // Run Tokio benchmark
    println!("Running Tokio benchmark...");
    {
        let addr = format!("{}:9999", base_addr);
        let addr_clone = addr.clone();
        let (throughput, metrics) = run_benchmark(&addr, num_connections, |shutdown| {
            let addr = addr_clone.clone();
            thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let local = LocalSet::new();
                rt.block_on(local.run_until(serve_tokio(&addr, shutdown)));
            })
        });
        results.push(("Tokio", throughput, metrics));
    }

    // Run Clockworker+Tokio benchmark
    println!("Running Clockworker+Tokio benchmark...");
    {
        let addr = format!("{}:10000", base_addr);
        let addr_clone = addr.clone();
        let (throughput, metrics) = run_benchmark(&addr, num_connections, |shutdown| {
            let addr = addr_clone.clone();
            thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let local = LocalSet::new();
                rt.block_on(local.run_until(serve_clockworker(&addr, shutdown)));
            })
        });
        results.push(("Clockworker+Tokio", throughput, metrics));
    }

    // Run Monoio benchmark
    {
        println!("Running Monoio benchmark...");
        let addr = format!("{}:10001", base_addr);
        let addr_clone = addr.clone();
        let (throughput, metrics) = run_benchmark(&addr, num_connections, |shutdown| {
            let addr = addr_clone.clone();
            thread::spawn(move || {
                #[cfg(target_os = "linux")]
                let mut rt = monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
                    .with_entries(32768)
                    .enable_timer()
                    .build()
                    .unwrap();

                #[cfg(not(target_os = "linux"))]
                let mut rt = monoio::RuntimeBuilder::<monoio::LegacyDriver>::new()
                    .enable_timer()
                    .build()
                    .unwrap();

                rt.block_on(serve_monoio(&addr, shutdown));
            })
        });
        results.push(("Monoio", throughput, metrics));
    }

    // Run Clockworker+Monoio benchmark
    {
        println!("Running Clockworker+Monoio benchmark...");
        let addr = format!("{}:10002", base_addr);
        let addr_clone = addr.clone();
        let (throughput, metrics) = run_benchmark(&addr, num_connections, |shutdown| {
            let addr = addr_clone.clone();
            thread::spawn(move || {
                #[cfg(target_os = "linux")]
                let mut rt = monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
                    .with_entries(32768)
                    .enable_timer()
                    .build()
                    .unwrap();

                #[cfg(not(target_os = "linux"))]
                let mut rt = monoio::RuntimeBuilder::<monoio::LegacyDriver>::new()
                    .enable_timer()
                    .build()
                    .unwrap();

                rt.block_on(serve_clockworker_monoio(&addr, shutdown));
            })
        });
        results.push(("Clockworker+Monoio", throughput, metrics));
    }

    // Print comparison table
    print_comparison_table(&results);
}
