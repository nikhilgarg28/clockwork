# Clockworker Benchmarks

## Running the Benchmarks

```bash
cargo bench --bench overhead
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
- Clockworker with RunnableFifo (simple FIFO, O(1) operations)
- Clockworker with LAS (Least Attained Service, O(log n) operations)

**What to look for**:
- RunnableFifo should be close to Tokio
- LAS will be slower (BTreeMap operations) - but how much?
- If LAS is 10x slower, that's a concern

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
   - LAS uses `BTreeMap` (O(log n)) - consider if simpler scheduling suffices
   - Check `drain_ingress_into_classes` - is channel draining slow?

3. **Reactor starvation (1C)**:
   - Increase `driver_yield` duration
   - Check if `yield_maybe` is being called frequently enough in long-running tasks