#!/bin/bash
set -e

# Default to priority if no argument provided
BENCHMARK=${1:-priority}

case "$BENCHMARK" in
    priority)
        echo "Running priority benchmark..."
        cargo bench --bench priority -- --nocapture
        ;;
    tcp)
        echo "Running TCP benchmark..."
        cargo bench --bench tcp -- --nocapture
        ;;
    all)
        echo "Running all benchmarks..."
        cargo bench --bench priority -- --nocapture
        echo ""
        echo "=========================================="
        echo ""
        cargo bench --bench tcp -- --nocapture
        ;;
    *)
        echo "Unknown benchmark: $BENCHMARK"
        echo "Available benchmarks: priority, tcp, all"
        exit 1
        ;;
esac
