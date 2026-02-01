# Single-stage build for clockworker benchmarks
FROM rust:slim-bookworm

# Install build dependencies
RUN apt-get update && apt-get install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create a working directory
WORKDIR /clockworker

# Copy only source files (not target directory or other build artifacts)
COPY Cargo.toml Cargo.lock* README.md ./
COPY src ./src
COPY benches ./benches

# Build all benchmarks in release mode
RUN cargo build --release --bench priority --bench tcp

# Copy and make entrypoint script executable
COPY docker-entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Use entrypoint script to handle benchmark selection
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]

# Default to priority benchmark if no argument provided
CMD ["priority"]
