.PHONY: help docker-build docker-run-priority docker-run-all docker-run-monoio docker-shell docker-clean

# Docker image name
IMAGE_NAME := clockworker-bench

help: ## Show this help message
	@echo 'Clockworker Benchmark Makefile'
	@echo ''
	@echo 'Usage:'
	@echo '  make <target>'
	@echo ''
	@echo 'Targets:'
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "  %-20s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

docker-build: ## Build the Docker image for running benchmarks
	docker build -t $(IMAGE_NAME) .

docker-run-priority: ## Run the priority benchmark in Docker
	docker run --rm $(IMAGE_NAME) priority

docker-run-all: ## Run all benchmarks in Docker
	docker run --rm $(IMAGE_NAME) all

docker-run-monoio: ## Run monoio/glommio comparison benchmarks
	docker run --rm $(IMAGE_NAME) monoio

docker-run-overhead: ## Run overhead benchmark
	docker run --rm $(IMAGE_NAME) overhead

docker-run-tail-latency: ## Run tail latency benchmark
	docker run --rm $(IMAGE_NAME) tail_latency

docker-run-poll-profile: ## Run poll profile benchmark
	docker run --rm $(IMAGE_NAME) poll_profile

docker-list: ## List available benchmarks
	docker run --rm $(IMAGE_NAME) list

docker-shell: ## Open an interactive shell in the Docker container
	docker run --rm -it $(IMAGE_NAME) /bin/bash

docker-shell-mount: ## Open shell with source code mounted (for rebuilding)
	docker run --rm -it -v $(PWD):/clockworker $(IMAGE_NAME) /bin/bash

docker-run-pinned: ## Run priority benchmark pinned to single CPU core
	docker run --rm --cpuset-cpus="0" $(IMAGE_NAME) priority

docker-clean: ## Remove the Docker image
	docker rmi $(IMAGE_NAME)

docker-rebuild: docker-clean docker-build ## Clean and rebuild the Docker image

# Convenience targets for local development (non-Docker)
bench-priority: ## Run priority benchmark locally (requires Linux)
	cargo build --release --bench priority
	./target/release/priority

bench-all: ## Run all benchmarks locally
	cargo bench

bench-overhead: ## Run overhead benchmark locally
	cargo bench --bench overhead

bench-tail-latency: ## Run tail latency benchmark locally
	cargo bench --bench tail_latency
