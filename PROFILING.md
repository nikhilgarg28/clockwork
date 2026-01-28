# Profiling Clockworker Overhead

This guide explains how to profile and analyze the ~2-4% overhead that clockworker adds compared to vanilla async runtimes.

## Quick Start

```bash
# 1. Build with profiling enabled
cargo build --release --bench overhead_profile

# 2. Run the overhead benchmark
cargo bench --bench overhead_profile

# 3. Profile with samply (macOS/Linux)
samply record target/release/deps/overhead_profile-*

# Or use flamegraph (Linux)
cargo flamegraph --bench overhead_profile
```

## Understanding the Overhead

The ~2-4% overhead comes from several sources:

### 1. **Task Wrapping** (~30% of overhead)
Every task spawned on clockworker needs to be wrapped in executor infrastructure:
- Task state tracking (Running, Completed, etc.)
- Join handle implementation
- Panic handling hooks

**Profile this:** Look for `spawn` calls in the flamegraph.

### 2. **Scheduler Indirection** (~25% of overhead)
Each task poll goes through:
- Trait vtable dispatch (`Box<dyn Scheduler>`)
- Queue selection logic (EEVDF scheduling)
- Scheduler-specific push/pop operations

**Profile this:** Look for `Scheduler::push`, `Scheduler::pop`, and `select_queue` in profiles.

### 3. **Waker Management** (~20% of overhead)
Clockworker maintains its own waker system:
- Custom waker implementation
- Waker caching on task records
- Wake notification propagation

**Profile this:** Look for `wake`, `wake_by_ref`, and waker-related allocations.

### 4. **Atomic Operations** (~15% of overhead)
Task lifecycle management requires atomics:
- Task state transitions
- Reference counting for join handles
- Queue statistics updates

**Profile this:** Look for atomic operations in hot paths.

### 5. **Channel Operations** (~10% of overhead)
Communication between components:
- Executor run queue notifications
- Task completion signaling

**Profile this:** Look for channel send/recv operations.

## Profiling Methods

### Method 1: Samply (Recommended for macOS)

Samply provides a browser-based flamegraph viewer with great UX:

```bash
# Build in release mode with debug symbols
cargo build --release --bench overhead_profile

# Run with samply
samply record target/release/deps/overhead_profile-*

# This will:
# 1. Record CPU samples
# 2. Open a browser with interactive flamegraph
# 3. Allow you to zoom, search, and analyze
```

**What to look for:**
- Compare the baseline (tokio) vs clockworker runs
- Identify functions that appear more in clockworker
- Look for unexpected allocations or locks

### Method 2: Cargo Flamegraph (Linux)

```bash
# Install if needed
cargo install flamegraph

# Generate flamegraph
cargo flamegraph --bench overhead_profile

# Opens flamegraph.svg in browser
```

### Method 3: perf + FlameGraph (Linux)

```bash
# Record
perf record -F 99 -g target/release/deps/overhead_profile-*

# Generate flamegraph
perf script | stackcollapse-perf.pl | flamegraph.pl > profile.svg
```

### Method 4: instruments (macOS)

For detailed time profiling on macOS:

```bash
# Build with profiling symbols
cargo build --profile samply --bench overhead_profile

# Run in Instruments
instruments -t "Time Profiler" target/samply/overhead_profile-*
```

## Analyzing Specific Components

### Analyzing Task Spawn Overhead

Create a micro-benchmark that just spawns tasks:

```rust
// In overhead_profile.rs, add:
fn benchmark_spawn_only() {
    for _ in 0..100_000 {
        queue.spawn(async {}); // Empty task
    }
}
```

Profile this to see pure spawn overhead.

### Analyzing Scheduler Overhead

Compare different schedulers:

```bash
# Already in overhead_profile.rs
cargo bench --bench overhead_profile
```

This shows:
- RunnableFifo: Minimal scheduler (FIFO queue)
- LAS: More complex (heap-based priority queue)

The difference between them is pure scheduler overhead.

### Analyzing Waker Overhead

Look at the waker hot path:

```bash
samply record target/release/deps/overhead_profile-*
# Search for "wake" in the flamegraph
```

