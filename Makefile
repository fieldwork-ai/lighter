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
gates: gate-m1 ## Run all landed milestone gates

.PHONY: gate-m1
gate-m1: ## M1: a custom kernel boots to a shell on the serial console
	@scripts/gates/m1-boot.sh

.PHONY: help
help:
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'
