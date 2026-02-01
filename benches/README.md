# Clockworker Benchmarks

## ExecutorBuilder API

The executor builder provides a simple method for adding queues:

- `with_queue(qid, share)` - Creates a queue with FIFO task scheduling

## Running the Benchmarks

```bash
cargo bench --bench overhead
cargo bench --bench tail_latency
cargo bench --bench poll_profile
```

Results are printed to stdout and also saved to `benchmark_results.csv` for further analysis.

## Benchmark Descriptions

### 1A: Spawn Throughput

**Goal**: Measure the cost of task creation and minimal scheduling.

**Setup**:
- Spawn N tasks (1K, 10K, 100K)
- Each task increments an atomic counter and completes immediately
- Measure wall-clock time for all tasks to complete

**Metric**: Tasks/second

**Comparison**: Raw `tokio::task::spawn_local`

**What to look for**:
- Overhead <10% is excellent
- Overhead 10-30% is acceptable
- Overhead >50% warrants investigation

### 1B: Yield/Poll Overhead

**Goal**: Measure the scheduler's pick-next-task cost.

**Setup**:
- Spawn N=1000 tasks
- Each task yields K times (10, 100, 1000) before completing
- This stresses the scheduler's polling logic

**Metric**: Polls/second

**Comparison**: 
- Tokio (baseline)
- Clockworker with FIFO scheduling (O(1) operations)

**What to look for**:
- FIFO scheduling should be close to Tokio

### 1C: IO Reactor Integration

**Goal**: Verify Clockworker doesn't starve the underlying runtime's IO reactor.

**Setup**:
- N=100 tasks
- Each task sleeps 10 times for 500μs
- Measure actual sleep durations

**Metrics**:
- Total completion time
- Sleep accuracy (actual vs requested duration)

**What to look for**:
- Sleep accuracy ratio should be close to Tokio's (within 1.5x)
- If sleeps take 2-3x longer than requested, Clockworker isn't yielding to the reactor enough
- The `driver_yield` configuration might need tuning

## CSV Output Format

```
benchmark,variant,parameter,iteration,duration_ns
1A_spawn,clockworker,1000,0,12345678
1A_spawn,tokio,1000,0,11234567
...
```

## Analyzing Results

To generate charts from the CSV:

```python
import pandas as pd
import matplotlib.pyplot as plt

df = pd.read_csv('benchmark_results.csv')

# Example: Plot 1A throughput
df_1a = df[df['benchmark'] == '1A_spawn']
# ... your analysis here
```

## Tuning Based on Results

If you find high overhead:

1. **High spawn overhead (1A)**: 
   - Check `Arc`/allocation costs in `TaskHeader`
   - Consider pre-allocation or arena allocation

2. **High poll overhead (1B)**:
   - Check if MPSC draining is slow
   - Profile using `cargo flamegraph --bench poll_profile`

3. **Reactor starvation (1C)**:
   - Increase `driver_yield` duration
   - Decrease `max_polls_per_yield` to yield more frequently
   - Check if `yield_maybe` is being called frequently enough in long-running tasks

## Poll Profile Benchmark

**Goal**: Profile the executor's poll path to identify performance bottlenecks.

**Setup**:
- Spawn N tasks (default: 10,000)
- Each task yields K times (default: 100)
- Minimal work - focuses on poll overhead

**Usage**:

```bash
# Run normally (just timing)
cargo bench --bench poll_profile

# Run with flamegraph (requires cargo-flamegraph: cargo install flamegraph)
cargo flamegraph --bench poll_profile

# Run with pprof (generates flamegraph.svg and profile.pb)
# Note: On macOS, this may require running with sudo or may fail with SIGTRAP
# If it fails, use cargo-flamegraph instead (recommended)
PROFILE=pprof cargo bench --bench poll_profile
```

**Configuration via environment variables**:
- `N_TASKS`: Number of tasks to spawn (default: 10000)
- `YIELDS_PER_TASK`: Number of yields per task (default: 100)
- `EXECUTOR`: "clockworker" or "tokio" (default: "clockworker")
- `PROFILE`: Set to "pprof" to enable pprof profiling

**Examples**:

```bash
# Profile Clockworker
cargo bench --bench poll_profile

# Profile with more tasks/yields
N_TASKS=50000 YIELDS_PER_TASK=200 cargo bench --bench poll_profile

# Compare with Tokio
EXECUTOR=tokio cargo bench --bench poll_profile

# Generate pprof flamegraph
PROFILE=pprof cargo bench --bench poll_profile
```

**Interpreting Results**:

1. **Flamegraph**: Shows call stack and time spent in each function
   - Look for hot paths in `poll_task`, `pop_next_task_from_queue`, MPSC operations
   - Identify RefCell borrow overhead, stats recording, `Instant::now()` calls, etc.
   - Open `flamegraph.svg` in a web browser to explore

2. **pprof**: Use `go tool pprof` or `pprof` command to analyze
   ```bash
   go tool pprof profile.pb
   # Or with web UI:
   pprof -http=:8080 profile.pb
   ```
   **Note**: On macOS, pprof may fail with SIGTRAP due to system-level profiling restrictions.
   If this happens, use `cargo flamegraph` instead (recommended for macOS).

3. **Timing output**: Shows total time, polls/sec, and avg time per poll
   - Compare with Tokio baseline to see overhead
   - Use to validate optimizations

**Troubleshooting pprof on macOS**:

If you get `SIGTRAP` errors when using pprof, pprof requires system-level profiling permissions. Options:

1. **Run with sudo** (enables signal-based profiling):
   ```bash
   sudo PROFILE=pprof cargo bench --bench poll_profile
   ```
   Note: This requires root access and may prompt for your password.

2. **Use cargo-flamegraph** (recommended, no sudo needed):
   ```bash
   cargo flamegraph --bench poll_profile
   ```
   This uses `perf` on Linux or `dtrace` on macOS with proper permissions.

3. **Use Instruments** (macOS native profiler):
   ```bash
   xcrun xctrace record --template 'Time Profiler' \
     --launch -- cargo bench --bench poll_profile
   ```
   Then open the trace in Instruments.app.

4. **Run in Linux container/VM**: pprof works reliably on Linux without special permissions.

The benchmark will gracefully handle pprof failures and suggest these alternatives.
