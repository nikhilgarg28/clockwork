# Running Clockworker Benchmarks in Docker

This Docker setup allows you to run Linux-specific benchmarks (like `priority` and monoio comparisons) that aren't available or meaningful on macOS.

## Quick Start

### Build the Docker image

```bash
docker build -t clockworker-bench .
```

### Run benchmarks

```bash
# Show available commands
docker run clockworker-bench help

# List all benchmarks
docker run clockworker-bench list

# Run the priority benchmark
docker run clockworker-bench priority

# Run all benchmarks
docker run clockworker-bench all

# Run a specific benchmark
docker run clockworker-bench tail_latency

# Run monoio/glommio comparisons
docker run clockworker-bench monoio
```

## Linux-Specific Benchmarks

### Priority Benchmark
The `priority` benchmark uses `libc::setpriority()` to test OS-level thread priority handling:

```bash
docker run clockworker-bench priority
```

This benchmark:
- Tests clockworker's priority queue implementation
- Compares against vanilla Tokio single-threaded runtime
- Tests two-runtime approach with OS-level priorities (`nice` values)
- Measures foreground task latency and background task throughput

### Monoio/Glommio Comparisons
These runtimes use Linux io_uring (requires kernel 5.1+):

```bash
docker run clockworker-bench monoio
```

## Advanced Usage

### Interactive Shell
Get a shell inside the container for manual testing:

```bash
docker run -it clockworker-bench /bin/bash
```

Inside the container, you can:
```bash
# Navigate to benchmark directory
cd /clockworker

# Run benchmarks manually
./target/release/priority

# Run cargo commands (requires source)
cargo bench --bench priority

# Check system info
uname -a
cat /proc/cpuinfo
```

### Mount Source Code
To rebuild inside the container or modify benchmarks:

```bash
docker run -it -v $(pwd):/clockworker clockworker-bench /bin/bash
```

Then inside the container:
```bash
cargo build --release --bench priority
./target/release/priority
```

### CPU Pinning for Accurate Results
For more accurate benchmark results, you can pin the container to specific CPU cores:

```bash
# Run on cores 0-3
docker run --cpuset-cpus="0-3" clockworker-bench priority

# Run on a single core for single-threaded benchmark accuracy
docker run --cpuset-cpus="0" clockworker-bench priority
```

### Resource Limits
Control memory and CPU resources:

```bash
# Limit to 4GB RAM and 2 CPUs
docker run --memory="4g" --cpus="2" clockworker-bench all
```

## Architecture

The Docker setup uses a multi-stage build:

1. **Builder stage**: Compiles all benchmarks in release mode
2. **Runtime stage**: Minimal Debian image with only the compiled binaries

This keeps the final image size small while ensuring benchmarks are optimized.

## Benchmark Output

All benchmarks print results to stdout. You can capture them:

```bash
# Save results to file
docker run clockworker-bench priority > results.txt

# Run multiple times and compare
for i in {1..5}; do
  echo "Run $i:"
  docker run clockworker-bench priority
done
```

## Troubleshooting

### Build fails with dependency errors
Make sure you have the latest Rust toolchain:
```bash
docker build --no-cache -t clockworker-bench .
```

### Benchmark binary not found
The entrypoint script looks for binaries in `/clockworker/target/release/`. If a benchmark fails to build, it won't be available. Check the build logs:
```bash
docker build -t clockworker-bench . 2>&1 | tee build.log
```

### Performance seems off
- Try pinning to specific CPUs with `--cpuset-cpus`
- Ensure your Docker daemon has adequate resources allocated
- Run on a Linux host rather than Docker Desktop (which uses a VM on macOS/Windows)

### io_uring not available
The monoio/glommio benchmarks require Linux kernel 5.1+:
```bash
docker run clockworker-bench /bin/bash -c "uname -r"
```

If your kernel is too old, consider using a newer base image or running on a different system.

## Comparing with Host System

If you want to compare Docker performance with host performance (on Linux):

```bash
# On host (Linux)
cargo build --release
./target/release/priority > host_results.txt

# In Docker
docker run clockworker-bench priority > docker_results.txt

# Compare
diff -y host_results.txt docker_results.txt
```

Note: Docker will have some overhead, but it should be minimal for CPU-bound benchmarks.

## CI/CD Integration

Example GitHub Actions workflow:

```yaml
name: Benchmark

on: [push, pull_request]

jobs:
  benchmark:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3

      - name: Build benchmark container
        run: docker build -t clockworker-bench .

      - name: Run priority benchmark
        run: docker run clockworker-bench priority

      - name: Run all benchmarks
        run: docker run clockworker-bench all
```

## Design Rationale

### Why Docker for benchmarks?

1. **Linux-specific features**: Priority benchmark uses `libc::setpriority`, monoio/glommio use io_uring
2. **Reproducible environment**: Same kernel, same libraries, same toolchain
3. **Easy CI/CD**: Run benchmarks in GitHub Actions, GitLab CI, etc.
4. **Isolation**: No interference from host system processes

### Interface Design

The entrypoint script uses a command-based interface because:

1. **Discoverability**: `docker run clockworker-bench help` shows all options
2. **Simplicity**: Single command to run a benchmark
3. **Flexibility**: Easy to add new benchmark suites
4. **Standard pattern**: Familiar to users of other benchmark tools

Alternative approaches considered:
- Environment variables: Less discoverable, harder to document
- Multiple images: More complex to maintain
- JSON config file: Overkill for simple use cases

The current approach balances simplicity with flexibility.
