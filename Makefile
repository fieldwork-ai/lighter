# lighter — build, sign, and gate targets.
#
# Everything that touches Hypervisor.framework must be code-signed with the
# hypervisor entitlement before it will run, so no target here builds without
# signing afterwards. `make gates` is the milestone ledger: each gate is a
# scripted check, and a milestone is done when its gate passes.

CARGO ?= cargo
SIGN  := scripts/sign.sh
PROFILE ?= debug
ifeq ($(PROFILE),release)
CARGO_PROFILE_FLAG := --release
else
CARGO_PROFILE_FLAG :=
endif
TARGET_DIR := target/$(PROFILE)

.DEFAULT_GOAL := help

# ---------------------------------------------------------------- build ----

.PHONY: build
build: ## Build all crates and sign anything that runs a VM
	$(CARGO) build $(CARGO_PROFILE_FLAG)
	@$(MAKE) --no-print-directory sign

.PHONY: sign
sign: ## Ad-hoc sign built binaries with the hypervisor entitlement
	@bins=$$(find $(TARGET_DIR) -maxdepth 2 \( -path '*/examples/*' -o -path '$(TARGET_DIR)/*' \) \
		-type f -perm -111 ! -name '*.d' ! -name '*.rlib' ! -name '*.dylib' 2>/dev/null \
		| grep -vE '/(build|deps|incremental)/' || true); \
	if [ -n "$$bins" ]; then $(SIGN) $$bins; fi

.PHONY: quick
quick: ## The inner loop: lint, unit tests, and a real boot (~15s)
	@scripts/quick.sh

.PHONY: latency
latency: ## Per-operation latency, macOS against the share (~15s)
	@benchmarks/latency.sh

.PHONY: bench
bench: ## Phase one: the workload cases on a small fixture (~60s)
	@benchmarks/quick.sh

.PHONY: bench-full
bench-full: ## Phase two: the real fixture, for numbers that get published (~20min)
	@./benchmarks/run.sh --target native --reps 3
	@./benchmarks/run.sh --target lighter --reps 3
	@python3 benchmarks/report.py

.PHONY: fmt
fmt: ## Format
	$(CARGO) fmt --all

.PHONY: lint
lint: ## Clippy, warnings are errors
	$(CARGO) clippy --all-targets -- -D warnings

.PHONY: check
check: ## Type-check without building binaries
	$(CARGO) check --all-targets

# ----------------------------------------------------------------- test ----

.PHONY: test
test: ## Unit tests that need no VM
	$(CARGO) test --all

# Tests that create a real VM have to be signed between build and run, which
# cargo cannot do on its own — hence build, sign, then execute by path.
.PHONY: test-hv
test-hv: ## Tests that drive the hypervisor (needs entitlement + real hardware)
	@scripts/run-hv-tests.sh $(CARGO_PROFILE_FLAG)

.PHONY: smoke
smoke: ## Prove the hypervisor path works on this machine
	$(CARGO) build $(CARGO_PROFILE_FLAG) --example smoke -p lighter-hv
	@$(SIGN) $(TARGET_DIR)/examples/smoke
	@$(TARGET_DIR)/examples/smoke

# ---------------------------------------------------------------- gates ----
# One target per milestone. `make gates` runs every gate that has landed.

.PHONY: gates
gates: ## Run all landed milestone gates, and report what each one cost
	@scripts/gates/run-all.sh

.PHONY: gate-m1
gate-m1: ## M1: a custom kernel boots to a shell on the serial console
	@scripts/gates/m1-boot.sh

.PHONY: gate-m2
gate-m2: ## M2: virtio block/entropy/balloon, and a disk that gives space back
	@scripts/gates/m2-devices.sh

.PHONY: gate-m3-network
gate-m3-network: ## M3: the guest is on the network, both directions
	@scripts/gates/m3-network.sh

.PHONY: gate-m3-vsock
gate-m3-vsock: ## M3: a host socket reaches a guest process over vsock
	@scripts/gates/m3-vsock.sh

.PHONY: gate-m3-docker
gate-m3-docker: ## M3: docker and compose on macOS, against our VMM
	@scripts/gates/m3-docker.sh

.PHONY: gate-m4-fs
gate-m4-fs: ## M4: a shared host directory the guest and macOS agree about
	@scripts/gates/m4-fs.sh

.PHONY: gate-m5-speed
gate-m5-speed: ## M5: caching earns its keep, measured against itself
	@scripts/gates/m5-speed.sh


.PHONY: guest
guest: ## Build the guest kernel, agent, initramfs, and root filesystem
	@guest/kernel/build.sh
	@guest/agent/build.sh
	@guest/fstest/build.sh
	@guest/initramfs/build.sh
	@guest/rootfs/build.sh

.PHONY: gate-m6-memory
gate-m6-memory: ## M6: memory tracks load, and idling costs nothing
	@scripts/gates/m6-memory.sh

.PHONY: gate-m7-amd64
gate-m7-amd64: ## M7: x86-64 containers run
	@scripts/gates/m7-amd64.sh

.PHONY: gate-m8-daily
gate-m8-daily: ## M8: a day of work, and a night of sleep
	@scripts/gates/m8-daily.sh

.PHONY: dogfood
dogfood: ## Rebuild the guest using lighter itself, not somebody else's VM
	@scripts/dogfood.sh

.PHONY: dist
dist: ## Build a release tarball (VERSION=x.y.z)
	@scripts/package-release.sh $(VERSION)

.PHONY: help
help:
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'
