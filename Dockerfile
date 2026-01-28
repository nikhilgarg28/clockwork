# Multi-stage build for clockworker benchmarks
FROM rust:1.75-slim-bookworm as builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create a working directory
WORKDIR /clockworker

# Copy the entire project
COPY . .

# Build the project in release mode
RUN cargo build --release --all-targets

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /clockworker

# Copy the built artifacts from builder
COPY --from=builder /clockworker/target/release /clockworker/target/release
COPY --from=builder /clockworker/benches /clockworker/benches
COPY --from=builder /clockworker/Cargo.toml /clockworker/Cargo.toml
COPY --from=builder /clockworker/Cargo.lock /clockworker/Cargo.lock

# Copy monoio-benchmark directory if it exists
COPY --from=builder /clockworker/monoio-benchmark /clockworker/monoio-benchmark

# Create a script directory
COPY docker-entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Default command shows available benchmarks
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["help"]