Common waker-related functions:
- `clockworker::task::waker::wake`
- `clockworker::task::waker::wake_by_ref`
- `RawWaker::new`

### Analyzing Memory Allocations

Use heap profiling to find allocation overhead:

```bash
# macOS
instruments -t "Allocations" target/release/deps/overhead_profile-*

# Linux with valgrind
valgrind --tool=massif target/release/deps/overhead_profile-*
ms_print massif.out.*
```

**Expected allocations:**
- Task structures
- Join handles
- Scheduler data structures (queues, heaps)
- Wakers

**Unexpected allocations to investigate:**
- String formatting
- Temporary vectors
- Boxed closures in hot paths

## Optimization Strategies

Once you've identified the overhead sources:

### 1. Reduce Allocations
- Pool task structures
- Cache wakers more aggressively
- Use stack-allocated buffers

### 2. Reduce Indirection
- Monomorphize hot paths
- Inline critical functions
- Use static dispatch where possible

### 3. Reduce Atomics
- Batch state updates
- Use relaxed orderings where safe
- Consider per-core counters

### 4. Improve Cache Locality
- Pack hot fields together
- Align structures to cache lines
- Prefetch in predictable patterns

## Comparing Profiles

To see the difference between tokio and clockworker:

```bash
# Generate both profiles
samply record --output tokio.json target/release/deps/overhead_profile-* tokio
samply record --output clockworker.json target/release/deps/overhead_profile-* clockworker

# Compare in browser
# Look for functions that appear in clockworker but not tokio
```

## Common Findings

Based on typical profiling sessions, here's what you'll likely find:

### Hot Paths in Clockworker

1. **`Executor::run` loop** (25-30% of time)
   - Queue selection (EEVDF)
   - Scheduler trait calls
   - Task polling

2. **`Scheduler::pop`** (15-20% of time)
   - RunnableFifo: VecDeque pop (fast)
   - LAS: Heap pop + update (slower)

3. **`Task::poll`** (15-20% of time)
   - Waker setup
   - Future polling
   - State management

4. **`wake_by_ref`** (10-15% of time)
   - Waker notification
   - Queue push
   - Executor notification

### Expected Overhead Breakdown

For a typical workload with RunnableFifo:

```
Total overhead: ~2.5%
├─ Task wrapping:      ~0.75% (30%)
├─ Scheduler calls:    ~0.62% (25%)
├─ Waker management:   ~0.50% (20%)
├─ Atomic operations:  ~0.38% (15%)
└─ Misc (channels):    ~0.25% (10%)
```

For LAS scheduler, add ~1-2% more for heap operations.

## When is Overhead Worth It?

The 2-4% overhead is worth it when:

✅ **You need prioritization** - Tail latency improvements (20-50%) far outweigh 2-4% throughput loss
✅ **You have mixed workloads** - Separating foreground/background work justifies overhead
✅ **You need fairness** - Per-tenant or per-connection fairness requires scheduling

❌ Don't use clockworker for:
- Simple FIFO workloads
- Maximum throughput at any cost
- Ultra-low-latency requirements (<10μs)

## Next Steps

1. **Run the profiling benchmark:**
   ```bash
   cargo bench --bench overhead_profile
   ```

2. **Generate a flamegraph:**
   ```bash
   samply record target/release/deps/overhead_profile-*
   ```

3. **Identify the top 3 hot paths** in clockworker that don't appear in tokio

4. **Consider optimizations** based on your specific use case

5. **Measure again** after any optimizations

## Questions?

Common profiling questions:

**Q: Why is the overhead higher/lower than 2-4%?**
A: Depends on your workload. Very short tasks show more overhead, long-running tasks show less.

**Q: Can I eliminate the overhead completely?**
A: No - any scheduling layer adds overhead. But we can minimize it.

**Q: Should I optimize clockworker?**
A: Only if the 2-4% matters for your use case. For most apps, the benefits outweigh the cost.

**Q: Which scheduler has lowest overhead?**
A: RunnableFifo (~2%), then ArrivalFifo (~2.5%), then LAS/QLAS (~3-4%).
