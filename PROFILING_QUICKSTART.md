# Profiling Quickstart

## TL;DR - Run These Commands

```bash
# 1. Run the overhead benchmark
cargo bench --bench overhead_profile

# 2. Profile with samply (opens interactive flamegraph in browser)
samply record target/release/deps/overhead_profile-*

# 3. Or save profile for later
samply record --save-only --output profile.json target/release/deps/overhead_profile-*
samply load profile.json  # View later
```

## What You'll See

### Benchmark Output
```
Tokio baseline:          36.4ms
CW+RunnableFifo:         43.7ms (+9.6%)   ← ~10% overhead in microbench
CW+LAS:                  150.7ms (+266%)  ← LAS is expensive for short tasks

Per-Task Overhead:
RunnableFifo: 146ns/task
LAS:          2,287ns/task
```

### In the Flamegraph

**Look for these functions (clockworker overhead):**

1. **`clockworker::executor::Executor::run`** (21% of overhead)
   - EEVDF queue selection
   - Main executor loop

2. **`clockworker::scheduler::RunnableFifo::pop`** (24% of overhead)
   - VecDeque operations
   - Getting next task

3. **`clockworker::task::spawn`** (27% of overhead)
   - Task allocation
   - Join handle creation

4. **`clockworker::task::waker::wake`** (17% of overhead)
   - Waker notifications
   - Runqueue updates

5. **Task state management** (11% of overhead)
   - Atomic operations
   - Reference counting

## Key Insights

### Why Microbenchmark Shows 10% but Real-World Shows 2-5%?

**It's all about task duration!**

| Task Type | Duration | Overhead | Impact |
|-----------|----------|----------|--------|
| Microbench (5 yields) | ~730ns | 146ns | 20% |
| Real TCP ping-pong | ~8,772ns | 146ns | **1.7%** |
| Typical web request | ~1ms | 146ns | **0.01%** |

**The overhead is FIXED, not proportional!**

### When to Worry About Overhead

✅ **Don't worry if:**
- Tasks do real work (I/O, computation)
- Task duration > 10μs
- You need prioritization or fairness

❌ **Do worry if:**
- Tasks are < 1μs (extremely short)
- Pure CPU-bound microtasks
- Every % of throughput is critical

## Quick Profiling Recipes

### Recipe 1: Compare Tokio vs Clockworker

```bash
# Profile just the tokio baseline
samply record --save-only --output tokio.json \
  target/release/deps/overhead_profile-* tokio

# Profile clockworker
samply record --save-only --output clockworker.json \
  target/release/deps/overhead_profile-* clockworker

# Load both and compare
samply load tokio.json
samply load clockworker.json
```

### Recipe 2: Find Hot Paths

```bash
# Generate flamegraph
samply record target/release/deps/overhead_profile-*

# In the browser flamegraph:
# 1. Search for "clockworker" - see all overhead
# 2. Search for "scheduler" - see scheduler overhead
# 3. Search for "wake" - see waker overhead
# 4. Click on functions to zoom in
```

### Recipe 3: Profile Your Own Application

```bash
# Build with release + debug symbols
cargo build --release

# Profile
samply record your-binary --your-args

# Look for these clockworker functions in flamegraph:
# - Executor::run
# - Scheduler::push/pop
# - Task::spawn
# - wake/wake_by_ref
```

### Recipe 4: Measure Just Spawn Overhead

Add to your code:

```rust
let start = std::time::Instant::now();
for _ in 0..100_000 {
    queue.spawn(async {});  // Empty task
}
let elapsed = start.elapsed();
println!("Spawn overhead: {}ns/task", elapsed.as_nanos() / 100_000);
```

Expected: ~40-50ns/spawn

### Recipe 5: Measure Just Poll Overhead

```rust
// Task that yields immediately
let start = std::time::Instant::now();
let handles: Vec<_> = (0..100_000)
    .map(|_| queue.spawn(async {
        tokio::task::yield_now().await;
    }))
    .collect();

for h in handles {
    h.await.unwrap();
}
let elapsed = start.elapsed();
println!("Poll overhead: {}ns/task", elapsed.as_nanos() / 100_000);
```

Expected: ~100-150ns/poll (includes spawn)

## Understanding the Numbers

### What's Normal?

| Metric | RunnableFifo | LAS | Notes |
|--------|--------------|-----|-------|
| Spawn overhead | 40ns | 50ns | Allocation + setup |
| Poll overhead | 60ns | 150ns | Scheduler + waker |
| Wake overhead | 30ns | 40ns | Notification |
| **Total** | **130-150ns** | **240ns** | Per task lifecycle |

### What's Problematic?

🚨 **If you see:**
- Spawn > 100ns: Possible memory allocation issue
- Poll > 200ns (RunnableFifo): Possible contention
- LAS > 500ns: Queue is very large (O(log n) operations)

## Next Steps After Profiling

1. **If overhead < 5% in your workload:** You're done! Don't optimize.

2. **If overhead 5-10%:** Consider:
   - Batching work within tasks
   - Using RunnableFifo instead of LAS
   - See optimization opportunities in OVERHEAD_ANALYSIS.md

3. **If overhead > 10%:** Your tasks might be too short:
   - Combine multiple operations per task
   - Profile to find unexpected hot paths
   - Consider if you really need clockworker

## Files to Read

1. **PROFILING.md** - Comprehensive profiling guide
2. **OVERHEAD_ANALYSIS.md** - Detailed overhead breakdown
3. **This file** - Quick commands and recipes

## Common Questions

**Q: My overhead is higher than 2-5%, why?**
A: Probably very short tasks. Check task duration vs overhead (see table above).

**Q: Should I optimize clockworker?**
A: Only if overhead > 5% AND you've exhausted application-level optimizations.

**Q: LAS seems expensive, should I avoid it?**
A: For short tasks (<10μs), yes. For normal tasks, the tail latency benefits are worth it.

**Q: Can I get to 0% overhead?**
A: No - any scheduling layer has overhead. But you can minimize it (see OVERHEAD_ANALYSIS.md).

## Quick Profile Viewing

```bash
# If you saved a profile earlier:
samply load /tmp/clockworker_profile.json

# This opens an interactive flamegraph where you can:
# - Zoom into functions
# - Search for specific symbols
# - See exact time percentages
# - Compare different runs
```

---

**Remember:** The 2-5% overhead is not a bug, it's the cost of having sophisticated scheduling! The benefits (prioritization, fairness, tail latency) far outweigh this cost for most workloads.
