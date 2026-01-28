#!/bin/bash
set -e

# Color output helpers
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

print_header() {
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}$1${NC}"
    echo -e "${BLUE}========================================${NC}"
}

print_success() {
    echo -e "${GREEN}✓ $1${NC}"
}

print_error() {
    echo -e "${RED}✗ $1${NC}"
}

print_warning() {
    echo -e "${YELLOW}⚠ $1${NC}"
}

show_help() {
    cat << EOF
${GREEN}Clockworker Benchmark Runner${NC}

${BLUE}USAGE:${NC}
    docker run [OPTIONS] clockworker-bench <COMMAND>

${BLUE}COMMANDS:${NC}
    help                    Show this help message
    list                    List all available benchmarks
    all                     Run all benchmarks
    priority                Run priority benchmark (Linux-specific)
    overhead                Run overhead benchmark
    tail_latency            Run tail latency benchmark
    poll_profile            Run poll profile benchmark
    monoio                  Run monoio comparison benchmarks (Linux-specific)

${BLUE}EXAMPLES:${NC}
    # Run priority benchmark
    docker run clockworker-bench priority

    # Run all benchmarks
    docker run clockworker-bench all

    # Run with custom cargo flags
    docker run clockworker-bench overhead --features "custom-feature"

    # Interactive shell for manual testing
    docker run -it clockworker-bench /bin/bash

${BLUE}LINUX-SPECIFIC BENCHMARKS:${NC}
    - ${YELLOW}priority${NC}: Tests task prioritization using Linux-specific features
    - ${YELLOW}monoio${NC}: Compares against monoio/glommio (io_uring based, Linux-only)

${BLUE}NOTES:${NC}
    - Priority benchmark uses libc::setpriority which requires Linux
    - Monoio/Glommio benchmarks use io_uring (Linux kernel 5.1+)
    - All benchmarks run in release mode for accurate performance measurement
EOF
}

list_benchmarks() {
    print_header "Available Benchmarks"
    echo ""
    echo "Standard benchmarks:"
    echo "  - overhead"
    echo "  - tail_latency"
    echo "  - poll_profile"
    echo ""
    echo "Linux-specific benchmarks:"
    echo "  - priority (uses libc::setpriority)"
    if [ -d "/clockworker/monoio-benchmark" ]; then
        echo "  - monoio (io_uring based comparisons)"
    else
        echo "  - monoio (not available - directory not found)"
    fi
}

run_benchmark() {
    local bench_name=$1
    shift

    print_header "Running: $bench_name"

    # Check if the benchmark exists
    if [ ! -f "/clockworker/target/release/$bench_name" ]; then
        print_error "Benchmark '$bench_name' not found"
        print_warning "Available benchmarks in /clockworker/target/release:"
        ls -1 /clockworker/target/release/ | grep -v '\.d$' | head -20
        return 1
    fi

    # Run the benchmark
    if /clockworker/target/release/"$bench_name" "$@"; then
        print_success "Completed: $bench_name"
    else
        print_error "Failed: $bench_name"
        return 1
    fi
}

run_monoio_benchmarks() {
    if [ ! -d "/clockworker/monoio-benchmark" ]; then
        print_error "monoio-benchmark directory not found"
        return 1
    fi

    print_header "Running Monoio/Glommio Comparison Benchmarks"

    cd /clockworker/monoio-benchmark

    if [ -f "run_benchmarks.sh" ]; then
        print_warning "Found run_benchmarks.sh, executing..."
        bash run_benchmarks.sh
    else
        print_warning "No run_benchmarks.sh found, building and running manually..."

        # Build all workspace members
        cargo build --release --all

        # Run each server comparison
        print_header "Running benchmark suite"
        # Add your specific monoio benchmark commands here
        print_warning "Please check monoio-benchmark/README.md for specific benchmark commands"
    fi
}

run_all_benchmarks() {
    print_header "Running All Benchmarks"

    local failed=0

    # Standard benchmarks
    for bench in overhead tail_latency poll_profile priority; do
        echo ""
        if ! run_benchmark "$bench"; then
            ((failed++))
        fi
        echo ""
    done

    # Monoio benchmarks if available
    if [ -d "/clockworker/monoio-benchmark" ]; then
        echo ""
        if ! run_monoio_benchmarks; then
            ((failed++))
        fi
    fi

    echo ""
    print_header "Summary"
    if [ $failed -eq 0 ]; then
        print_success "All benchmarks completed successfully!"
    else
        print_error "$failed benchmark(s) failed"
        return 1
    fi
}

# Main command dispatcher
case "$1" in
    help|--help|-h)
        show_help
        ;;
    list)
        list_benchmarks
        ;;
    all)
        run_all_benchmarks
        ;;
    priority|overhead|tail_latency|poll_profile)
        run_benchmark "$@"
        ;;
    monoio)
        run_monoio_benchmarks
        ;;
    /bin/bash|bash|sh)
        exec /bin/bash
        ;;
    *)
        if [ -n "$1" ]; then
            print_error "Unknown command: $1"
            echo ""
        fi
        show_help
        exit 1
        ;;
esac
