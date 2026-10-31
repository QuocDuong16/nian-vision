.DEFAULT_GOAL := help

CARGO ?= cargo
MISE ?= mise
PNPM ?= pnpm
RUSTUP ?= rustup
TOOLCHAIN_PINS ?= ./scripts/toolchain-pins.sh
WINDOWS_TARGET ?= x86_64-pc-windows-msvc

.PHONY: help clean toolchain-install toolchain-check deps-install \
	format format-check rust-check clippy windows-clippy rust-test rust-build rust-machete coverage \
	ui-lint ui-typecheck ui-test ui-build ui-audit ui-outdated \
	rust-audit deny-check rust-outdated audit outdated build test \
	release-test release-version media-gate scripts-test quality-check check

##@ Getting started
help: ## Show the documented Make targets
	@awk 'BEGIN { FS = ":.*##"; printf "Usage: make <target> [VAR=value...]\n" } /^##@/ { if (shown++) printf "\n"; printf "%s\n", substr($$0, 5); next } /^[a-zA-Z0-9_.-]+:.*##/ { printf "  \033[36m%-32s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

##@ Toolchain
toolchain-install: ## Install the repository-pinned Rust, Node, pnpm, and Cargo tools
	$(MISE) install --locked rust node pnpm \
		cargo:cargo-audit cargo:cargo-deny cargo:cargo-llvm-cov cargo:cargo-machete

toolchain-check: ## Verify active Rust, Node, pnpm, and Cargo tool versions against the mise pins
	@set -eu; \
	expected_rust="$$($(TOOLCHAIN_PINS) rust)"; \
	expected_node="$$($(TOOLCHAIN_PINS) node)"; \
	expected_pnpm="$$($(TOOLCHAIN_PINS) pnpm)"; \
	rust_version="$$($(MISE) exec -- rustc --version)"; \
	node_version="$$($(MISE) exec -- node --version)"; \
	pnpm_version="$$($(MISE) exec -- pnpm --version)"; \
	case "$$rust_version" in \
		"rustc $$expected_rust "*) ;; \
		*) echo "Expected Rust $$expected_rust from mise.toml, got: $$rust_version" >&2; exit 1 ;; \
	esac; \
	test "$$node_version" = "v$$expected_node" || { echo "Expected Node v$$expected_node from mise.toml, got: $$node_version" >&2; exit 1; }; \
	test "$$pnpm_version" = "$$expected_pnpm" || { echo "Expected pnpm $$expected_pnpm from mise.toml, got: $$pnpm_version" >&2; exit 1; }; \
	for tool in cargo-audit cargo-deny cargo-llvm-cov cargo-machete; do \
		expected="$$($(TOOLCHAIN_PINS) $$tool)"; \
		actual="$$($(MISE) exec -- cargo $${tool#cargo-} --version 2>&1 | head -n 1)"; \
		case "$$actual" in \
			*"$$expected"*) ;; \
			*) echo "Expected $$tool $$expected from mise.toml, got: $$actual" >&2; exit 1 ;; \
		esac; \
	done; \
	grep -qx "channel = \"$$expected_rust\"" rust-toolchain.toml || { echo "rust-toolchain.toml does not match the mise.toml Rust pin" >&2; exit 1; }; \
	grep -qx "rust-version = \"$$expected_rust\"" Cargo.toml || { echo "Cargo.toml rust-version does not match the mise.toml Rust pin" >&2; exit 1; }; \
	printf '%s; Node %s; pnpm %s\n' "$$rust_version" "$$node_version" "$$pnpm_version"

deps-install: ## Install frontend dependencies from the committed lockfile
	$(MISE) exec -- $(PNPM) install --frozen-lockfile

##@ Rust workspace
format: ## Format Rust workspace sources
	$(CARGO) fmt --all

format-check: ## Check Rust formatting without changing files
	$(CARGO) fmt --all --check

rust-check: ## Check every Rust workspace member
	$(CARGO) check --workspace

clippy: ## Run the CI Clippy gate with warnings denied
	$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings

windows-clippy: ## Cross-check the Windows-only crate used by CI
	$(RUSTUP) target add $(WINDOWS_TARGET)
	$(CARGO) clippy -p nian-platform-windows --target $(WINDOWS_TARGET) --all-targets -- -D warnings

rust-test: ## Run the Rust workspace tests
	$(CARGO) test --workspace

rust-build: ## Build the default Rust workspace members
	$(CARGO) build

rust-machete: ## Detect unused Rust workspace dependencies
	$(MISE) exec -- cargo machete

coverage: ## Generate an HTML coverage report for the Rust workspace
	$(MISE) exec -- cargo llvm-cov --workspace --all-features --html

##@ React UI
ui-lint: ## Lint the React UI
	$(MISE) exec -- $(PNPM) lint

ui-typecheck: ## Typecheck the React UI
	$(MISE) exec -- $(PNPM) typecheck

ui-test: ## Run the React UI tests
	$(MISE) exec -- $(PNPM) test

ui-build: ## Build the React UI
	$(MISE) exec -- $(PNPM) build

##@ Dependency maintenance
rust-audit: ## Check Cargo.lock against the RustSec advisory database
	$(MISE) exec -- cargo audit

deny-check: ## Check dependency licenses, advisories, sources, and bans
	$(MISE) exec -- cargo deny check --hide-inclusion-graph

ui-audit: ## Audit production pnpm dependencies
	$(MISE) exec -- $(PNPM) audit --prod

audit: ## Audit RustSec advisories and production UI dependencies
	$(MAKE) rust-audit
	$(MAKE) ui-audit

rust-outdated: ## List newer direct dependencies across Rust workspace members
	CARGO="$(CARGO)" $(MISE) exec -- node scripts/run_cargo_outdated.mjs

ui-outdated: ## List newer pnpm workspace dependencies
	PNPM="$(PNPM)" $(MISE) exec -- node scripts/run_pnpm_outdated.mjs

outdated: ## List newer direct dependencies in Rust and pnpm workspaces
	$(MAKE) rust-outdated
	$(MAKE) ui-outdated

##@ Build and release
build: ## Build the default Rust workspace members and React UI
	$(MAKE) rust-build
	$(MAKE) ui-build

test: ## Run Rust and UI test suites
	$(MAKE) rust-test
	$(MAKE) ui-test

release-test: ## Run release-script contract tests
	$(PNPM) release:test

release-version: ## Check synchronized application and release versions
	$(PNPM) release:version

media-gate: ## Validate the packaged media runtime contract
	$(PNPM) media:gate

scripts-test: ## Run the tooling-script unit tests
	$(MISE) exec -- $(PNPM) scripts:test

##@ Quality
quality-check: ## Run the Rust and UI quality gates used by Forgejo CI
	$(MAKE) toolchain-check
	$(MAKE) scripts-test
	$(MAKE) format-check
	$(MAKE) rust-check
	$(MAKE) windows-clippy
	$(MAKE) clippy
	$(MAKE) rust-test
	$(MAKE) deny-check
	$(MAKE) ui-lint
	$(MAKE) ui-typecheck
	$(MAKE) ui-test
	$(MAKE) ui-build

check: quality-check ## Alias for quality-check

##@ Cleanup
clean: ## Remove Cargo and React UI build outputs
	$(CARGO) clean
	rm -rf ui/dist
